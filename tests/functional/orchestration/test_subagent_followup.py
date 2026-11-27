"""主代理按子会话标识继续派工并保留上下文。"""

import json
import re
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from wire import first_event


def test_主代理在后续回合续派已完成子代理(sandbox, serve_session):
    requests = []
    child_ids = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append(request)
            number = len(requests)
            if number == 1:
                delta = {"tool_calls": [{"index": 0, "id": "first", "type": "function", "function": {
                    "name": "call_explorer", "arguments": json.dumps({"prompt": "记住代号北极星", "context_tail": 2})}}]}
                finish = "tool_calls"
            elif number == 2:
                delta, finish = {"content": '{"result":"已记住北极星"}'}, "stop"
            elif number == 3:
                history = json.dumps(request["messages"], ensure_ascii=False)
                match = re.search(r"call_explorer: ([0-9a-f-]{36})", history)
                assert match, history
                child_ids.append(match.group(1))
                delta, finish = {"content": "子代理首轮已完成"}, "stop"
            elif number == 4:
                delta = {"tool_calls": [{"index": 0, "id": "again", "type": "function", "function": {
                    "name": "call_explorer", "arguments": json.dumps({"prompt": "继续处理之前的代号", "session_id": child_ids[0]})}}]}
                finish = "tool_calls"
            else:
                delta, finish = {"content": "子代理继续完成" if number == 5 else "父代理收到续派结果"}, "stop"
            chunk = {"id": "test", "model": "gpt-4o", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
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
        env = {"ZHUYE_MODEL": "openai/gpt-4o", "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            first = session.submit({"type": "user_input", "items": [{"type": "text", "text": "派工"}]})
            second = session.submit({"type": "user_input", "items": [{"type": "text", "text": "继续派工"}]})
            first_end = first_event(first, "tool_call_end")["msg"]
            second_end = first_event(second, "tool_call_end")["msg"]
            assert not first_end["is_error"] and not second_end["is_error"]
            assert first_end["output"]["metadata"]["session_id"] == child_ids[0]
            assert second_end["output"]["metadata"]["session_id"] == child_ids[0]
            assert first_end["output"]["content"][0]["text"] == '{"result":"已记住北极星"}'
            assert first_event(second, "turn_complete")["msg"]["status"] == "success"
        assert len(requests) == 6
        child_history = json.dumps(requests[4]["messages"], ensure_ascii=False)
        assert "记住代号北极星" in child_history and "已记住北极星" in child_history
        assert "派工" in child_history
        assert "继续处理之前的代号" in child_history
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
