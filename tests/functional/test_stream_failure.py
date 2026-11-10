"""模型流提前结束时的重试、已见输出和工具副作用。"""

from __future__ import annotations

import json
import socket
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from wire import first_event, rollout_files


def _frames(provider: str, phase: str) -> bytes:
    if provider == "chat":
        if phase == "empty":
            delta = {"role": "assistant"}
        elif phase == "text":
            delta = {"content": "部分回复"}
        else:
            delta = {"tool_calls": [{"index": 0, "id": "write-call", "type": "function",
                                     "function": {"name": "write", "arguments": json.dumps(
                                         {"path": "must-not-exist", "content": "不应写入"},
                                         ensure_ascii=False)}}]}
        chunk = {"id": "test", "model": "gpt-4o", "choices": [
            {"index": 0, "delta": delta, "finish_reason": None}]}
        return ("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\n").encode()

    if provider == "responses":
        frames = [("response.created", {"response": {"id": "resp", "model": "gpt-4o"}})]
        if phase == "text":
            frames.append(("response.output_text.delta", {"delta": "部分回复"}))
        elif phase == "tool":
            frames.extend([
                ("response.output_item.added", {"output_index": 0, "item": {
                    "type": "function_call", "id": "fc-1", "call_id": "write-call",
                    "name": "write", "arguments": ""}}),
                ("response.function_call_arguments.delta", {"output_index": 0, "item_id": "fc-1",
                    "delta": json.dumps({"path": "must-not-exist", "content": "不应写入"},
                                        ensure_ascii=False)}),
            ])
    else:
        frames = [("message_start", {"type": "message_start", "message": {
            "id": "msg", "model": "claude-test", "usage": {"input_tokens": 1}}})]
        if phase == "text":
            frames.extend([
                ("content_block_start", {"type": "content_block_start", "index": 0,
                                         "content_block": {"type": "text", "text": ""}}),
                ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                         "delta": {"type": "text_delta", "text": "部分回复"}}),
            ])
        elif phase == "tool":
            frames.extend([
                ("content_block_start", {"type": "content_block_start", "index": 0,
                                         "content_block": {"type": "tool_use", "id": "write-call",
                                                           "name": "write", "input": {}}}),
                ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                         "delta": {"type": "input_json_delta", "partial_json": json.dumps(
                                             {"path": "must-not-exist", "content": "不应写入"},
                                             ensure_ascii=False)}}),
            ])
    return "".join("event: " + event + "\ndata: " + json.dumps(data, ensure_ascii=False)
                   + "\n\n" for event, data in frames).encode()


def _successful_frames(provider: str) -> bytes:
    if provider == "chat":
        body = {"id": "ok", "model": "gpt-4o", "choices": [{
            "index": 0, "delta": {"content": "正常回复"}, "finish_reason": "stop"}]}
        return ("data: " + json.dumps(body, ensure_ascii=False) + "\n\ndata: [DONE]\n\n").encode()
    if provider == "responses":
        frames = [("response.output_text.delta", {"delta": "正常回复"}),
                  ("response.completed", {"response": {"status": "completed"}})]
    else:
        frames = [
            ("message_start", {"type": "message_start", "message": {"id": "ok", "model": "claude-test"}}),
            ("content_block_start", {"type": "content_block_start", "index": 0,
                                     "content_block": {"type": "text", "text": ""}}),
            ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                     "delta": {"type": "text_delta", "text": "正常回复"}}),
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
            ("message_stop", {"type": "message_stop"}),
        ]
    return "".join("event: " + event + "\ndata: " + json.dumps(data, ensure_ascii=False)
                   + "\n\n" for event, data in frames).encode()


