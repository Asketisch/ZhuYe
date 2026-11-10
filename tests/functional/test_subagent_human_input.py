"""子代理人工输入请求经父会话往返。"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from wire import first_event


def _tool_chunk(call_id: str, name: str, args: dict) -> bytes:
    chunk = {"id": "human-input", "model": "gpt-4o", "choices": [{
        "index": 0,
        "delta": {"tool_calls": [{"index": 0, "id": call_id, "type": "function",
                                  "function": {"name": name, "arguments": json.dumps(args, ensure_ascii=False)}}]},
        "finish_reason": "tool_calls",
    }]}
    return ("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\ndata: [DONE]\n\n").encode()


def _text_chunk(text: str) -> bytes:
    chunk = {"id": "human-input", "model": "gpt-4o", "choices": [{
        "index": 0, "delta": {"content": text}, "finish_reason": "stop",
    }]}
    return ("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\ndata: [DONE]\n\n").encode()


@pytest.mark.parametrize("question_kind", ["text", "choice"])
def test_子代理人工输入经父会话往返(sandbox, serve_session, question_kind) -> None:
    requests: list[dict] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            number = len(requests)
            if number == 1:
                body = _tool_chunk("worker-call", "call_worker", {"prompt": "询问用户"})
            elif number == 2 and question_kind == "text":
                body = _tool_chunk("question-call", "ask_user", {
                    "prompt": "项目代号是什么?", "placeholder": "输入代号",
                })
            elif number == 2:
                body = _tool_chunk("question-call", "ask_user_question", {
                    "questions": [{
                        "header": "代号",
                        "question": "选择项目代号",
                        "options": [
                            {"label": "北极星", "description": "主代号"},
                            {"label": "启明星", "description": "备用代号"},
                        ],
                        "multi_select": False,
                    }],
                })
            elif number == 3:
                body = _text_chunk("子代理已收到用户回答")
            else:
                body = _text_chunk("父代理完成")
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
            'system_prompt = "与用户交互"\n'
            'allowed_tools = ["ask_user", "ask_user_question"]\n'
        )
        env = {
            "ZHUYE_MODEL": "openai/gpt-4o",
            "ZHUYE_HOME": str(sandbox.home / ".zhuye"),
            "ZHUYE_APPROVALS": "1",
        }
        with serve_session(env=env) as session:
            parent = session.send({"type": "user_input", "items": [
                {"type": "text", "text": "让 worker 询问用户"}
            ]})
            delegation = session.wait_event(parent, "approval_request", timeout=5)["msg"]
            assert delegation["kind"]["tool_name"] == "call_worker", delegation
            session.send({"type": "tool_approval", "id": delegation["request_id"], "decision": "approve"})
            message_type = "ask_user_input" if question_kind == "text" else "ask_user_question"
            question = session.wait_event(parent, message_type, timeout=10)["msg"]
            if question_kind == "text":
                assert question["prompt"] == "项目代号是什么?", question
                session.send({
                    "type": "ask_user_input_response",
                    "id": question["request_id"],
                    "text": "北极星",
                })
            else:
                assert question["questions"][0]["question"] == "选择项目代号", question
                session.send({
                    "type": "ask_user_question_response",
                    "id": question["request_id"],
                    "answers": {"answers": [{"selected": [0]}]},
                })
            events = session.collect(parent, timeout=30)
            end = first_event(events, "tool_call_end")
            assert end is not None and end["msg"]["call_id"] == "worker-call", events
            assert end["msg"]["is_error"] is False, end
            assert end["msg"]["output"]["content"] == [
                {"type": "text", "text": "子代理已收到用户回答"}
            ], end
            assert first_event(events, "turn_complete")["msg"]["status"] == "success", events
            status = first_event(session.submit({"type": "query_subagents"}), "subagent_status")["msg"]["children"]
            assert len(status) == 1 and status[0]["state"] == "completed", status
        assert len(requests) == 4, requests
        followup_messages = requests[2]["messages"]
        followup = json.dumps(followup_messages, ensure_ascii=False)
        assert "question-call" in followup, followup
        if question_kind == "text":
            assert "北极星" in followup, followup
        else:
            result = next(message["content"] for message in followup_messages
                          if message.get("role") == "tool"
                          and message.get("tool_call_id") == "question-call")
            assert json.loads(result) == {"answers": [{"selected": [0]}]}, result
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
