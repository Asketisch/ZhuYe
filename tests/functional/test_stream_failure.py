"""模型流提前结束时的自动重试、输出重置和工具副作用。

v1.10 语义:mid-stream 可重试错误(SSE 中断 / 网络 / 5xx / 429)即使
已有可见增量也会自动重试 —— 重试前发 `agent_message_reset` 让客户端
清掉半成品输出,重试从头生成,防止两次输出拼接。只有不可重试错误
(4xx / 取消 / 上下文超限)才保留部分输出并结束回合。
"""

from __future__ import annotations

import json
import socket
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from wire import first_event


def _frames(provider: str, phase: str) -> bytes:
    if provider == "chat":
        if phase == "empty":
            delta = {"role": "assistant"}
        elif phase == "text":
            delta = {"content": "部分回复"}
        else:
            delta = {"tool_calls": [{"index": 0, "id": "write-call", "type": "function",
                                     "function": {"name": "write", "arguments": json.dumps(
                                         {"path": "must-not-exist", "content": "不应写入"},
                                         ensure_ascii=False)}}]}
        chunk = {"id": "test", "model": "gpt-4o", "choices": [
            {"index": 0, "delta": delta, "finish_reason": None}]}
        return ("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\n").encode()

    if provider == "responses":
        frames = [("response.created", {"response": {"id": "resp", "model": "gpt-4o"}})]
        if phase == "text":
            frames.append(("response.output_text.delta", {"delta": "部分回复"}))
        elif phase == "tool":
            frames.extend([
                ("response.output_item.added", {"output_index": 0, "item": {
                    "type": "function_call", "id": "fc-1", "call_id": "write-call",
                    "name": "write", "arguments": ""}}),
                ("response.function_call_arguments.delta", {"output_index": 0, "item_id": "fc-1",
                    "delta": json.dumps({"path": "must-not-exist", "content": "不应写入"},
                                        ensure_ascii=False)}),
            ])
    else:
        frames = [("message_start", {"type": "message_start", "message": {
            "id": "msg", "model": "claude-test", "usage": {"input_tokens": 1}}})]
        if phase == "text":
            frames.extend([
                ("content_block_start", {"type": "content_block_start", "index": 0,
                                         "content_block": {"type": "text", "text": ""}}),
                ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                         "delta": {"type": "text_delta", "text": "部分回复"}}),
            ])
        elif phase == "tool":
            frames.extend([
                ("content_block_start", {"type": "content_block_start", "index": 0,
                                         "content_block": {"type": "tool_use", "id": "write-call",
                                                           "name": "write", "input": {}}}),
                ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                         "delta": {"type": "input_json_delta", "partial_json": json.dumps(
                                             {"path": "must-not-exist", "content": "不应写入"},
                                             ensure_ascii=False)}}),
            ])
    return "".join("event: " + event + "\ndata: " + json.dumps(data, ensure_ascii=False)
                   + "\n\n" for event, data in frames).encode()


def _successful_frames(provider: str) -> bytes:
    if provider == "chat":
        body = {"id": "ok", "model": "gpt-4o", "choices": [{
            "index": 0, "delta": {"content": "正常回复"}, "finish_reason": "stop"}]}
        return ("data: " + json.dumps(body, ensure_ascii=False) + "\n\ndata: [DONE]\n\n").encode()
    if provider == "responses":
        frames = [("response.output_text.delta", {"delta": "正常回复"}),
                  ("response.completed", {"response": {"status": "completed"}})]
    else:
        frames = [
            ("message_start", {"type": "message_start", "message": {"id": "ok", "model": "claude-test"}}),
            ("content_block_start", {"type": "content_block_start", "index": 0,
                                     "content_block": {"type": "text", "text": ""}}),
            ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                     "delta": {"type": "text_delta", "text": "正常回复"}}),
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
            ("message_stop", {"type": "message_stop"}),
        ]
    return "".join("event: " + event + "\ndata: " + json.dumps(data, ensure_ascii=False)
                   + "\n\n" for event, data in frames).encode()