@pytest.mark.parametrize("provider", ["chat", "responses", "anthropic"])
@pytest.mark.parametrize("phase", ["empty", "text", "tool"])
@pytest.mark.parametrize("truncated", [False, True], ids=["干净EOF", "连接截断"])
def test_模型流提前断开后的重试与恢复(sandbox, serve_session, provider, phase, truncated) -> None:
    requests: list[dict] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            body = _frames(provider, phase) if len(requests) == 1 else _successful_frames(provider)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body) + (100 if len(requests) == 1 and truncated else 0)))
            self.end_headers()
            self.wfile.write(body)
            self.wfile.flush()
            if len(requests) == 1 and truncated:
                self.connection.shutdown(socket.SHUT_WR)

        def log_message(self, *_args) -> None:
            pass

    gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
    thread = threading.Thread(target=gateway.serve_forever, daemon=True)
    thread.start()
    try:
        provider_name = "anthropic" if provider == "anthropic" else "openai"
        model = "anthropic/claude-test" if provider == "anthropic" else "openai/gpt-4o"
        response_option = "responses_api = true\n" if provider == "responses" else ""
        sandbox.write_config(
            f'\n[active]\nprovider = "{provider_name}"\n[{provider_name}]\napi_key = "test"\n'
            f'{response_option}base_url = "http://127.0.0.1:{gateway.server_port}"\n'
        )
        env = {"ZHUYE_MODEL": model, "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            first = session.send({"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]})
            if phase == "empty":
                events = session.collect(first, timeout=20)
                assert first_event(events, "turn_complete")["msg"]["status"] == "success", events
                assert "".join(e["msg"]["delta"] for e in events
                               if e["msg"]["type"] == "agent_message_delta") == "正常回复"
                assert len(requests) == 2, requests
            else:
                error = session.wait_event(first, "error", timeout=10)
                assert error["msg"]["code"] == "SSE_PARSE_ERROR", error
                assert len(requests) == 1, requests
                assert not (sandbox.cwd / "must-not-exist").exists()
                if phase == "text":
                    records = [json.loads(line) for path in rollout_files(sandbox.home)
                               for line in path.read_text().splitlines() if line]
                    assert any(r.get("role") == "assistant" and "部分回复" in str(r.get("content"))
                               for r in records), records
                later_id = session.send({"type": "user_input", "items": [
                    {"type": "text", "text": "第二轮"}
                ]})
                session.wait_event(first, "submission_closed", timeout=10)
                events = [e for e in session.all_events() if e["id"] == first]
                assert first_event(events, "turn_complete") is None, events
                assert first_event(events, "tool_call_end") is None, events
                if phase == "text":
                    assert "".join(e["msg"]["delta"] for e in events
                                   if e["msg"]["type"] == "agent_message_delta") == "部分回复"
                later = session.collect(later_id, timeout=20)
                assert first_event(later, "turn_complete")["msg"]["status"] == "success", later
                assert len(requests) == 2
                if phase == "text":
                    assert "部分回复" in json.dumps(requests[-1], ensure_ascii=False)
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()


@pytest.mark.parametrize(
    ("provider", "expected_code"),
    [("chat", "RATE_LIMITED"), ("responses", "PROVIDER_ERROR"), ("anthropic", "OVERLOADED")],
)
def test_部分输出后流内报错会保留内容并冷却凭证(
    sandbox, serve_session, provider, expected_code,
) -> None:
    requests: list[tuple[str, dict]] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            auth = self.headers.get("Authorization", "")
            requests.append((auth, json.loads(self.rfile.read(int(self.headers["Content-Length"])))))
            if auth == "Bearer primary-key":
                if provider == "chat":
                    failure = ("data: " + json.dumps({"error": {"code": "rate_limit_exceeded",
                               "message": "限流"}}) + "\n\n").encode()
                else:
                    event = "response.failed" if provider == "responses" else "error"
                    data = ({"response": {"status": "failed", "error": {
                        "code": "server_error", "message": "暂不可用"}}} if provider == "responses" else
                        {"type": "error", "error": {"type": "overloaded_error", "message": "过载"}})
                    failure = ("event: " + event + "\ndata: " + json.dumps(data, ensure_ascii=False)
                               + "\n\n").encode()
                body = _frames(provider, "text") + failure
            else:
                body = _successful_frames(provider)
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
        name = "anthropic" if provider == "anthropic" else "openai"
        model = "anthropic/claude-test" if provider == "anthropic" else "openai/gpt-4o"
        response_option = "responses_api = true\n" if provider == "responses" else ""
        url = f"http://127.0.0.1:{gateway.server_port}"
        sandbox.write_config(
            f'\n[active]\nprovider = "{name}"\ncredential = "primary"\n'
            f'[{name}]\n{response_option}'
            f'[[{name}.credentials]]\nlabel = "primary"\napi_key = "primary-key"\nbase_url = "{url}"\n'
            f'[[{name}.credentials]]\nlabel = "backup"\napi_key = "backup-key"\nbase_url = "{url}"\n'
        )
        env = {"ZHUYE_MODEL": model, "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            first = session.send({"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]})
            error = session.wait_event(first, "error", timeout=10)
            assert error["msg"]["code"] == expected_code, error
            assert error["msg"]["details"]["credential_label"] == "primary", error
            assert [auth for auth, _ in requests] == ["Bearer primary-key"], requests
            records = [json.loads(line) for path in rollout_files(sandbox.home)
                       for line in path.read_text().splitlines() if line]
            assert any(r.get("role") == "assistant" and "部分回复" in str(r.get("content"))
                       for r in records), records
            later = session.submit({"type": "user_input", "items": [
                {"type": "text", "text": "继续"}
            ]}, timeout=20)
            assert first_event(later, "turn_complete")["msg"]["status"] == "success", later
            assert [auth for auth, _ in requests] == ["Bearer primary-key", "Bearer backup-key"], requests
            assert "部分回复" in json.dumps(requests[-1][1], ensure_ascii=False)
            session.wait_event(first, "submission_closed", timeout=10)
            events = [e for e in session.all_events() if e["id"] == first]
            assert first_event(events, "turn_complete") is None, events
            assert "".join(e["msg"]["delta"] for e in events
                           if e["msg"]["type"] == "agent_message_delta") == "部分回复"
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
