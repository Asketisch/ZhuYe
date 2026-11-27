"""工具在途崩溃后恢复历史与工具配对的端到端回归。"""

import json
from collections import Counter
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from wire import first_event, rollout_records, rollout_thread_id


@pytest.mark.parametrize("crash_point", ["before_start", "executing", "after_effect"])
def test_工具不同执行阶段强杀后恢复已提交历史(sandbox, serve_session, crash_point):
    requests = []
    side_effect = sandbox.cwd / "外部工具产物.txt"

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self):
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            if len(requests) == 2:
                delta = {"tool_calls": [{"index": 0, "id": "pending-call", "type": "function",
                                         "function": {"name": "remote_slow", "arguments": "{}"}}]}
                finish = "tool_calls"
            else:
                delta = {"content": "稳定检查点" if len(requests) == 1 else "恢复完成"}
                finish = "stop"
            chunk = {"id": "recovery", "model": "gpt-4o", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
            body = ("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\ndata: [DONE]\n\n").encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args):
            pass

    gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
    thread = threading.Thread(target=gateway.serve_forever, daemon=True)
    thread.start()
    try:
        sandbox.write_config('\n[active]\nprovider = "openai"\n[openai]\napi_key = "test"\n'
                             f'base_url = "http://127.0.0.1:{gateway.server_port}"\n'
                             '\n[tool_journal]\nstrict = true\n'
                             '\n[[permissions.rule]]\ntool = "remote_slow"\naction = "ask"\n')
        env = {"ZHUYE_MODEL": "openai/gpt-4o", "ZHUYE_APPROVALS": "1"}
        with serve_session(env=env) as session:
            first = session.submit({"type": "user_input", "items": [{"type": "text", "text": "记住第一轮"}]})
            assert first_event(first, "turn_complete")["msg"]["status"] == "success"
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                if "稳定检查点" in json.dumps(rollout_records(sandbox.home), ensure_ascii=False):
                    break
                time.sleep(.05)
            else:
                raise AssertionError("首轮检查点未落盘")
            tid = rollout_thread_id(sandbox.home)
            session.send({"type": "register_tools", "tools": [{"name": "remote_slow", "description": "等待",
                          "parameters": {"type": "object"}}]})
            sid = session.send({"type": "user_input", "items": [{"type": "text", "text": "调用等待工具"}]})
            approval = session.wait_event(sid, "approval_request", timeout=20)["msg"]
            if crash_point != "before_start":
                session.send({"type": "tool_approval", "id": approval["request_id"], "decision": "approve"})
                session.wait_global(lambda event: event.get("msg", {}).get("type") == "tool_execution_request", timeout=20)
            if crash_point == "after_effect":
                # 模拟宿主工具已产生副作用,但尚未发送 ToolExecutionResponse。
                side_effect.write_text("只执行一次\n")
            started_turn = next(event["msg"]["turn_id"] for event in session.all_events()
                                if event.get("id") == sid and event.get("msg", {}).get("type") == "turn_started")
            session.kill()
            session._proc.wait(timeout=5)
            assert not [event for event in session.all_events() if event.get("id") == sid
                        and event.get("msg", {}).get("type") in {"turn_complete", "turn_aborted"}]
        phases = [record["phase"] for record in rollout_records(sandbox.home)
                  if record.get("type") == "tool_execution" and record.get("call_id") == "pending-call"]
        assert phases == ([] if crash_point == "before_start" else ["started"])
        with serve_session(args=["--resume", tid], env=env) as resumed:
            last = resumed.submit({"type": "user_input", "items": [{"type": "text", "text": "恢复后继续"}]})
            assert first_event(last, "turn_complete")["msg"]["status"] == "success"
            uncertain = first_event(last, "tool_execution_uncertain")
            if crash_point == "before_start":
                assert uncertain is None, last
            else:
                assert uncertain is not None, last
                assert uncertain["msg"]["session_id"] == tid
                assert uncertain["msg"]["call_id"] == "pending-call"
                assert uncertain["msg"]["tool_name"] == "remote_slow"
                assert uncertain["msg"]["turn_id"] == started_turn
            assert not any(event["msg"].get("type") == "tool_execution_request" for event in last)
        if crash_point == "after_effect":
            assert side_effect.read_text() == "只执行一次\n"
        else:
            assert not side_effect.exists()
        history = requests[-1]["messages"]
        serialized = json.dumps(history, ensure_ascii=False)
        assert "记住第一轮" in serialized and "稳定检查点" in serialized
        call_ids = [call["id"] for message in history for call in message.get("tool_calls", [])]
        result_ids = [message.get("tool_call_id") for message in history if message.get("role") == "tool"]
        assert Counter(call_ids) == Counter(result_ids)
        assert all(count == 1 for count in Counter(call_ids).values())
        pending = [message for message in history if message.get("tool_call_id") == "pending-call"]
        assert len(pending) == 1, history
        assert "结果未知" in json.dumps(pending[0], ensure_ascii=False), pending
        assert "可能已产生副作用" in json.dumps(pending[0], ensure_ascii=False), pending
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()


