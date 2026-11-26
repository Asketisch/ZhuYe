"""父子代理的取消和后续会话场景。"""

from __future__ import annotations

import json
import select
import socket
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from wire import first_event


@pytest.mark.parametrize("target_child", [False, True], ids=["中断父回合", "定向中断子代理"])
@pytest.mark.parametrize("child_partial", [False, True], ids=["等待响应头", "已有部分回复"])
def test_中断在途子代理并继续会话(sandbox, serve_session, target_child, child_partial) -> None:
    child_started = threading.Event()
    child_disconnected = threading.Event()
    release_child = threading.Event()
    child_replied = threading.Event()
    requests: list[dict] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append(request)
            number = len(requests)
            if number == 1:
                delta = {"tool_calls": [{"index": 0, "id": "child-call", "type": "function",
                                         "function": {"name": "call_explorer",
                                                      "arguments": '{"prompt":"深入检查"}'}}]}
                finish = "tool_calls"
            elif number == 2:
                if child_partial:
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Content-Length", "100000")
                    self.end_headers()
                    chunk = {"id": "child", "model": "gpt-4o", "choices": [{
                        "index": 0, "delta": {"content": "子代理部分回复"}, "finish_reason": None}]}
                    self.wfile.write(("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\n").encode())
                    chunk = {"id": "child", "model": "gpt-4o", "choices": [{
                        "index": 0, "delta": {"tool_calls": [{
                            "index": 0, "id": "child-pending", "type": "function",
                            "function": {"name": "read", "arguments": '{"path":'},
                        }]}, "finish_reason": None}]}
                    self.wfile.write(("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\n").encode())
                    self.wfile.flush()
                child_started.set()
                while not release_child.wait(timeout=0.05):
                    readable, _, _ = select.select([self.connection], [], [], 0)
                    try:
                        closed = bool(readable) and not self.connection.recv(1, socket.MSG_PEEK)
                    except ConnectionResetError:
                        closed = True
                    if closed:
                        child_disconnected.set()
                        break
                release_child.wait(timeout=15)
                delta = {"content": "不应被采用的迟到子代理回复"}
                finish = "stop"
            else:
                delta = {"content": "后续回合正常"}
                finish = "stop"
            chunk = {"id": "multiagent", "model": "gpt-4o", "choices": [{
                "index": 0, "delta": delta, "finish_reason": finish,
            }]}
            body = ("data: " + json.dumps(chunk, ensure_ascii=False)
                    + "\n\ndata: [DONE]\n\n").encode()
            try:
                if number != 2 or not child_partial:
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                pass
            finally:
                if "不应被采用的迟到子代理回复" in str(delta):
                    child_replied.set()

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
            parent = session.send({"type": "user_input", "items": [
                {"type": "text", "text": "委派探索"}
            ]})
            assert child_started.wait(timeout=10), "子代理未发起模型请求"
            if child_partial:
                progress = session.wait_event(parent, "subagent_progress", timeout=10)["msg"]
                assert progress["text"] == "子代理部分回复", progress
            status = first_event(session.submit({"type": "query_subagents"}), "subagent_status")["msg"]
            assert len(status["children"]) == 1, status
            child = status["children"][0]
            assert child["role"] == "explorer" and child["state"] == "running", child
            op = {"type": "interrupt", "child_id": child["child_id"]} if target_child else {"type": "interrupt"}
            session.submit(op)
            events = session.collect(parent, timeout=5)
            aborted = first_event(events, "turn_aborted")
            if target_child:
                assert aborted is None, events
                assert first_event(events, "turn_complete")["msg"]["status"] == "success", events
            else:
                assert aborted is not None and aborted["msg"]["reason"]["type"] == "user_interrupt", events
            tool_end = first_event(events, "tool_call_end")
            assert tool_end is not None and tool_end["msg"]["is_error"] is True, events
            assert "子代理部分回复" not in str(tool_end["msg"]["output"]), tool_end
            if target_child and child_partial:
                assert any(e["msg"]["type"] == "subagent_progress"
                           and e["msg"].get("text") == "子代理部分回复" for e in events), events
            assert child_disconnected.wait(timeout=5), "子代理模型连接仍未取消"
            status = first_event(session.submit({"type": "query_subagents"}), "subagent_status")["msg"]
            child_after = next(c for c in status["children"] if c["child_id"] == child["child_id"])
            assert child_after["state"] == "cancelled", child_after
            later = session.submit({"type": "user_input", "items": [
                {"type": "text", "text": "继续其他工作"}
            ]})
            assert first_event(later, "turn_complete")["msg"]["status"] == "success", later
            assert "".join(e["msg"]["delta"] for e in later
                           if e["msg"]["type"] == "agent_message_delta") == "后续回合正常"
            assert len(requests) == (4 if target_child else 3), requests
            release_child.set()
            assert child_replied.wait(timeout=5), "未完成迟到回复尝试"
            status = first_event(session.submit({"type": "query_subagents"}), "subagent_status")["msg"]
            child_after = next(c for c in status["children"] if c["child_id"] == child["child_id"])
            assert child_after["state"] == "cancelled", child_after
    finally:
        release_child.set()
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
