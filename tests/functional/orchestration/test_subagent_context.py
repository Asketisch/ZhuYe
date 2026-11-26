"""子代理父上下文传递和工具历史配对。"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from wire import first_event


def _chunk(delta: dict, finish: str) -> bytes:
    body = {"id": "test", "model": "gpt-4o", "choices": [{
        "index": 0, "delta": delta, "finish_reason": finish}]}
    return ("data: " + json.dumps(body, ensure_ascii=False) + "\n\ndata: [DONE]\n\n").encode()


def _tool(call_id: str, name: str, args: dict) -> dict:
    return {"tool_calls": [{"index": 0, "id": call_id, "type": "function", "function": {
        "name": name, "arguments": json.dumps(args, ensure_ascii=False)}}]}


@pytest.mark.parametrize("context_tail", [0, 2], ids=["默认隔离", "显式传递"])
def test_父上下文只在请求时按尾部传给子代理(sandbox, serve_session, context_tail) -> None:
    requests: list[dict] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            number = len(requests)
            if number == 1:
                delta, finish = {"content": "项目代号是北极星"}, "stop"
            elif number == 2:
                delta, finish = _tool("delegate", "call_explorer", {
                    "prompt": "检查相关文件", "context_tail": context_tail,
                }), "tool_calls"
            else:
                delta, finish = {"content": "子代理完成" if number == 3 else "父代理完成"}, "stop"
            body = _chunk(delta, finish)
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
                {"type": "text", "text": "旧轮内容不应传递"}
            ]})
            assert first_event(first, "turn_complete")["msg"]["status"] == "success", first
            second = session.submit({"type": "user_input", "items": [
                {"type": "text", "text": "交给子代理处理"}
            ]}, timeout=30)
            end = first_event(second, "tool_call_end")
            assert end is not None and end["msg"]["is_error"] is False, second
            assert first_event(second, "turn_complete")["msg"]["status"] == "success", second
        assert len(requests) == 4, requests
        child_messages = requests[2]["messages"]
        child_wire = json.dumps(child_messages, ensure_ascii=False)
        assert "检查相关文件" in child_wire
        assert "旧轮内容不应传递" not in child_wire
        assert "delegate" not in child_wire, child_messages
        if context_tail:
            assert "项目代号是北极星" in child_wire and "交给子代理处理" in child_wire, child_messages
        else:
            assert "项目代号是北极星" not in child_wire and "交给子代理处理" not in child_wire, child_messages
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()


def test_父上下文截断边界保留完整工具调用和结果(sandbox, serve_session) -> None:
    requests: list[dict] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            number = len(requests)
            if number == 1:
                delta, finish = _tool("parent-echo", "echo", {"text": "已知线索"}), "tool_calls"
            elif number == 2:
                delta, finish = {"content": "线索已经收集"}, "stop"
            elif number == 3:
                delta, finish = _tool("delegate", "call_explorer", {
                    "prompt": "基于线索探索", "context_tail": 3,
                }), "tool_calls"
            else:
                delta, finish = {"content": "子代理完成" if number == 4 else "父代理完成"}, "stop"
            body = _chunk(delta, finish)
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
                {"type": "text", "text": "先收集线索"}
            ]})
            assert first_event(first, "tool_call_end")["msg"]["is_error"] is False, first
            assert first_event(first, "turn_complete")["msg"]["status"] == "success", first
            second = session.submit({"type": "user_input", "items": [
                {"type": "text", "text": "交给子代理"}
            ]}, timeout=30)
            assert first_event(second, "turn_complete")["msg"]["status"] == "success", second
        assert len(requests) == 5, requests
        child = requests[3]["messages"]
        calls = [(i, call) for i, message in enumerate(child)
                 for call in message.get("tool_calls", []) if call.get("id") == "parent-echo"]
        results = [(i, message) for i, message in enumerate(child)
                   if message.get("role") == "tool" and message.get("tool_call_id") == "parent-echo"]
        assert len(calls) == len(results) == 1, child
        assert calls[0][0] < results[0][0] and "已知线索" in results[0][1]["content"], child
        assert "delegate" not in json.dumps(child, ensure_ascii=False), child
        assert "基于线索探索" in json.dumps(child, ensure_ascii=False), child
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()


def test_父上下文请求超过十条仍按请求传递(sandbox, serve_session) -> None:
    requests: list[dict] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            number = len(requests)
            if number <= 7:
                delta, finish = {"content": f"ASSIST-{number}"}, "stop"
            elif number == 8:
                delta, finish = _tool("delegate", "call_explorer", {
                    "prompt": "检查历史", "context_tail": 12,
                }), "tool_calls"
            else:
                delta, finish = {"content": "子代理完成" if number == 9 else "父代理完成"}, "stop"
            body = _chunk(delta, finish)
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
            for index in range(1, 8):
                events = session.submit({"type": "user_input", "items": [
                    {"type": "text", "text": f"USER-{index}"}
                ]})
                assert first_event(events, "turn_complete")["msg"]["status"] == "success", events
            events = session.submit({"type": "user_input", "items": [
                {"type": "text", "text": "USER-8"}
            ]}, timeout=30)
            assert first_event(events, "turn_complete")["msg"]["status"] == "success", events
        assert len(requests) == 10, requests
        child = json.dumps(requests[8]["messages"], ensure_ascii=False)
        assert "USER-1" not in child and "ASSIST-1" not in child and "USER-2" not in child, child
        assert all(f"ASSIST-{n}" in child for n in range(2, 8)), child
        assert all(f"USER-{n}" in child for n in range(3, 9)), child
        assert "检查历史" in child and "delegate" not in child, child
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