def test_rewind完成后强杀并恢复保留截断历史(sandbox, serve_session):
    requests = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self):
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            text = ["保留回复", "移除回复", "恢复回复"][min(len(requests) - 1, 2)]
            chunk = {"id": "rewind", "model": "gpt-4o", "choices": [{"index": 0,
                     "delta": {"content": text}, "finish_reason": "stop"}]}
            body = ("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\ndata: [DONE]\n\n").encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args):
            pass

    gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
    thread = threading.Thread(target=gateway.serve_forever, daemon=True)
    thread.start()
    try:
        sandbox.write_config('\n[active]\nprovider = "openai"\n[openai]\napi_key = "test"\n'
                             f'base_url = "http://127.0.0.1:{gateway.server_port}"\n')
        env = {"ZHUYE_MODEL": "openai/gpt-4o"}
        with serve_session(env=env) as session:
            for text in ("保留输入", "移除输入"):
                events = session.submit({"type": "user_input", "items": [{"type": "text", "text": text}]})
                assert first_event(events, "turn_complete")["msg"]["status"] == "success"
            tid = rollout_thread_id(sandbox.home)
            rewound = session.submit({"type": "rewind"})
            assert first_event(rewound, "turn_rewound") is not None, rewound
            session.kill()
            session._proc.wait(timeout=5)
        with serve_session(args=["--resume", tid], env=env) as resumed:
            events = resumed.submit({"type": "user_input", "items": [{"type": "text", "text": "恢复后继续"}]})
            assert first_event(events, "turn_complete")["msg"]["status"] == "success"
        history = json.dumps(requests[-1]["messages"], ensure_ascii=False)
        assert "保留输入" in history and "保留回复" in history
        assert "移除输入" not in history and "移除回复" not in history
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()


def test_工具结束后强杀并恢复成对历史(sandbox, serve_session):
    requests = []
    blocked = threading.Event()
    release = threading.Event()

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self):
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            number = len(requests)
            if number == 1:
                delta = {"tool_calls": [{"index": 0, "id": "stored-call", "type": "function",
                                         "function": {"name": "remote_saved", "arguments": "{}"}}]}
                finish = "tool_calls"
            else:
                if number == 2:
                    blocked.set()
                    release.wait(timeout=15)
                delta, finish = {"content": "恢复成功"}, "stop"
            chunk = {"id": "recovery", "model": "gpt-4o", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
            body = ("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\ndata: [DONE]\n\n").encode()
            try:
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                pass

        def log_message(self, *_args):
            pass

    gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
    thread = threading.Thread(target=gateway.serve_forever, daemon=True)
    thread.start()
    try:
        sandbox.write_config('\n[active]\nprovider = "openai"\n[openai]\napi_key = "test"\n'
                             f'base_url = "http://127.0.0.1:{gateway.server_port}"\n')
        env = {"ZHUYE_MODEL": "openai/gpt-4o"}
        with serve_session(env=env) as session:
            session.send({"type": "register_tools", "tools": [{"name": "remote_saved", "description": "查询",
                          "parameters": {"type": "object"}}]})
            turn = session.send({"type": "user_input", "items": [{"type": "text", "text": "执行查询"}]})
            request = session.wait_global(lambda event: event.get("msg", {}).get("type") == "tool_execution_request")
            side_effect = sandbox.cwd / "effect.txt"
            side_effect.write_text("只执行一次")
            session.send({"type": "tool_execution_response", "call_id": request["msg"]["call_id"], "output": {
                "content": [{"type": "text", "text": "已保存回执"}], "is_error": False,
                "metadata": {}, "elapsed_ms": 0}})
            assert session.wait_event(turn, "tool_call_end")["msg"]["is_error"] is False
            assert blocked.wait(timeout=15), "模型后续请求未到达"
            tid = rollout_thread_id(sandbox.home)
            session.kill()
            session._proc.wait(timeout=5)
        release.set()
        with serve_session(args=["--resume", tid], env=env) as resumed:
            events = resumed.submit({"type": "user_input", "items": [{"type": "text", "text": "继续"}]})
            assert first_event(events, "turn_complete")["msg"]["status"] == "success"
        history = requests[-1]["messages"]
        call_ids = [call["id"] for message in history for call in message.get("tool_calls", [])]
        result_ids = [message.get("tool_call_id") for message in history if message.get("role") == "tool"]
        assert Counter(call_ids) == Counter(result_ids)
        assert all(count == 1 for count in Counter(call_ids).values())
        assert side_effect.read_text() == "只执行一次"
    finally:
        release.set()
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
