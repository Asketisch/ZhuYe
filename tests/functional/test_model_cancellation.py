"""模型响应头和流式输出阶段的中断与续跑。"""

from __future__ import annotations

import json
import select
import socket
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from wire import first_event, rollout_files


@pytest.mark.parametrize("streaming", [False, True], ids=["等待响应头", "已输出部分文本"])
def test_中断模型请求后关闭连接并继续(sandbox, serve_session, streaming) -> None:
    entered = threading.Event()
    disconnected = threading.Event()
    release = threading.Event()
    requests: list[dict] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            first = len(requests) == 1
            if first and streaming:
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", "100000")
                self.end_headers()
                chunk = {"id": "partial", "model": "gpt-4o", "choices": [{
                    "index": 0, "delta": {"content": "已经输出"}, "finish_reason": None,
                }]}
                self.wfile.write(("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\n").encode())
                self.wfile.flush()
            if first:
                entered.set()
                while not release.wait(timeout=0.05):
                    readable, _, _ = select.select([self.connection], [], [], 0)
                    try:
                        if readable and not self.connection.recv(1, socket.MSG_PEEK):
                            disconnected.set()
                            break
                    except ConnectionResetError:
                        disconnected.set()
                        break
                release.wait(timeout=15)
                return
            chunk = {"id": "next", "model": "gpt-4o", "choices": [{
                "index": 0, "delta": {"content": "继续完成"}, "finish_reason": "stop",
            }]}
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
        )
        env = {"ZHUYE_MODEL": "openai/gpt-4o", "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            first = session.send({"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]})
            assert entered.wait(timeout=10), "模型请求未到达"
            if streaming:
                delta = session.wait_event(first, "agent_message_delta", timeout=5)
                assert delta["msg"]["delta"] == "已经输出"
            session.submit({"type": "interrupt"})
            aborted = session.wait_event(first, "turn_aborted", timeout=5)
            assert aborted["msg"]["reason"]["type"] == "user_interrupt"
            session.wait_event(first, "submission_closed", timeout=5)
            terminals = [e for e in session.all_events() if e["id"] == first
                         and e["msg"]["type"] in ("turn_complete", "turn_aborted")]
            assert len(terminals) == 1, terminals
            assert disconnected.wait(timeout=5), "模型请求连接仍未取消"
            records = [json.loads(line) for path in rollout_files(sandbox.home)
                       for line in path.read_text().splitlines() if line]
            if streaming:
                assert any(r.get("role") == "assistant" and "已经输出" in str(r.get("content"))
                           for r in records), records
            later = session.submit({"type": "user_input", "items": [{"type": "text", "text": "第二轮"}]})
            assert first_event(later, "turn_complete")["msg"]["status"] == "success", later
            assert "".join(e["msg"]["delta"] for e in later
                           if e["msg"]["type"] == "agent_message_delta") == "继续完成"
            if streaming:
                assert "已经输出" in json.dumps(requests[-1]["messages"], ensure_ascii=False)
    finally:
        release.set()
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
