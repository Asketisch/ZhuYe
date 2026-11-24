"""异步子代理派发:父回合挂起等待,子代理完成后**同轮**激活续跑。

覆盖的能力语义:

1. `call_<role>(mode="async")` 立即返回占位回执(不等子代理);
2. 父 agent 在子代理还在跑时**继续推进**(发出下一次模型请求)——
   即异步派发换来了真正的并行,不是换了个写法的阻塞;
3. 父回合在「模型没有后续动作」处**挂起等待**子代理,而不是结束回合;
4. 子代理结果被注入回本轮上下文(尾部 `<system-reminder>`),父 agent 在
   **同一个 turn 内**基于结果继续作答(全程只出现一次 `turn_complete`)。
"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from wire import first_event

PARENT_MARK = "异步派发测试"
CHILD_MARK = "子任务-找模块"
CHILD_RESULT = "调研结论:共 3 个模块"


def test_异步子代理派发后父回合挂起并在同轮被激活续跑(sandbox, serve_session) -> None:
    requests: list[dict] = []
    lock = threading.Lock()
    parent_seq = 0
    child_started = threading.Event()
    parent_second_seen = threading.Event()

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            nonlocal parent_seq
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            request_text = json.dumps(request, ensure_ascii=False)
            with lock:
                requests.append(request)
                # 子代理会话拿不到父级的原始用户输入(默认不传父历史),
                # 据此把两类请求分开。
                is_child = all(
                    PARENT_MARK not in json.dumps(message, ensure_ascii=False)
                    for message in request["messages"]
                )
                if is_child:
                    seq = 0
                else:
                    parent_seq += 1
                    seq = parent_seq

            if seq == 0:
                # 子代理:先确认自己跑起来了,再等父 agent 推进到下一次
                # 模型请求 —— 这样「父 agent 在等待期间继续工作」才是被
                # 真实时序证明的,而不是碰巧。
                child_started.set()
                if not parent_second_seen.wait(timeout=15):
                    body = '{"error":{"message":"父 agent 未在预期时间内继续"}}'.encode()
                    self.send_response(500)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                    return
                chunks = [{"id": "child", "model": "gpt-4o", "choices": [{
                    "index": 0, "delta": {"content": CHILD_RESULT}, "finish_reason": "stop"}]}]
            elif seq == 1:
                delta = {"tool_calls": [
                    {"index": 0, "id": "async-call", "type": "function", "function": {
                        "name": "call_explorer",
                        "arguments": json.dumps({"prompt": CHILD_MARK, "mode": "async"},
                                                ensure_ascii=False)}},
                ]}
                chunks = [{"id": "parent-1", "model": "gpt-4o", "choices": [{
                    "index": 0, "delta": delta, "finish_reason": "tool_calls"}]}]
            elif seq == 2:
                # 父 agent 的第二次模型请求:此时子代理还在跑 —— 记下这个
                # 时序事实,子代理据此才允许返回。
                parent_second_seen.set()
                chunks = [{"id": "parent-2", "model": "gpt-4o", "choices": [{
                    "index": 0, "delta": {"content": "子代理已派发,等待结果中"},
                    "finish_reason": "stop"}]}]
            else:
                chunks = [{"id": "parent-final", "model": "gpt-4o", "choices": [{
                    "index": 0, "delta": {"content": "已根据子代理结果收口"},
                    "finish_reason": "stop"}]}]

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
                {"type": "text", "text": PARENT_MARK}
            ]})
            assert child_started.wait(timeout=10), "子代理未启动"
            events = session.collect(parent, timeout=40)

            # ── 1) 异步派发返回占位回执,不等子代理 ──
            begins = [event["msg"] for event in events
                      if event["msg"]["type"] == "tool_call_begin"]
            assert len(begins) == 1, begins
            assert begins[0]["tool_name"] == "call_explorer", begins
            ends = [event["msg"] for event in events
                    if event["msg"]["type"] == "tool_call_end"]
            assert len(ends) == 1, ends
            end = ends[0]
            assert end["call_id"] == begins[0]["call_id"], (begins, ends)
            assert end["is_error"] is False, end
            assert end["args"]["mode"] == "async", end
            assert end["output"]["metadata"].get("async") is True, end
            assert end["output"]["metadata"].get("pending") is True, end
            assert end["output"]["metadata"].get("child_id"), end
            placeholder = end["output"]["content"][0]["text"]
            assert "已派发子代理 explorer" in placeholder, end

            # ── 2) 同轮续跑:全程只有一次 turn_complete ──
            terminals = [event["msg"] for event in events
                         if event["msg"]["type"] in {"turn_complete", "turn_aborted"}]
            assert len(terminals) == 1, terminals
            assert terminals[0]["type"] == "turn_complete", terminals
            assert terminals[0]["status"] == "success", terminals

            # ── 3) 结果被注入回本轮上下文,父 agent 据此续跑 ──
            with lock:
                parent_requests = [r for r in requests
                                   if any(PARENT_MARK in json.dumps(m, ensure_ascii=False)
                                          for m in r["messages"])]
            assert len(parent_requests) == 3, [
                json.dumps(r["messages"])[:200] for r in parent_requests]
            # 第二次模型请求发生在子代理返回之前 —— 父 agent 真的并行推进了。
            assert not any(CHILD_RESULT in json.dumps(r["messages"], ensure_ascii=False)
                           for r in parent_requests[:2]), parent_requests[:2]
            # 第三次请求(挂起等待之后)才带上子代理结果。
            final_messages = json.dumps(parent_requests[2]["messages"], ensure_ascii=False)
            assert "<system-reminder>" in final_messages, final_messages[:400]
            assert CHILD_RESULT in final_messages, final_messages[:400]
            # 占位回执仍在工具回执里(模型看到的是「已派发」+ 后续注入)。
            tool_text = json.dumps(parent_requests[2]["messages"], ensure_ascii=False)
            assert "已派发子代理 explorer" in tool_text, tool_text[:400]

            # ── 4) 最终收口文本来自挂起之后的模型调用 ──
            assert "".join(event["msg"]["delta"] for event in events
                           if event["msg"]["type"] == "agent_message_delta") == \
                "子代理已派发,等待结果中已根据子代理结果收口"
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
