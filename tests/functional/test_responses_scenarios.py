"""Responses API 的并行工具分片、回执和会话恢复。"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from wire import first_event, rollout_files, rollout_thread_id


def test_Responses并行分片工具调用回执并恢复(sandbox, serve_session) -> None:
    requests: list[dict] = []
    paths: list[str] = []
    continue_stream = threading.Event()

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            paths.append(self.path)
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            if len(requests) == 1:
                frames = [
                    ("response.created", {"response": {"id": "resp-1", "model": "gpt-4o"}}),
                    ("response.output_item.added", {"output_index": 0, "item": {
                        "type": "function_call", "id": "fc-a", "call_id": "call-a",
                        "name": "scenario_lookup", "arguments": ""}}),
                    ("response.function_call_arguments.delta", {"output_index": 0, "item_id": "fc-a",
                                                                 "delta": '{"key":'}),
                    ("response.output_item.added", {"output_index": 1, "item": {
                        "type": "function_call", "id": "fc-b", "call_id": "call-b",
                        "name": "scenario_lookup", "arguments": ""}}),
                    ("response.function_call_arguments.delta", {"output_index": 1, "item_id": "fc-b",
                                                                 "delta": '{"key":'}),
                    ("response.function_call_arguments.delta", {"output_index": 0, "item_id": "fc-a",
                                                                 "delta": '"alpha"}'}),
                    ("response.function_call_arguments.delta", {"output_index": 1, "item_id": "fc-b",
                                                                 "delta": '"beta"}'}),
                    ("response.function_call_arguments.done", {"output_index": 0, "item_id": "fc-a",
                                                                "arguments": '{"key":"alpha"}'}),
                    ("response.function_call_arguments.done", {"output_index": 1, "item_id": "fc-b",
                                                                "arguments": '{"key":"beta"}'}),
                    ("response.completed", {"response": {"id": "resp-1", "usage": {
                        "input_tokens": 20, "output_tokens": 8}}}),
                ]
            else:
                frames = [
                    ("response.output_text.delta", {"delta": "合并完成" if len(requests) == 2 else "恢复完成"}),
                    ("response.completed", {"response": {"id": "resp-next", "usage": {
                        "input_tokens": 23, "output_tokens": 4}}}),
                ]
            encoded = [
                ("event: " + event + "\ndata: " + json.dumps(data, ensure_ascii=False) + "\n\n").encode()
                for event, data in frames
            ]
            body = b"".join(encoded)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            if len(requests) == 1:
                prefix = b"".join(encoded[:3])
                self.wfile.write(prefix)
                self.wfile.flush()
                continue_stream.wait(timeout=15)
                self.wfile.write(body[len(prefix):])
            else:
                self.wfile.write(body)

        def log_message(self, *_args) -> None:
            pass

    gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
    thread = threading.Thread(target=gateway.serve_forever, daemon=True)
    thread.start()
    try:
        sandbox.write_config(
            '\n[active]\nprovider = "openai"\n[openai]\napi_key = "test"\n'
            f'responses_api = true\nbase_url = "http://127.0.0.1:{gateway.server_port}"\n'
        )
        env = {"ZHUYE_MODEL": "openai/gpt-4o", "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            session.send({"type": "register_tools", "tools": [{
                "name": "scenario_lookup", "description": "按 key 查询",
                "parameters": {"type": "object", "properties": {"key": {"type": "string"}},
                               "required": ["key"]},
            }]})
            submission = session.send({"type": "user_input", "items": [
                {"type": "text", "text": "查询 alpha 和 beta"}
            ], "thread_settings": {"max_tool_concurrency": 2}})
            early = session.wait_event(submission, "tool_call_args_delta", timeout=10)["msg"]
            assert early["call_id"] == "call-a" and early["delta"] == '{"key":', early
            continue_stream.set()
            pending = {}
            for key in ("alpha", "beta"):
                event = session.wait_global(
                    lambda e: e["msg"].get("type") == "tool_execution_request"
                    and (e["msg"].get("args") or {}).get("key") == key, timeout=10,
                )
                pending[key] = event["msg"]["call_id"]
            assert pending["alpha"] != pending["beta"], pending
            for key in ("beta", "alpha"):
                session.send({"type": "tool_execution_response", "call_id": pending[key], "output": {
                    "content": [{"type": "text", "text": f"结果-{key}"}],
                    "is_error": False, "metadata": {}, "elapsed_ms": 0,
                }})
            events = session.collect(submission, timeout=30)
            assert first_event(events, "turn_complete")["msg"]["status"] == "success", events
            ends = {e["msg"]["call_id"]: e["msg"] for e in events if e["msg"]["type"] == "tool_call_end"}
            assert set(ends) == {"call-a", "call-b"}, ends
            for call_id, key in (("call-a", "alpha"), ("call-b", "beta")):
                assert ends[call_id]["output"]["content"] == [{"type": "text", "text": f"结果-{key}"}]
            assert "".join(e["msg"]["delta"] for e in events
                           if e["msg"]["type"] == "agent_message_delta") == "合并完成"
        assert len(requests) == 2 and paths == ["/v1/responses"] * 2
        _assert_call_history(requests[1])

        tid = rollout_thread_id(sandbox.home)
        records = [json.loads(line) for line in rollout_files(sandbox.home)[-1].read_text().splitlines() if line]
        assert any(r.get("role") == "assistant" and "call-a" in str(r.get("content"))
                   and "call-b" in str(r.get("content")) for r in records), records
        with serve_session(args=["--resume", tid], env=env) as resumed:
            events = resumed.submit({"type": "user_input", "items": [
                {"type": "text", "text": "接着查询"}
            ]}, timeout=30)
            assert first_event(events, "turn_complete")["msg"]["status"] == "success", events
            assert "".join(e["msg"]["delta"] for e in events
                           if e["msg"]["type"] == "agent_message_delta") == "恢复完成"
        assert len(requests) == 3
        _assert_call_history(requests[2])
        assert "接着查询" in json.dumps(requests[2]["input"], ensure_ascii=False)
    finally:
        continue_stream.set()
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()


def _assert_call_history(request: dict) -> None:
    """Responses 协议要求调用与结果是按 call_id 关联的独立输入项。"""
    calls = [item for item in request["input"] if item.get("type") == "function_call"]
    assert {item["call_id"] for item in calls} == {"call-a", "call-b"}, request["input"]
    assert {item["call_id"]: json.loads(item["arguments"]) for item in calls} == {
        "call-a": {"key": "alpha"}, "call-b": {"key": "beta"},
    }, calls
    outputs = [item for item in request["input"] if item.get("type") == "function_call_output"]
    assert {item["call_id"]: item["output"] for item in outputs} == {
        "call-a": "结果-alpha", "call-b": "结果-beta",
    }, outputs


@pytest.mark.parametrize("failure_event", ["response.failed", "error"])
def test_Responses流内错误不会伪装成功(sandbox, serve_session, failure_event) -> None:
    requests: list[dict] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            error = {"code": "invalid_prompt", "message": "请求字段不合法"}
            payload = ({"type": "response.failed", "response": {"status": "failed", "error": error}}
                       if failure_event == "response.failed" else {"type": "error", **error})
            body = ("event: " + failure_event + "\ndata: " + json.dumps(payload, ensure_ascii=False)
                    + "\n\n").encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args) -> None:
            pass

    gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
    thread = threading.Thread(target=gateway.serve_forever, daemon=True)
    thread.start()
    try:
        sandbox.write_config(
            '\n[active]\nprovider = "openai"\n[openai]\napi_key = "test"\n'
            f'responses_api = true\nbase_url = "http://127.0.0.1:{gateway.server_port}"\n'
        )
        env = {"ZHUYE_MODEL": "openai/gpt-4o", "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            events = session.submit({"type": "user_input", "items": [{"type": "text", "text": "开始"}]})
        error = first_event(events, "error")
        assert error is not None and error["msg"]["code"] == "INVALID_REQUEST", events
        assert "请求字段不合法" in error["msg"]["message"], error
        assert first_event(events, "turn_complete") is None, events
        assert len(requests) == 1, requests
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()


def test_Responses输出达到上限后自动续写(sandbox, serve_session) -> None:
    requests: list[dict] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            terminal = ("response.incomplete", {"response": {"status": "incomplete",
                        "incomplete_details": {"reason": "max_output_tokens"},
                        "usage": {"input_tokens": 12, "output_tokens": 4}}}) if len(requests) == 1 else (
                        "response.completed", {"response": {"status": "completed",
                        "usage": {"input_tokens": 15, "output_tokens": 3}}})
            frames = [("response.output_text.delta", {"delta": "第一段" if len(requests) == 1 else "第二段"}),
                      terminal]
            body = "".join("event: " + event + "\ndata: " + json.dumps(data, ensure_ascii=False)
                           + "\n\n" for event, data in frames).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args) -> None:
            pass

    gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
    thread = threading.Thread(target=gateway.serve_forever, daemon=True)
    thread.start()
    try:
        sandbox.write_config(
            '\n[active]\nprovider = "openai"\n[openai]\napi_key = "test"\n'
            f'responses_api = true\nbase_url = "http://127.0.0.1:{gateway.server_port}"\n'
        )
        env = {"ZHUYE_MODEL": "openai/gpt-4o", "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            events = session.submit({"type": "user_input", "items": [{"type": "text", "text": "写文章"}]})
        assert first_event(events, "turn_complete")["msg"]["status"] == "success", events
        assert "".join(e["msg"]["delta"] for e in events
                       if e["msg"]["type"] == "agent_message_delta") == "第一段第二段"
        assert len(requests) == 2, requests
        assert "第一段" in json.dumps(requests[1]["input"], ensure_ascii=False)
        usage = first_event(events, "turn_complete")["msg"]["usage"]
        assert usage["input_tokens"] == 27 and usage["output_tokens"] == 7, usage
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()


def test_Responses未完成的工具调用不会执行(sandbox, serve_session) -> None:
    marker = sandbox.cwd / "must-not-exist"

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            self.rfile.read(int(self.headers["Content-Length"]))
            frames = [
                ("response.output_item.added", {"output_index": 0, "item": {
                    "type": "function_call", "id": "fc-partial", "call_id": "call-partial",
                    "name": "write", "arguments": ""}}),
                ("response.function_call_arguments.delta", {"output_index": 0, "item_id": "fc-partial",
                                                             "delta": '{"path":"must-not-exist",'}),
                ("response.incomplete", {"response": {"status": "incomplete",
                    "incomplete_details": {"reason": "max_output_tokens"}}}),
            ]
            body = "".join("event: " + event + "\ndata: " + json.dumps(data, ensure_ascii=False)
                           + "\n\n" for event, data in frames).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args) -> None:
            pass

    gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
    thread = threading.Thread(target=gateway.serve_forever, daemon=True)
    thread.start()
    try:
        sandbox.write_config(
            '\n[active]\nprovider = "openai"\n[openai]\napi_key = "test"\n'
            f'responses_api = true\nbase_url = "http://127.0.0.1:{gateway.server_port}"\n'
        )
        env = {"ZHUYE_MODEL": "openai/gpt-4o", "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            events = session.submit({"type": "user_input", "items": [
                {"type": "text", "text": "写文件"}
            ]})
        error = first_event(events, "error")
        assert error is not None and error["msg"]["code"] == "PROVIDER_ERROR", events
        assert "max_output_tokens" in error["msg"]["message"], error
        assert first_event(events, "tool_call_end") is None, events
        assert first_event(events, "turn_complete") is None, events
        assert not marker.exists()
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()


def test_Responses流内失败冷却凭证并在下一轮复用备用(sandbox, serve_session) -> None:
    requests: list[tuple[str, dict]] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            auth = self.headers.get("Authorization", "")
            requests.append((auth, json.loads(self.rfile.read(int(self.headers["Content-Length"])))))
            if auth == "Bearer primary-key":
                frames = [("response.failed", {"response": {"status": "failed", "error": {
                    "code": "server_error", "message": "模型服务故障"}}})]
            else:
                frames = [("response.output_text.delta", {"delta": "备用回复"}),
                          ("response.completed", {"response": {"status": "completed"}})]
            body = "".join("event: " + event + "\ndata: " + json.dumps(data, ensure_ascii=False)
                           + "\n\n" for event, data in frames).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args) -> None:
            pass

    gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
    thread = threading.Thread(target=gateway.serve_forever, daemon=True)
    thread.start()
    try:
        url = f"http://127.0.0.1:{gateway.server_port}"
        sandbox.write_config(
            '\n[active]\nprovider = "openai"\ncredential = "primary"\n'
            '[openai]\nmodel = "gpt-4o"\nresponses_api = true\n'
            f'[[openai.credentials]]\nlabel = "primary"\napi_key = "primary-key"\nbase_url = "{url}"\n'
            f'[[openai.credentials]]\nlabel = "backup"\napi_key = "backup-key"\nbase_url = "{url}"\n'
        )
        env = {"ZHUYE_MODEL": "openai/gpt-4o", "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            first = session.submit({"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]})
            error = first_event(first, "stream_error")
            assert error is not None and error["msg"]["code"] == "PROVIDER_ERROR", first
            assert error["msg"]["credential_label"] == "primary", error
            complete = first_event(first, "turn_complete")
            assert complete is not None and complete["msg"]["status"] == "success", first
            second = session.submit({"type": "user_input", "items": [{"type": "text", "text": "第二轮"}]})
            assert first_event(second, "turn_complete")["msg"]["status"] == "success", second
        assert [auth for auth, _ in requests] == ["Bearer primary-key", "Bearer backup-key", "Bearer backup-key"]
        assert "第一轮" in json.dumps(requests[-1][1]["input"], ensure_ascii=False)
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()


def test_Responses配额耗尽后下一轮切换凭证(sandbox, serve_session) -> None:
    requests: list[str] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            requests.append(self.headers.get("Authorization", ""))
            self.rfile.read(int(self.headers["Content-Length"]))
            frames = [("response.output_text.delta", {"delta": "完成"}),
                      ("response.completed", {"response": {"status": "completed",
                       "usage": {"input_tokens": 7, "output_tokens": 4}}})]
            body = "".join("event: " + event + "\ndata: " + json.dumps(data, ensure_ascii=False)
                           + "\n\n" for event, data in frames).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args) -> None:
            pass

    gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
    thread = threading.Thread(target=gateway.serve_forever, daemon=True)
    thread.start()
    try:
        url = f"http://127.0.0.1:{gateway.server_port}"
        sandbox.write_config(
            '\n[active]\nprovider = "openai"\ncredential = "primary"\n'
            '[openai]\nmodel = "gpt-4o"\nresponses_api = true\n'
            f'[[openai.credentials]]\nlabel = "primary"\napi_key = "primary-key"\nbase_url = "{url}"\n'
            '[openai.credentials.quota]\nwindow_secs = 3600\nmax_tokens = 10\n'
            f'[[openai.credentials]]\nlabel = "backup"\napi_key = "backup-key"\nbase_url = "{url}"\n'
        )
        env = {"ZHUYE_MODEL": "openai/gpt-4o", "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            first = session.submit({"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]})
            assert first_event(first, "turn_complete")["msg"]["status"] == "success", first
            quota = session.wait_global(lambda e: e["msg"].get("type") == "quota_exhausted")["msg"]
            assert quota["provider"] == "openai" and quota["label"] == "primary", quota
            assert quota["used_tokens"] == 11 and quota["window_ends_secs"] > 0, quota
            second = session.submit({"type": "user_input", "items": [{"type": "text", "text": "第二轮"}]})
            assert first_event(second, "turn_complete")["msg"]["status"] == "success", second
        assert requests == ["Bearer primary-key", "Bearer backup-key"], requests
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
