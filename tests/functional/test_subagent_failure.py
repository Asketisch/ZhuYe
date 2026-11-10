"""子代理失败后父代理继续和重启恢复。"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from wire import first_event, rollout_files, rollout_thread_id


@pytest.mark.parametrize("partial", [False, True], ids=["调用前失败", "部分回复后失败"])
def test_子代理失败不能作为成功结果并可恢复(sandbox, serve_session, partial) -> None:
    requests: list[dict] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append(request)
            number = len(requests)
            if number == 2 and not partial:
                body = json.dumps({"error": {"message": "子代理请求被拒绝"}},
                                  ensure_ascii=False).encode()
                self.send_response(400)
                self.send_header("Content-Type", "application/json")
            else:
                if number == 1:
                    delta = {"tool_calls": [{"index": 0, "id": "delegate-call", "type": "function",
                        "function": {"name": "call_explorer", "arguments": '{"prompt":"探索代码"}'}}]}
                    chunks = [{"id": "parent", "model": "gpt-4o", "choices": [{
                        "index": 0, "delta": delta, "finish_reason": "tool_calls"}]}]
                elif number == 2:
                    chunks = [
                        {"id": "child", "model": "gpt-4o", "choices": [{
                            "index": 0, "delta": {"content": "不完整的子代理回复"}, "finish_reason": None}]},
                        {"error": {"type": "invalid_request_error", "message": "子代理流内失败"}},
                    ]
                else:
                    text = {3: "父代理已处理失败", 4: "会话可继续", 5: "重启后可继续"}[number]
                    chunks = [{"id": "parent", "model": "gpt-4o", "choices": [{
                        "index": 0, "delta": {"content": text}, "finish_reason": "stop"}]}]
                body = b"".join(("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\n").encode()
                                for chunk in chunks) + b"data: [DONE]\n\n"
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
            f'base_url = "http://127.0.0.1:{gateway.server_port}"\n'
        )
        env = {"ZHUYE_MODEL": "openai/gpt-4o", "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            first = session.submit({"type": "user_input", "items": [
                {"type": "text", "text": "请委派探索"}
            ]}, timeout=30)
            tool_end = first_event(first, "tool_call_end")
            assert tool_end is not None and tool_end["msg"]["call_id"] == "delegate-call", first
            assert tool_end["msg"]["is_error"] is True, tool_end
            output = tool_end["msg"]["output"]
            assert output["is_error"] is True, output
            output_text = "".join(c.get("text", "") for c in output["content"])
            expected_error = "PROVIDER_ERROR" if partial else "INVALID_REQUEST"
            expected_message = "子代理流内失败" if partial else "子代理请求被拒绝"
            assert expected_error in output_text and expected_message in output_text, output_text
            assert "不完整的子代理回复" not in output_text
            if partial:
                assert any(e["msg"]["type"] == "subagent_progress"
                           and e["msg"].get("text") == "不完整的子代理回复" for e in first), first
            assert first_event(first, "turn_complete")["msg"]["status"] == "success", first
            assert "".join(e["msg"]["delta"] for e in first
                           if e["msg"]["type"] == "agent_message_delta") == "父代理已处理失败"
            children = first_event(session.submit({"type": "query_subagents"}), "subagent_status")["msg"]["children"]
            assert len(children) == 1 and children[0]["role"] == "explorer", children
            assert children[0]["state"] == "failed" and children[0]["finished_at"], children
            second = session.submit({"type": "user_input", "items": [{"type": "text", "text": "继续工作"}]})
            assert first_event(second, "turn_complete")["msg"]["status"] == "success", second
        assert len(requests) == 4, requests
        _assert_failed_tool_history(requests[2], expected_error, expected_message)
        _assert_failed_tool_history(requests[3], expected_error, expected_message)

        records = [json.loads(line) for line in rollout_files(sandbox.home)[-1].read_text().splitlines() if line]
        blocks = [block for record in records if record.get("role") == "assistant"
                  for block in record.get("content", []) if isinstance(block, dict)]
        result = next(b for b in blocks if b.get("type") == "tool_result" and b.get("call_id") == "delegate-call")
        assert result["output"]["is_error"] is True, result
        assert expected_error in str(result) and expected_message in str(result), result
        tid = rollout_thread_id(sandbox.home)
        with serve_session(args=["--resume", tid], env=env) as resumed:
            third = resumed.submit({"type": "user_input", "items": [
                {"type": "text", "text": "重启后继续"}
            ]})
            assert first_event(third, "turn_complete")["msg"]["status"] == "success", third
            assert "".join(e["msg"]["delta"] for e in third
                           if e["msg"]["type"] == "agent_message_delta") == "重启后可继续"
        assert len(requests) == 5, requests
        _assert_failed_tool_history(requests[4], expected_error, expected_message)
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()


def _assert_failed_tool_history(request: dict, code: str, message: str) -> None:
    results = [m for m in request["messages"]
               if m.get("role") == "tool" and m.get("tool_call_id") == "delegate-call"]
    assert len(results) == 1, request["messages"]
    assert code in results[0]["content"] and message in results[0]["content"], results
    assert "不完整的子代理回复" not in results[0]["content"], results


def test_子代理临时限流切换凭证后成功返回(sandbox, serve_session) -> None:
    requests: list[tuple[str, dict]] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            auth = self.headers.get("Authorization", "")
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append((auth, request))
            number = len(requests)
            if number == 2:
                body = b'{"error":{"message":"rate limited"}}'
                self.send_response(429)
                self.send_header("Content-Type", "application/json")
                self.send_header("Retry-After", "60")
            else:
                if number == 1:
                    delta = {"tool_calls": [{"index": 0, "id": "delegate-call", "type": "function",
                        "function": {"name": "call_explorer", "arguments": '{"prompt":"探索代码"}'}}]}
                    finish = "tool_calls"
                else:
                    delta = {"content": "子代理已恢复" if number == 3 else "父代理继续"}
                    finish = "stop"
                chunk = {"id": "agent", "model": "gpt-4o", "choices": [{
                    "index": 0, "delta": delta, "finish_reason": finish}]}
                body = ("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\ndata: [DONE]\n\n").encode()
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
            '[openai]\nmodel = "gpt-4o"\n'
            f'[[openai.credentials]]\nlabel = "primary"\napi_key = "primary-key"\nbase_url = "{url}"\n'
            f'[[openai.credentials]]\nlabel = "backup"\napi_key = "backup-key"\nbase_url = "{url}"\n'
        )
        env = {"ZHUYE_MODEL": "openai/gpt-4o", "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            events = session.submit({"type": "user_input", "items": [
                {"type": "text", "text": "请委派探索"}
            ]}, timeout=30)
            end = first_event(events, "tool_call_end")
            assert end is not None and end["msg"]["is_error"] is False, events
            assert end["msg"]["output"]["content"] == [{"type": "text", "text": "子代理已恢复"}]
            assert first_event(events, "turn_complete")["msg"]["status"] == "success", events
            children = first_event(session.submit({"type": "query_subagents"}), "subagent_status")["msg"]["children"]
            assert len(children) == 1 and children[0]["state"] == "completed", children
            assert children[0]["finished_at"], children
        assert [auth for auth, _ in requests] == [
            "Bearer primary-key", "Bearer primary-key", "Bearer backup-key", "Bearer backup-key",
        ], requests
        result = next(m for m in requests[-1][1]["messages"]
                      if m.get("role") == "tool" and m.get("tool_call_id") == "delegate-call")
        assert result["content"] == "子代理已恢复", result
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
