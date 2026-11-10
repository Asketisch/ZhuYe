"""Anthropic 流内错误和凭证故障切换。"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from wire import first_event


@pytest.mark.parametrize(
    ("error_type", "error_code"),
    [("overloaded_error", "OVERLOADED"), ("rate_limit_error", "RATE_LIMITED"),
     ("unexpected_eof", None)],
)
def test_流内错误切换凭证并继续对话(sandbox, serve_session, error_type, error_code) -> None:
    requests: list[tuple[str, dict]] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            auth = self.headers.get("Authorization", "")
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append((auth, body))
            if auth == "Bearer primary-key":
                frames = [
                    ("message_start", {"type": "message_start", "message": {
                        "id": "msg-1", "model": "claude-test", "usage": {"input_tokens": 10}}}),
                ]
                if error_type != "unexpected_eof":
                    frames.append(("error", {"type": "error", "error": {
                        "type": error_type, "message": "受控流内故障"}}))
            else:
                frames = [
                    ("message_start", {"type": "message_start", "message": {
                        "id": "msg-2", "model": "claude-test", "usage": {"input_tokens": 11}}}),
                    ("content_block_start", {"type": "content_block_start", "index": 0,
                                             "content_block": {"type": "text", "text": ""}}),
                    ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                             "delta": {"type": "text_delta", "text": "备用成功"}}),
                    ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                    ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                                       "usage": {"output_tokens": 4}}),
                    ("message_stop", {"type": "message_stop"}),
                ]
            payload = "".join("event: " + event + "\ndata: " + json.dumps(data, ensure_ascii=False)
                              + "\n\n" for event, data in frames).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def log_message(self, *_args) -> None:
            pass

    gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
    thread = threading.Thread(target=gateway.serve_forever, daemon=True)
    thread.start()
    try:
        url = f"http://127.0.0.1:{gateway.server_port}"
        sandbox.write_config(
            '\n[active]\nprovider = "anthropic"\ncredential = "primary"\n'
            '[anthropic]\nmodel = "claude-test"\n'
            f'[[anthropic.credentials]]\nlabel = "primary"\napi_key = "primary-key"\nbase_url = "{url}"\n'
            f'[[anthropic.credentials]]\nlabel = "backup"\napi_key = "backup-key"\nbase_url = "{url}"\n'
        )
        env = {"ZHUYE_MODEL": "anthropic/claude-test", "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            first = session.submit({"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]})
            if error_code is not None:
                error = first_event(first, "stream_error")
                assert error is not None and error["msg"]["code"] == error_code, first
                assert error["msg"]["credential_label"] == "primary", error
            else:
                assert any(e["msg"]["type"] == "routing"
                           and e["msg"]["reason"] == "retry_same_exhausted" for e in first), first
            assert first_event(first, "turn_complete")["msg"]["status"] == "success", first
            assert "".join(e["msg"]["delta"] for e in first
                           if e["msg"]["type"] == "agent_message_delta") == "备用成功"
            second = session.submit({"type": "user_input", "items": [{"type": "text", "text": "第二轮"}]})
            assert first_event(second, "turn_complete")["msg"]["status"] == "success", second
            assert "".join(e["msg"]["delta"] for e in second
                           if e["msg"]["type"] == "agent_message_delta") == "备用成功"
        # 畸形或提前结束的流走每回合同凭证重试后切换,不设置凭证冷却。
        expected_auth = (["Bearer primary-key", "Bearer primary-key", "Bearer backup-key"] * 2
                         if error_code is None else
                         ["Bearer primary-key", "Bearer backup-key", "Bearer backup-key"])
        assert [auth for auth, _ in requests] == expected_auth
        messages = json.dumps(requests[-1][1]["messages"], ensure_ascii=False)
        assert "第一轮" in messages and "第二轮" in messages and "备用成功" in messages
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
