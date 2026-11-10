"""子代理继承父会话权限规则与沙箱策略。"""

from __future__ import annotations

import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from wire import first_event


def test_子代理继承父会话写入拒绝规则(sandbox, serve_session) -> None:
    requests: list[dict] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            number = len(requests)
            if number == 1:
                delta = {"tool_calls": [{"index": 0, "id": "worker-call", "type": "function",
                    "function": {"name": "call_worker", "arguments": '{"prompt":"尝试写入"}'}}]}
                finish = "tool_calls"
            elif number == 2:
                delta = {"tool_calls": [{"index": 0, "id": "write-call", "type": "function",
                    "function": {"name": "write", "arguments": json.dumps({
                        "path": "denied-by-parent.txt", "content": "不应写入"
                    }, ensure_ascii=False)}}]}
                finish = "tool_calls"
            elif number == 3:
                delta, finish = {"content": "子代理已处理拒绝"}, "stop"
            else:
                delta, finish = {"content": "父代理完成"}, "stop"
            chunk = {"id": "permission-flow", "model": "gpt-4o", "choices": [{
                "index": 0, "delta": delta, "finish_reason": finish,
            }]}
            body = ("data: " + json.dumps(chunk, ensure_ascii=False)
                    + "\n\ndata: [DONE]\n\n").encode()
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
            '\n[[permissions.rule]]\ntool = "write"\naction = "deny"\n'
            '\n[[subagents]]\nname = "Worker"\nrole = "worker"\n'
            'system_prompt = "尝试写入"\nallowed_tools = ["write"]\n'
        )
        env = {
            "ZHUYE_MODEL": "openai/gpt-4o",
            "ZHUYE_HOME": str(sandbox.home / ".zhuye"),
            "ZHUYE_APPROVALS": "1",
        }
        target = sandbox.cwd / "denied-by-parent.txt"
        with serve_session(env=env) as session:
            parent = session.send({"type": "user_input", "items": [
                {"type": "text", "text": "让 worker 写入"}
            ]})
            delegation = session.wait_event(parent, "approval_request", timeout=5)["msg"]
            assert delegation["kind"]["tool_name"] == "call_worker", delegation
            session.send({"type": "tool_approval", "id": delegation["request_id"], "decision": "approve"})
            deadline = time.monotonic() + 2
            child_approvals = []
            while time.monotonic() < deadline:
                child_approvals = [
                    event["msg"] for event in session.all_events()
                    if event["msg"].get("type") == "approval_request"
                    and event["msg"].get("request_id") != delegation["request_id"]
                ]
                if child_approvals:
                    break
                time.sleep(0.05)
            assert not child_approvals, child_approvals
            events = session.collect(parent, timeout=30)
            end = first_event(events, "tool_call_end")
            assert end is not None and end["msg"]["call_id"] == "worker-call", events
            assert end["msg"]["is_error"] is False, end
            assert end["msg"]["output"]["content"] == [{
                "type": "text", "text": "子代理已处理拒绝"
            }]
            assert first_event(events, "turn_complete")["msg"]["status"] == "success", events
            status = first_event(session.submit({"type": "query_subagents"}), "subagent_status")["msg"]["children"]
            assert len(status) == 1 and status[0]["state"] == "completed", status
        assert not target.exists(), "子代理绕过了父会话的 write 拒绝规则"
        assert len(requests) == 4, requests
        followup = json.dumps(requests[2]["messages"], ensure_ascii=False)
        assert "write-call" in followup and "denied by rule for write" in followup, followup
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()


def test_子代理继承父级放行但仍受沙箱边界限制(sandbox, serve_session) -> None:
    """父级允许写入只扩大审批,不扩大子代理的文件系统沙箱。"""
    requests: list[dict] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            number = len(requests)
            if number == 1:
                delta = {"tool_calls": [{"index": 0, "id": "worker-call", "type": "function",
                    "function": {"name": "call_worker", "arguments": '{"prompt":"尝试写入"}'}}]}
                finish = "tool_calls"
            elif number == 2:
                delta = {"tool_calls": [{"index": 0, "id": "outside-call", "type": "function",
                    "function": {"name": "write", "arguments": json.dumps({
                        "path": "../outside-child.txt", "content": "不应写入"
                    }, ensure_ascii=False)}}]}
                finish = "tool_calls"
            elif number == 3:
                delta = {"tool_calls": [{"index": 0, "id": "inside-call", "type": "function",
                    "function": {"name": "write", "arguments": json.dumps({
                        "path": "allowed-child.txt", "content": "子代理可写入"
                    }, ensure_ascii=False)}}]}
                finish = "tool_calls"
            elif number == 4:
                delta, finish = {"content": "子代理完成写入"}, "stop"
            else:
                delta, finish = {"content": "父代理完成"}, "stop"
            chunk = {"id": "sandbox-flow", "model": "gpt-4o", "choices": [{
                "index": 0, "delta": delta, "finish_reason": finish,
            }]}
            body = ("data: " + json.dumps(chunk, ensure_ascii=False)
                    + "\n\ndata: [DONE]\n\n").encode()
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
            '\n[[permissions.rule]]\ntool = "write"\naction = "allow"\n'
            '\n[[subagents]]\nname = "Worker"\nrole = "worker"\n'
            'system_prompt = "尝试写入"\nallowed_tools = ["write"]\n'
        )
        env = {
            "ZHUYE_MODEL": "openai/gpt-4o",
            "ZHUYE_HOME": str(sandbox.home / ".zhuye"),
            "ZHUYE_APPROVALS": "1",
        }
        outside = sandbox.cwd.parent / "outside-child.txt"
        inside = sandbox.cwd / "allowed-child.txt"
        with serve_session(env=env) as session:
            parent = session.send({"type": "user_input", "items": [
                {"type": "text", "text": "让 worker 写入"}
            ]})
            delegation = session.wait_event(parent, "approval_request", timeout=5)["msg"]
            assert delegation["kind"]["tool_name"] == "call_worker", delegation
            session.send({"type": "tool_approval", "id": delegation["request_id"], "decision": "approve"})
            deadline = time.monotonic() + 2
            extra_approvals = []
            while time.monotonic() < deadline:
                extra_approvals = [
                    event["msg"] for event in session.all_events()
                    if event["msg"].get("type") == "approval_request"
                    and event["msg"].get("request_id") != delegation["request_id"]
                ]
                if extra_approvals:
                    break
                time.sleep(0.05)
            assert not extra_approvals, extra_approvals
            events = session.collect(parent, timeout=30)
            end = first_event(events, "tool_call_end")
            assert end is not None and end["msg"]["call_id"] == "worker-call", events
            assert end["msg"]["is_error"] is False, end
            assert end["msg"]["output"]["content"] == [{
                "type": "text", "text": "子代理完成写入"
            }]
            assert first_event(events, "turn_complete")["msg"]["status"] == "success", events
        assert inside.read_text() == "子代理可写入"
        assert not outside.exists(), "子代理绕过了工作区沙箱"
        assert len(requests) == 5, requests
        outside_history = json.dumps(requests[2]["messages"], ensure_ascii=False)
        assert "outside-call" in outside_history and "path escaped sandbox" in outside_history
        inside_history = json.dumps(requests[3]["messages"], ensure_ascii=False)
        assert "子代理可写入" in inside_history, inside_history
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
