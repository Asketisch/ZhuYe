"""多级委派成功与失败的结果传播。"""

from __future__ import annotations

import json
import select
import socket
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from wire import first_event, rollout_files


@pytest.mark.parametrize("review_fails", [False, True], ids=["孙代理成功", "孙代理部分输出后失败"])
def test_多级委派结果与状态逐级传播(sandbox, serve_session, review_fails) -> None:
    requests: list[dict] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            number = len(requests)
            if number == 1:
                delta = {"tool_calls": [{"index": 0, "id": "explore-call", "type": "function",
                    "function": {"name": "call_explorer", "arguments": '{"prompt":"探索项目"}'}}]}
                chunks = [{"id": "root", "model": "gpt-4o", "choices": [{
                    "index": 0, "delta": delta, "finish_reason": "tool_calls"}]}]
            elif number == 2:
                delta = {"content": "开始审阅。", "tool_calls": [{"index": 0, "id": "review-call", "type": "function",
                    "function": {"name": "call_reviewer", "arguments": '{"prompt":"审阅发现"}'}}]}
                chunks = [{"id": "child", "model": "gpt-4o", "choices": [{
                    "index": 0, "delta": delta, "finish_reason": "tool_calls"}]}]
            elif number == 3 and review_fails:
                chunks = [
                    {"id": "grandchild", "model": "gpt-4o", "choices": [{
                        "index": 0, "delta": {"content": "不完整的审阅"}, "finish_reason": None}]},
                    {"error": {"type": "invalid_request_error", "message": "审阅模型失败"}},
                ]
            else:
                text = {3: "审阅完成", 4: "探索已处理审阅", 5: "父代理完成"}[number]
                chunks = [{"id": "agent", "model": "gpt-4o", "choices": [{
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
            '\n[[subagents]]\nname = "Explorer"\nrole = "explorer"\n'
            'system_prompt = "探索代码"\nallowed_tools = ["call_reviewer"]\n'
            '\n[[subagents]]\nname = "Reviewer"\nrole = "reviewer"\n'
            'system_prompt = "审阅代码"\nallowed_tools = []\n'
        )
        env = {"ZHUYE_MODEL": "openai/gpt-4o", "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            events = session.submit({"type": "user_input", "items": [
                {"type": "text", "text": "探索并审阅"}
            ]}, timeout=30)
            end = first_event(events, "tool_call_end")
            assert end is not None and end["msg"]["call_id"] == "explore-call", events
            assert end["msg"]["is_error"] is False, end
            assert end["msg"]["output"]["content"] == [{"type": "text", "text": "探索已处理审阅"}]
            assert first_event(events, "turn_complete")["msg"]["status"] == "success", events
            progress = [e["msg"] for e in events if e["msg"]["type"] == "subagent_progress"]
            assert any(p["kind"] == "message" and p["text"] == "开始审阅。" for p in progress), progress
            assert any(p["kind"] == "tool_begin" and p["call_id"] == "review-call" for p in progress), progress
            assert any(p["kind"] == "tool_end" and p["call_id"] == "review-call"
                       and ("failed" in p["text"]) == review_fails for p in progress), progress
            statuses = first_event(session.submit({"type": "query_subagents"}), "subagent_status")["msg"]["children"]
            by_role = {entry["role"]: entry for entry in statuses}
            assert set(by_role) == {"explorer", "reviewer"}, statuses
            assert by_role["explorer"]["state"] == "completed", statuses
            assert by_role["reviewer"]["state"] == ("failed" if review_fails else "completed"), statuses
            assert all(entry["finished_at"] for entry in statuses)
        assert len(requests) == 5, requests
        review_result = next(m for m in requests[3]["messages"]
                             if m.get("role") == "tool" and m.get("tool_call_id") == "review-call")
        if review_fails:
            assert "PROVIDER_ERROR" in review_result["content"] and "审阅模型失败" in review_result["content"]
            assert "不完整的审阅" not in review_result["content"]
        else:
            assert review_result["content"] == "审阅完成"
        parent_result = next(m for m in requests[4]["messages"]
                             if m.get("role") == "tool" and m.get("tool_call_id") == "explore-call")
        assert parent_result["content"] == "探索已处理审阅"
        records = [json.loads(line) for line in rollout_files(sandbox.home)[-1].read_text().splitlines() if line]
        assert {r["branch_name"] for r in records if r.get("type") == "fork"} >= {"explorer", "reviewer"}, records
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()


@pytest.mark.parametrize("target", ["root", "explorer", "reviewer"],
                         ids=["中断父回合", "中断子代理", "中断孙代理"])
def test_多级委派定向中断与级联取消(sandbox, serve_session, target) -> None:
    requests: list[dict] = []
    reviewer_started = threading.Event()
    reviewer_disconnected = threading.Event()
    release_reviewer = threading.Event()

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            number = len(requests)
            if number == 3:
                reviewer_started.set()
                while not release_reviewer.wait(timeout=0.05):
                    readable, _, _ = select.select([self.connection], [], [], 0)
                    try:
                        closed = bool(readable) and not self.connection.recv(1, socket.MSG_PEEK)
                    except ConnectionResetError:
                        closed = True
                    if closed:
                        reviewer_disconnected.set()
                        break
                release_reviewer.wait(timeout=15)
                return
            if number == 1:
                delta = {"tool_calls": [{"index": 0, "id": "explore-call", "type": "function",
                    "function": {"name": "call_explorer", "arguments": '{"prompt":"探索项目"}'}}]}
                finish = "tool_calls"
            elif number == 2:
                delta = {"tool_calls": [{"index": 0, "id": "review-call", "type": "function",
                    "function": {"name": "call_reviewer", "arguments": '{"prompt":"审阅发现"}'}}]}
                finish = "tool_calls"
            else:
                text = "探索已处理取消" if target == "reviewer" and number == 4 else "父代理继续"
                delta, finish = {"content": text}, "stop"
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
        sandbox.write_config(
            '\n[active]\nprovider = "openai"\n[openai]\napi_key = "test"\n'
            f'base_url = "http://127.0.0.1:{gateway.server_port}"\n'
            '\n[[subagents]]\nname = "Explorer"\nrole = "explorer"\n'
            'system_prompt = "探索代码"\nallowed_tools = ["call_reviewer"]\n'
            '\n[[subagents]]\nname = "Reviewer"\nrole = "reviewer"\n'
            'system_prompt = "审阅代码"\nallowed_tools = []\n'
        )
        env = {"ZHUYE_MODEL": "openai/gpt-4o", "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            parent = session.send({"type": "user_input", "items": [
                {"type": "text", "text": "探索并审阅"}
            ]})
            assert reviewer_started.wait(timeout=10), "孙代理未发起模型请求"
            statuses = first_event(session.submit({"type": "query_subagents"}), "subagent_status")["msg"]["children"]
            before = {entry["role"]: entry for entry in statuses}
            assert set(before) == {"explorer", "reviewer"} and all(
                entry["state"] == "running" for entry in statuses
            ), statuses
            op = {"type": "interrupt"}
            if target != "root":
                op["child_id"] = before[target]["child_id"]
            session.submit(op)
            events = session.collect(parent, timeout=10)
            end = first_event(events, "tool_call_end")
            assert end is not None and end["msg"]["call_id"] == "explore-call", events
            if target == "root":
                assert first_event(events, "turn_aborted")["msg"]["reason"]["type"] == "user_interrupt", events
                assert end["msg"]["is_error"] is True, end
            elif target == "explorer":
                assert first_event(events, "turn_complete")["msg"]["status"] == "success", events
                assert end["msg"]["is_error"] is True, end
            else:
                assert first_event(events, "turn_complete")["msg"]["status"] == "success", events
                assert end["msg"]["is_error"] is False, end
                assert end["msg"]["output"]["content"] == [{"type": "text", "text": "探索已处理取消"}]
            assert reviewer_disconnected.wait(timeout=10), "孙代理模型连接没有关闭"
            statuses = first_event(session.submit({"type": "query_subagents"}), "subagent_status")["msg"]["children"]
            after = {entry["role"]: entry for entry in statuses}
            assert after["reviewer"]["state"] == "cancelled", statuses
            assert after["explorer"]["state"] == ("completed" if target == "reviewer" else "cancelled"), statuses
            assert all(entry["finished_at"] for entry in statuses)
            if target == "root":
                later = session.submit({"type": "user_input", "items": [
                    {"type": "text", "text": "继续其他任务"}
                ]})
                assert first_event(later, "turn_complete")["msg"]["status"] == "success", later
    finally:
        release_reviewer.set()
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
