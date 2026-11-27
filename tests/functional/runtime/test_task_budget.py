"""任务树预算经配置和真实协议接入。"""

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def _input():
    return {"type": "user_input", "items": [{"type": "text", "text": "执行任务"}]}


def test_token预算不足在模型调用前拒绝(sandbox, serve_session, mock_script):
    sandbox.write_config("\n[task_budget]\ntotal_tokens = 1\n")
    script = mock_script(['{"type":"text","text":"不应出现"}'])
    with serve_session(env=sandbox.mock_env(script)) as session:
        events = session.submit(_input())
    error = next(e["msg"] for e in events if e["msg"]["type"] == "error")
    assert error["code"] == "task_budget_exceeded"
    assert error["details"]["reserved_tokens"] == 0
    assert error["details"]["actual_tokens"] == 0
    snapshot = next(e["msg"] for e in events if e["msg"]["type"] == "task_budget_snapshot")
    assert snapshot["task_id"] and snapshot["turn_id"]
    assert snapshot["actual_tokens"] == 0
    assert snapshot["reserved_tokens"] == 0
    assert snapshot["unknown_estimated_tokens"] == 0
    assert snapshot["dispatched_subagents"] == 0
    assert next(e["msg"] for e in events if e["msg"]["type"] == "turn_complete")["status"] == "token_budget_exceeded"
    assert "不应出现" not in json.dumps(events, ensure_ascii=False)


def test_子代理派发次数超限时返回工具错误(sandbox, serve_session, mock_script):
    sandbox.write_config('\n[task_budget]\nsubagent_dispatches = 0\n'
                         '\n[[subagents]]\nname = "Worker"\nrole = "worker"\n'
                         'system_prompt = "处理任务"\nallowed_tools = []\n')
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "call_worker", "args": {"prompt": "处理"}}),
        '{"type":"text","text":"派发被拒绝"}',
    ])
    with serve_session(env=sandbox.mock_env(script)) as session:
        events = session.submit(_input())
    end = next(e["msg"] for e in events if e["msg"]["type"] == "tool_call_end")
    assert end["is_error"] is True
    assert "预算" in json.dumps(end["output"], ensure_ascii=False)
    snapshot = next(e["msg"] for e in events if e["msg"]["type"] == "task_budget_snapshot")
    assert snapshot["actual_tokens"] == 8
    assert snapshot["dispatched_subagents"] == 0
    assert snapshot["reserved_tokens"] == 0
    assert next(e["msg"] for e in events if e["msg"]["type"] == "turn_complete")["status"] == "success"
    assert not any(e["msg"]["type"] == "subagent_progress" for e in events)


def test_主循环模型重试分别结算失败和成功请求(sandbox, serve_session):
    requests = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self):
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            if len(requests) == 1:
                body = b"data: {incomplete\n\n"
            else:
                chunk = {"id": "retry", "model": "gpt-4o",
                         "choices": [{"index": 0, "delta": {"content": "重试成功"},
                                      "finish_reason": "stop"}],
                         "usage": {"prompt_tokens": 12, "completion_tokens": 3,
                                   "total_tokens": 15}}
                body = ("data: " + json.dumps(chunk) + "\n\ndata: [DONE]\n\n").encode()
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
                             '\n[task_budget]\ntotal_tokens = 100000\n')
        with serve_session(env={"ZHUYE_MODEL": "openai/gpt-4o"}) as session:
            events = session.submit(_input())
        assert len(requests) == 2
        retry = next(e["msg"] for e in events if e["msg"]["type"] == "stream_error")
        assert retry["code"] == "SSE_PARSE_ERROR"
        snapshot = next(e["msg"] for e in events if e["msg"]["type"] == "task_budget_snapshot")
        assert snapshot["actual_tokens"] == 15
        assert snapshot["reserved_tokens"] == 0
        assert snapshot["unknown_calls"] == 1
        assert snapshot["unknown_estimated_tokens"] > 0
        assert snapshot["estimated_tokens"] == 2 * snapshot["unknown_estimated_tokens"]
        complete = next(e["msg"] for e in events if e["msg"]["type"] == "turn_complete")
        assert complete["status"] == "success"
        assert complete["usage"]["total_tokens"] == 15
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
