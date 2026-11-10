"""同角色子代理并发执行的成功与失败混合场景。"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from wire import first_event, rollout_files


def test_并行子代理混合成功失败并汇总给父代理(sandbox, serve_session) -> None:
    requests: list[dict] = []
    slow_started = threading.Event()
    failure_started = threading.Event()
    release_slow = threading.Event()

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append(request)
            request_text = json.dumps(request, ensure_ascii=False)
            if len(requests) == 1:
                delta = {"tool_calls": [
                    {"index": 0, "id": "parallel-good", "type": "function", "function": {
                        "name": "call_explorer", "arguments": '{"prompt":"慢速探索"}'}},
                    {"index": 1, "id": "parallel-bad", "type": "function", "function": {
                        "name": "call_explorer", "arguments": '{"prompt":"快速失败"}'}},
                ]}
                chunks = [{"id": "parent", "model": "gpt-4o", "choices": [{
                    "index": 0, "delta": delta, "finish_reason": "tool_calls"}]}]
            elif "parallel-good" in request_text:
                chunks = [{"id": "parent-final", "model": "gpt-4o", "choices": [{
                    "index": 0, "delta": {"content": "已汇总成功与失败"}, "finish_reason": "stop"}]}]
            elif "快速失败" in request_text:
                failure_started.set()
                body = json.dumps({"error": {"message": "子代理请求被拒绝"}},
                                  ensure_ascii=False).encode()
                self.send_response(400)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            elif "慢速探索" in request_text:
                slow_started.set()
                if not release_slow.wait(timeout=15):
                    self.send_response(500)
                    self.end_headers()
                    return
                chunks = [{"id": "slow-child", "model": "gpt-4o", "choices": [{
                    "index": 0, "delta": {"content": "慢速结果"}, "finish_reason": "stop"}]}]
            else:
                chunks = [{"id": "parent-final", "model": "gpt-4o", "choices": [{
                    "index": 0, "delta": {"content": "已汇总成功与失败"}, "finish_reason": "stop"}]}]
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
            parent = session.send({"type": "user_input", "items": [
                {"type": "text", "text": "并行探索"}
            ], "thread_settings": {"max_tool_concurrency": 2}})
            assert slow_started.wait(timeout=10), "慢速子代理未启动"
            assert failure_started.wait(timeout=10), "失败子代理未启动"
            running = first_event(session.submit({"type": "query_subagents"}), "subagent_status")["msg"]
            assert len(running["children"]) == 2, running
            assert all(child["role"] == "explorer" for child in running["children"]), running
            assert any(child["state"] == "running" for child in running["children"]), running
            release_slow.set()
            events = session.collect(parent, timeout=30)
            ends = {event["msg"]["call_id"]: event["msg"] for event in events
                    if event["msg"]["type"] == "tool_call_end"}
            assert set(ends) == {"parallel-good", "parallel-bad"}, ends
            assert ends["parallel-good"]["is_error"] is False, ends
            assert ends["parallel-good"]["output"]["content"] == [
                {"type": "text", "text": "慢速结果"}
            ], ends["parallel-good"]
            assert ends["parallel-bad"]["is_error"] is True, ends
            assert "INVALID_REQUEST" in str(ends["parallel-bad"]["output"]), ends["parallel-bad"]
            assert "子代理请求被拒绝" in str(ends["parallel-bad"]["output"]), ends["parallel-bad"]
            terminal = first_event(events, "turn_complete")
            errors = [event["msg"] for event in events if event["msg"]["type"] in {"error", "stream_error"}]
            assert terminal is not None and terminal["msg"]["status"] == "success", errors
            assert "".join(event["msg"]["delta"] for event in events
                           if event["msg"]["type"] == "agent_message_delta") == "已汇总成功与失败"

            finished = first_event(session.submit({"type": "query_subagents"}), "subagent_status")["msg"]["children"]
            assert len(finished) == 2, finished
            assert {child["state"] for child in finished} == {"completed", "failed"}, finished
            assert all(child["finished_at"] for child in finished), finished
        assert len(requests) == 4, requests
        final_messages = requests[-1]["messages"]
        tool_results = {message["tool_call_id"]: message["content"]
                        for message in final_messages if message.get("role") == "tool"}
        assert tool_results["parallel-good"] == "慢速结果", tool_results
        assert "子代理请求被拒绝" in tool_results["parallel-bad"], tool_results
        records = [json.loads(line) for line in rollout_files(sandbox.home)[-1].read_text().splitlines() if line]
        assistant = next(record for record in records
                         if record.get("role") == "assistant" and "parallel-good" in str(record.get("content")))
        block_types = [block["type"] for block in assistant["content"]]
        assert block_types == ["tool_use", "tool_use", "tool_result", "tool_result", "text"], assistant
    finally:
        release_slow.set()
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