@pytest.mark.parametrize("provider", ["chat", "responses", "anthropic"])
@pytest.mark.parametrize("phase", ["empty", "text", "tool"])
@pytest.mark.parametrize("truncated", [False, True], ids=["干净EOF", "连接截断"])
def test_模型流提前断开后的重试与恢复(sandbox, serve_session, provider, phase, truncated) -> None:
    requests: list[dict] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            body = _frames(provider, phase) if len(requests) == 1 else _successful_frames(provider)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body) + (100 if len(requests) == 1 and truncated else 0)))
            self.end_headers()
            self.wfile.write(body)
            self.wfile.flush()
            if len(requests) == 1 and truncated:
                self.connection.shutdown(socket.SHUT_WR)

        def log_message(self, *_args) -> None:
            pass

    gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
    thread = threading.Thread(target=gateway.serve_forever, daemon=True)
    thread.start()
    try:
        provider_name = "anthropic" if provider == "anthropic" else "openai"
        model = "anthropic/claude-test" if provider == "anthropic" else "openai/gpt-4o"
        response_option = "responses_api = true\n" if provider == "responses" else ""
        sandbox.write_config(
            f'\n[active]\nprovider = "{provider_name}"\n[{provider_name}]\napi_key = "test"\n'
            f'{response_option}base_url = "http://127.0.0.1:{gateway.server_port}"\n'
        )
        env = {"ZHUYE_MODEL": model, "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            first = session.send({"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]})
            # v1.10:三个 phase(空流 / 已见文本 / 已见工具调用)都自动重试
            # 并成功收尾 —— 可见增量不再是放弃重试的理由。
            events = session.collect(first, timeout=20)
            assert first_event(events, "turn_complete")["msg"]["status"] == "success", events
            assert len(requests) == 2, requests
            # 失败原因必须可见(StreamError),且重试前必须通知客户端清缓冲。
            stream_error = first_event(events, "stream_error")
            assert stream_error is not None and stream_error["msg"]["code"] == "SSE_PARSE_ERROR", events
            resets = [e["msg"] for e in events if e["msg"]["type"] == "agent_message_reset"]
            if phase == "empty":
                # 尚无可见增量,没有需要清空的缓冲 → 不发 reset。
                assert resets == [], events
            else:
                assert len(resets) == 1, events
                assert resets[0]["reason_code"] == "SSE_PARSE_ERROR", resets
                assert resets[0]["retry_in_ms"] >= 0, resets
            # 半成品工具调用被丢弃,重试流没有执行任何工具。
            assert not (sandbox.cwd / "must-not-exist").exists()
            assert first_event(events, "tool_call_end") is None, events
            # 最终文本只来自重试后的完整输出(阶段一的半成品不拼接)。
            # 按客户端语义累积:agent_message_reset 到达时清空缓冲。
            streamed = ""
            for e in events:
                if e["msg"]["type"] == "agent_message_reset":
                    streamed = ""
                elif e["msg"]["type"] == "agent_message_delta":
                    streamed += e["msg"]["delta"]
            assert streamed == "正常回复", events
            # 重试请求体不得携带被丢弃的半成品(避免把半截回答喂回模型)。
            retried = json.dumps(requests[-1], ensure_ascii=False)
            assert "部分回复" not in retried, retried
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()


