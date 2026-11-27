"""同批工具调用的读写顺序。"""

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from wire import rollout_records


def test_同批读写按模型调用顺序执行(sandbox, serve_session):
    target = sandbox.cwd / "order.txt"
    target.write_text("修改之前", encoding="utf-8")
    requests = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self):
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            if len(requests) == 1:
                calls = [
                    ("before", "read", {"path": "order.txt"}),
                    ("write", "write", {"path": "order.txt", "content": "修改之后"}),
                    ("after", "read", {"path": "order.txt"}),
                ]
                delta = {"tool_calls": [
                    {"index": i, "id": call_id, "type": "function",
                     "function": {"name": name, "arguments": json.dumps(args, ensure_ascii=False)}}
                    for i, (call_id, name, args) in enumerate(calls)
                ]}
                finish = "tool_calls"
            else:
                delta, finish = {"content": "完成"}, "stop"
            chunk = {"id": "batch-order", "model": "gpt-4o", "choices": [
                {"index": 0, "delta": delta, "finish_reason": finish}
            ]}
            body = ("data: " + json.dumps(chunk, ensure_ascii=False)
                    + "\n\ndata: [DONE]\n\n").encode()
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
        sandbox.write_config(
            '\n[active]\nprovider = "openai"\n[openai]\napi_key = "test"\n'
            f'base_url = "http://127.0.0.1:{gateway.server_port}"\n'
            '\n[[permissions.rule]]\ntool = "write"\naction = "allow"\n'
            '\n[tool_journal]\nstrict = true\n'
        )
        with serve_session(env={"ZHUYE_MODEL": "openai/gpt-4o"}) as session:
            events = session.submit({"type": "user_input", "items": [
                {"type": "text", "text": "读、修改并再次读取"}
            ]})
        ends = {event["msg"]["call_id"]: event["msg"] for event in events
                if event["msg"].get("type") == "tool_call_end"}
        assert set(ends) == {"before", "write", "after"}, ends
        assert all(not item["is_error"] for item in ends.values()), ends
        assert "修改之前" in json.dumps(ends["before"]["output"], ensure_ascii=False)
        assert "修改之后" in json.dumps(ends["after"]["output"], ensure_ascii=False)
        assert target.read_text(encoding="utf-8") == "修改之后"
        assert any(event["msg"].get("type") == "turn_complete" for event in events), events
        records = [record for record in rollout_records(sandbox.home)
                   if record.get("type") == "tool_execution"]
        for call_id in ("before", "write", "after"):
            selected = [record for record in records if record["call_id"] == call_id]
            assert [record["phase"] for record in selected] == ["started", "completed"], selected
            assert all(record["session_id"] and record["turn_id"] for record in selected), selected
            assert selected[0]["session_id"] == selected[1]["session_id"]
            assert selected[0]["turn_id"] == selected[1]["turn_id"]
            assert selected[1]["is_error"] is False
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
