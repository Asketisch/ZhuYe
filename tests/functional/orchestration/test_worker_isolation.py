"""子代理工作区隔离与会话归属。"""

import json
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from wire import rollout_records


@pytest.mark.parametrize("repository", ["missing", "unborn", "ready"])
def test_worktree隔离按创建结果派发子代理(sandbox, serve_session, repository):
    if repository != "missing":
        subprocess.run(["git", "init", "-q", str(sandbox.cwd)], check=True)
    if repository == "ready":
        subprocess.run(["git", "-C", str(sandbox.cwd), "-c", "user.name=测试",
                        "-c", "user.email=test@example.invalid", "commit",
                        "--allow-empty", "-qm", "初始"], check=True)
    requests = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self):
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            if len(requests) == 1:
                delta = {"tool_calls": [{"index": 0, "id": "delegate", "type": "function",
                    "function": {"name": "call_worker", "arguments": '{"prompt":"处理任务"}'}}]}
                finish = "tool_calls"
            else:
                delta, finish = {"content": "完成"}, "stop"
            chunk = {"id": "worker-isolation", "model": "gpt-4o", "choices": [
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
            '\n[coordinator]\nenabled = true\n'
            '\n[[subagents]]\nname = "Worker"\nrole = "worker"\n'
            'system_prompt = "处理任务"\nallowed_tools = []\n'
        )
        with serve_session(env={"ZHUYE_MODEL": "openai/gpt-4o"}) as session:
            events = session.submit({"type": "user_input", "items": [
                {"type": "text", "text": "让 worker 处理任务"}
            ]})
        ends = [event["msg"] for event in events if event["msg"].get("type") == "tool_call_end"]
        end = next(item for item in ends if item["call_id"] == "delegate")
        child_meta = [record for record in rollout_records(sandbox.home)
                      if record.get("type") == "session_meta" and record.get("parent_session_id")]
        if repository == "ready":
            assert end["is_error"] is False, end
            assert len(requests) == 3, requests
            assert len(child_meta) == 1, child_meta
            workspace = Path(child_meta[0]["workspace"])
            assert workspace != sandbox.cwd
            assert (workspace / ".git").is_file(), workspace
        else:
            assert end["is_error"] is True, end
            assert "worktree" in json.dumps(end["output"], ensure_ascii=False), end
            if repository == "missing":
                assert "Git 根目录" in json.dumps(end["output"], ensure_ascii=False), end
            assert len(requests) == 2, requests
            assert child_meta == []
            assert not list((sandbox.cwd / ".zhuye" / "worktrees").glob("*"))
        assert any(event["msg"].get("type") == "turn_complete" for event in events), events
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
