"""模型输出无效工具参数时,宿主工具不可被执行。"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from test_stream_failure import _successful_frames
from wire import first_event


def _invalid_call(provider: str) -> bytes:
    bad_args = '{"secret":"SENSITIVE"'
    if provider == "chat":
        chunk = {"id": "bad", "model": "gpt-4o", "choices": [{"index": 0,
                 "delta": {"tool_calls": [{"index": 0, "id": "call-bad", "type": "function",
                                           "function": {"name": "dangerous_noargs", "arguments": bad_args}},
                                          {"index": 1, "id": "call-good", "type": "function",
                                           "function": {"name": "safe_lookup", "arguments": '{"value":"ok"}'}}]},
                 "finish_reason": "tool_calls"}]}
        return ("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\ndata: [DONE]\n\n").encode()
    if provider == "responses":
        frames = [
            ("response.output_item.added", {"output_index": 0, "item": {
                "type": "function_call", "id": "fc-bad", "call_id": "call-bad",
                "name": "dangerous_noargs", "arguments": ""}}),
            ("response.function_call_arguments.delta", {"output_index": 0, "item_id": "fc-bad",
                                                         "delta": bad_args}),
            ("response.function_call_arguments.done", {"output_index": 0, "item_id": "fc-bad",
                                                        "arguments": bad_args}),
            ("response.output_item.added", {"output_index": 1, "item": {
                "type": "function_call", "id": "fc-good", "call_id": "call-good",
                "name": "safe_lookup", "arguments": ""}}),
            ("response.function_call_arguments.delta", {"output_index": 1, "item_id": "fc-good",
                                                         "delta": '{"value":"ok"}'}),
            ("response.function_call_arguments.done", {"output_index": 1, "item_id": "fc-good",
                                                        "arguments": '{"value":"ok"}'}),
            ("response.completed", {"response": {"status": "completed"}}),
        ]
    else:
        frames = [
            ("message_start", {"type": "message_start", "message": {"id": "bad", "model": "claude-test"}}),
            ("content_block_start", {"type": "content_block_start", "index": 0,
                                     "content_block": {"type": "tool_use", "id": "call-bad",
                                                       "name": "dangerous_noargs", "input": {}}}),
            ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                     "delta": {"type": "input_json_delta", "partial_json": bad_args}}),
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            ("content_block_start", {"type": "content_block_start", "index": 1,
                                     "content_block": {"type": "tool_use", "id": "call-good",
                                                       "name": "safe_lookup", "input": {}}}),
            ("content_block_delta", {"type": "content_block_delta", "index": 1,
                                     "delta": {"type": "input_json_delta", "partial_json": '{"value":"ok"}'}}),
            ("content_block_stop", {"type": "content_block_stop", "index": 1}),
            ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
            ("message_stop", {"type": "message_stop"}),
        ]
    return "".join("event: " + event + "\ndata: " + json.dumps(data, ensure_ascii=False)
                   + "\n\n" for event, data in frames).encode()


@pytest.mark.parametrize("provider", ["chat", "responses", "anthropic"])
def test_无效参数不触发宿主工具并向模型返回错误(sandbox, serve_session, provider) -> None:
    requests: list[dict] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            body = _invalid_call(provider) if len(requests) == 1 else _successful_frames(provider)
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
        provider_name = "anthropic" if provider == "anthropic" else "openai"
        model = "anthropic/claude-test" if provider == "anthropic" else "openai/gpt-4o"
        option = "responses_api = true\n" if provider == "responses" else ""
        sandbox.write_config(
            f'\n[active]\nprovider = "{provider_name}"\n[{provider_name}]\napi_key = "test"\n'
            f'{option}base_url = "http://127.0.0.1:{gateway.server_port}"\n'
        )
        env = {"ZHUYE_MODEL": model, "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        marker = sandbox.cwd / "side-effect"
        with serve_session(env=env) as session:
            session.send({"type": "register_tools", "tools": [{
                "name": "dangerous_noargs", "description": "模拟有副作用的宿主工具",
                "parameters": {"type": "object", "properties": {}},
            }, {
                "name": "safe_lookup", "description": "验证同批有效调用",
                "parameters": {"type": "object", "properties": {"value": {"type": "string"}}},
            }]})
            done = threading.Event()

            def host() -> None:
                seen: set[str] = set()
                while not done.wait(0.02):
                    for event in session.global_events():
                        msg = event["msg"]
                        if msg.get("type") != "tool_execution_request" or msg["call_id"] in seen:
                            continue
                        seen.add(msg["call_id"])
                        if msg["tool"] == "dangerous_noargs":
                            marker.write_text("工具被执行")
                            result = "错误地执行了"
                        else:
                            assert msg["tool"] == "safe_lookup" and msg["args"] == {"value": "ok"}, msg
                            result = "安全结果"
                        session.send({"type": "tool_execution_response", "call_id": msg["call_id"], "output": {
                            "content": [{"type": "text", "text": result}],
                            "is_error": False, "metadata": {}, "elapsed_ms": 0,
                        }})

            worker = threading.Thread(target=host, daemon=True)
            worker.start()
            try:
                events = session.submit({"type": "user_input", "items": [
                    {"type": "text", "text": "调用工具"}
                ]}, timeout=20)
            finally:
                done.set()
                worker.join(timeout=5)
            ends = [e["msg"] for e in events if e["msg"]["type"] == "tool_call_end"]
            assert [e["call_id"] for e in ends] == ["call-bad", "call-good"], ends
            assert ends[0]["is_error"] is True and ends[1]["is_error"] is False, ends
            output = ends[0]["output"]
            assert "JSON" in str(output) and "SENSITIVE" not in str(output), output
            assert ends[1]["output"]["content"] == [{"type": "text", "text": "安全结果"}]
            assert not marker.exists(), "无效参数仍触发了宿主工具"
            assert all(e["msg"].get("tool") != "dangerous_noargs" for e in session.global_events()
                       if e["msg"].get("type") == "tool_execution_request")
            assert first_event(events, "turn_complete")["msg"]["status"] == "success", events
        assert len(requests) == 2, requests
        history = json.dumps(requests[1], ensure_ascii=False)
        assert all(value in history for value in ("call-bad", "call-good", "JSON", "安全结果"))
        assert "SENSITIVE" not in history
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
