"""子代理工具审批必须转发给父客户端。"""

from __future__ import annotations

import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from wire import first_event


@pytest.mark.parametrize("approved", [True, False], ids=["批准写入", "拒绝写入"])
def test_子代理写入审批经父会话执行(sandbox, serve_session, approved) -> None:
    requests: list[dict] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            number = len(requests)
            if number == 1:
                delta = {"tool_calls": [{"index": 0, "id": "worker-call", "type": "function",
                    "function": {"name": "call_worker", "arguments": '{"prompt":"写入结果"}'}}]}
                finish = "tool_calls"
            elif number == 2:
                delta = {"tool_calls": [{"index": 0, "id": "write-call", "type": "function",
                    "function": {"name": "write", "arguments": json.dumps({
                        "path": "child-output.txt", "content": "子代理写入内容"
                    }, ensure_ascii=False)}}]}
                finish = "tool_calls"
            elif number == 3:
                delta = {"content": "子代理已完成" if approved else "子代理处理了拒绝"}
                finish = "stop"
            else:
                delta = {"content": "父代理完成"}
                finish = "stop"
            chunk = {"id": "approval-flow", "model": "gpt-4o", "choices": [{
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
            '\n[[subagents]]\nname = "Worker"\nrole = "worker"\n'
            'system_prompt = "执行写入任务"\nallowed_tools = ["write"]\n'
        )
        env = {
            "ZHUYE_MODEL": "openai/gpt-4o",
            "ZHUYE_HOME": str(sandbox.home / ".zhuye"),
            "ZHUYE_APPROVALS": "1",
        }
        target = sandbox.cwd / "child-output.txt"
        with serve_session(env=env) as session:
            parent = session.send({"type": "user_input", "items": [
                {"type": "text", "text": "让 worker 写入文件"}
            ]})
            delegation = session.wait_event(parent, "approval_request", timeout=5)["msg"]
            assert delegation["kind"]["tool_name"] == "call_worker", delegation
            session.send({"type": "tool_approval", "id": delegation["request_id"], "decision": "approve"})
            deadline = time.monotonic() + 5
            approval = None
            while time.monotonic() < deadline and approval is None:
                approval = next((event["msg"] for event in session.all_events()
                                 if event["id"] == parent and event["msg"].get("type") == "approval_request"
                                 and event["msg"].get("request_id") != delegation["request_id"]), None)
                if approval is None:
                    time.sleep(0.05)
            approvals = [event for event in session.all_events()
                         if event["msg"].get("type") == "approval_request"]
            statuses = first_event(session.submit({"type": "query_subagents"}), "subagent_status")["msg"]
            assert approval is not None, {"approvals": approvals, "requests": requests, "statuses": statuses}
            assert approval["kind"]["tool_name"] == "write", approval
            assert approval["kind"]["args"] == {
                "path": "child-output.txt", "content": "子代理写入内容",
            }, approval
            assert not target.exists(), "审批前发生了文件写入"
            decision = "approve" if approved else {"deny": {"reason": "用户拒绝子代理写入"}}
            session.send({"type": "tool_approval", "id": approval["request_id"], "decision": decision})
            events = session.collect(parent, timeout=30)
            end = first_event(events, "tool_call_end")
            assert end is not None and end["msg"]["call_id"] == "worker-call", events
            assert end["msg"]["is_error"] is False, end
            assert end["msg"]["output"]["content"] == [{
                "type": "text", "text": "子代理已完成" if approved else "子代理处理了拒绝",
            }]
            assert first_event(events, "turn_complete")["msg"]["status"] == "success", events
            status = first_event(session.submit({"type": "query_subagents"}), "subagent_status")["msg"]["children"]
            assert len(status) == 1 and status[0]["state"] == "completed", status
        if approved:
            assert target.read_text() == "子代理写入内容"
        else:
            assert not target.exists(), "拒绝后子代理仍写入文件"
        assert len(requests) == 4, requests
        child_followup = json.dumps(requests[2]["messages"], ensure_ascii=False)
        assert "write-call" in child_followup, requests[2]
        if approved:
            assert "子代理写入内容" in child_followup and "is_error" not in child_followup, child_followup
        else:
            assert "用户拒绝子代理写入" in child_followup, child_followup
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