@pytest.mark.parametrize(
    ("provider", "expected_code"),
    [("chat", "RATE_LIMITED"), ("responses", "PROVIDER_ERROR"), ("anthropic", "OVERLOADED")],
)
def test_部分输出后流内可重试错误会重置输出并切凭证(
    sandbox, serve_session, provider, expected_code,
) -> None:
    """v1.10:已见输出后的流内可重试错误 → 清缓冲 + 切凭证重试成功。

    此前该场景直接放弃(保留半截输出、发 Error 收场);现在按自动重试
    语义切到备用凭证重新生成,半成品不再进入历史。
    """
    requests: list[tuple[str, dict]] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            auth = self.headers.get("Authorization", "")
            requests.append((auth, json.loads(self.rfile.read(int(self.headers["Content-Length"])))))
            if auth == "Bearer primary-key":
                if provider == "chat":
                    failure = ("data: " + json.dumps({"error": {"code": "rate_limit_exceeded",
                               "message": "限流"}}) + "\n\n").encode()
                else:
                    event = "response.failed" if provider == "responses" else "error"
                    data = ({"response": {"status": "failed", "error": {
                        "code": "server_error", "message": "暂不可用"}}} if provider == "responses" else
                        {"type": "error", "error": {"type": "overloaded_error", "message": "过载"}})
                    failure = ("event: " + event + "\ndata: " + json.dumps(data, ensure_ascii=False)
                               + "\n\n").encode()
                body = _frames(provider, "text") + failure
            else:
                body = _successful_frames(provider)
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
        name = "anthropic" if provider == "anthropic" else "openai"
        model = "anthropic/claude-test" if provider == "anthropic" else "openai/gpt-4o"
        response_option = "responses_api = true\n" if provider == "responses" else ""
        url = f"http://127.0.0.1:{gateway.server_port}"
        sandbox.write_config(
            f'\n[active]\nprovider = "{name}"\ncredential = "primary"\n'
            f'[{name}]\n{response_option}'
            f'[[{name}.credentials]]\nlabel = "primary"\napi_key = "primary-key"\nbase_url = "{url}"\n'
            f'[[{name}.credentials]]\nlabel = "backup"\napi_key = "backup-key"\nbase_url = "{url}"\n'
        )
        env = {"ZHUYE_MODEL": model, "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            first = session.send({"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]})
            events = session.collect(first, timeout=20)
            # 切换消费的是备用凭证,回合成功收尾。
            assert [auth for auth, _ in requests] == ["Bearer primary-key", "Bearer backup-key"], requests
            stream_error = first_event(events, "stream_error")
            assert stream_error is not None and stream_error["msg"]["code"] == expected_code, events
            assert stream_error["msg"]["credential_label"] == "primary", stream_error
            resets = [e["msg"] for e in events if e["msg"]["type"] == "agent_message_reset"]
            assert len(resets) == 1 and resets[0]["reason_code"] == expected_code, events
            assert first_event(events, "error") is None, events
            assert first_event(events, "turn_complete")["msg"]["status"] == "success", events
            # 半成品输出被丢弃:最终文本只来自备用凭证的重试流。
            # 按客户端语义累积:agent_message_reset 到达时清空缓冲。
            streamed = ""
            for e in events:
                if e["msg"]["type"] == "agent_message_reset":
                    streamed = ""
                elif e["msg"]["type"] == "agent_message_delta":
                    streamed += e["msg"]["delta"]
            assert streamed == "正常回复", events
            # 切换后的请求不携带被丢弃的半成品。
            assert "部分回复" not in json.dumps(requests[-1][1], ensure_ascii=False), requests[-1][1]
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()


def test_重试预算耗尽后以错误与回合终态收场(sandbox, serve_session) -> None:
    """v1.10:自动重试烧满预算 → `error(MAX_ATTEMPTS)` + `turn_aborted`。

    「默认重试 10 次失败后再停下」的端到端断言(用 max_attempts=2
    保持测试时长可控)。关键是失败回合必须带 turn 终态事件 —— 否则
    客户端 turn 永远停在 streaming(停止按钮失效的根因)。
    """
    requests: list[dict] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            body = _frames("chat", "text")  # 每次都提前断流
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body) + 100))
            self.end_headers()
            self.wfile.write(body)
            self.wfile.flush()
            self.connection.shutdown(socket.SHUT_WR)

        def log_message(self, *_args) -> None:
            pass

    gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
    thread = threading.Thread(target=gateway.serve_forever, daemon=True)
    thread.start()
    try:
        url = f"http://127.0.0.1:{gateway.server_port}"
        sandbox.write_config(
            f'\n[active]\nprovider = "openai"\n[routing]\nmax_attempts = 2\n'
            f'[openai]\napi_key = "test"\nbase_url = "{url}"\n'
        )
        env = {"ZHUYE_MODEL": "openai/gpt-4o", "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            sub = session.send({"type": "user_input", "items": [{"type": "text", "text": "必失败"}]})
            events = session.collect(sub, timeout=30)
            assert len(requests) == 2, requests
            error = first_event(events, "error")
            assert error is not None and error["msg"]["code"] == "MAX_ATTEMPTS", events
            aborted = first_event(events, "turn_aborted")
            assert aborted is not None, f"失败回合必须发 turn_aborted:{events}"
            assert aborted["msg"]["reason"]["type"] == "error", aborted
            assert aborted["msg"]["reason"]["code"] == "MAX_ATTEMPTS", aborted
            assert aborted["msg"]["turn_id"] == first_event(events, "turn_started")["msg"]["turn_id"]
            assert first_event(events, "turn_complete") is None, events
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
