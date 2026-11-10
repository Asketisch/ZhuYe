"""Anthropic 兼容流的并行工具调用场景。"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from wire import first_event


def _frame(event: str, data: dict) -> bytes:
    return ("event: " + event + "\ndata: " + json.dumps(data, ensure_ascii=False)
            + "\n\n").encode()


def test_Anthropic并行工具交错分片逆序回执并恢复(sandbox, serve_session) -> None:
    requests: list[dict] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            if len(requests) == 1:
                frames = [
                    ("message_start", {"type": "message_start", "message": {
                        "id": "msg-parallel", "model": "claude-test",
                        "usage": {"input_tokens": 12}}}),
                    ("content_block_start", {"type": "content_block_start", "index": 0,
                        "content_block": {"type": "tool_use", "id": "toolu-a",
                                          "name": "scenario_lookup", "input": {}}}),
                    ("content_block_start", {"type": "content_block_start", "index": 1,
                        "content_block": {"type": "tool_use", "id": "toolu-b",
                                          "name": "scenario_lookup", "input": {}}}),
                    ("content_block_delta", {"type": "content_block_delta", "index": 0,
                        "delta": {"type": "input_json_delta", "partial_json": '{"key":'}}),
                    ("content_block_delta", {"type": "content_block_delta", "index": 1,
                        "delta": {"type": "input_json_delta", "partial_json": '{"key":'}}),
                    ("content_block_delta", {"type": "content_block_delta", "index": 0,
                        "delta": {"type": "input_json_delta", "partial_json": '"alpha"}'}}),
                    ("content_block_delta", {"type": "content_block_delta", "index": 1,
                        "delta": {"type": "input_json_delta", "partial_json": '"beta"}'}}),
                    ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                    ("content_block_stop", {"type": "content_block_stop", "index": 1}),
                    ("message_delta", {"type": "message_delta",
                        "delta": {"stop_reason": "tool_use"},
                        "usage": {"input_tokens": 12, "output_tokens": 8}}),
                    ("message_stop", {"type": "message_stop"}),
                ]
            else:
                frames = [
                    ("message_start", {"type": "message_start", "message": {
                        "id": "msg-final", "model": "claude-test"}}),
                    ("content_block_start", {"type": "content_block_start", "index": 0,
                                             "content_block": {"type": "text", "text": ""}}),
                    ("content_block_delta", {"type": "content_block_delta", "index": 0,
                        "delta": {"type": "text_delta", "text": "已合并结果"}}),
                    ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                    ("message_delta", {"type": "message_delta",
                        "delta": {"stop_reason": "end_turn"},
                        "usage": {"output_tokens": 4}}),
                    ("message_stop", {"type": "message_stop"}),
                ]
            body = b"".join(_frame(event, data) for event, data in frames)
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
            '\n[active]\nprovider = "anthropic"\n[anthropic]\napi_key = "test"\n'
            f'base_url = "http://127.0.0.1:{gateway.server_port}"\n'
        )
        env = {"ZHUYE_MODEL": "anthropic/claude-test", "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            session.send({"type": "register_tools", "tools": [{
                "name": "scenario_lookup", "description": "按 key 查询",
                "parameters": {"type": "object", "properties": {"key": {"type": "string"}},
                               "required": ["key"]},
            }]})
            submission = session.send({"type": "user_input", "items": [
                {"type": "text", "text": "查询 alpha 与 beta"}
            ], "thread_settings": {"max_tool_concurrency": 2}})
            pending = {}
            for key in ("alpha", "beta"):
                event = session.wait_global(
                    lambda e: e["msg"].get("type") == "tool_execution_request"
                    and (e["msg"].get("args") or {}).get("key") == key,
                    timeout=10,
                )
                pending[key] = event["msg"]["call_id"]
            assert set(pending) == {"alpha", "beta"} and pending["alpha"] != pending["beta"], pending
            for key in ("beta", "alpha"):
                session.send({"type": "tool_execution_response", "call_id": pending[key], "output": {
                    "content": [{"type": "text", "text": f"结果-{key}"}],
                    "is_error": False, "metadata": {}, "elapsed_ms": 0,
                }})
            events = session.collect(submission, timeout=30)
            ends = {e["msg"]["call_id"]: e["msg"] for e in events
                    if e["msg"]["type"] == "tool_call_end"}
            assert set(ends) == {"toolu-a", "toolu-b"}, ends
            for call_id, key in (("toolu-a", "alpha"), ("toolu-b", "beta")):
                assert ends[call_id]["output"]["content"] == [
                    {"type": "text", "text": f"结果-{key}"}
                ], ends[call_id]
            assert first_event(events, "turn_complete")["msg"]["status"] == "success", events
            assert "".join(e["msg"]["delta"] for e in events
                           if e["msg"]["type"] == "agent_message_delta") == "已合并结果"
        assert len(requests) == 2, requests
        messages = requests[1]["messages"]
        tool_uses = [block for message in messages for block in message.get("content", [])
                     if block.get("type") == "tool_use"]
        assert [(block["id"], block["input"]) for block in tool_uses] == [
            ("toolu-a", {"key": "alpha"}), ("toolu-b", {"key": "beta"}),
        ], messages
        results = [block for message in messages for block in message.get("content", [])
                   if block.get("type") == "tool_result"]
        assert {block["tool_use_id"]: block["content"][0]["text"] for block in results} == {
            "toolu-a": "结果-alpha", "toolu-b": "结果-beta",
        }, messages
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
