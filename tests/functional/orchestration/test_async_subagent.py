"""异步子代理派发:父回合挂起等待,子代理完成后**同轮**激活续跑。

覆盖的能力语义:

1. `call_<role>(mode="async")` 立即返回占位回执(不等子代理);
2. 父 agent 在子代理还在跑时**继续推进**(发出下一次模型请求)——
   即异步派发换来了真正的并行,不是换了个写法的阻塞;
3. 父回合在「模型没有后续动作」处**挂起等待**子代理,而不是结束回合;
4. 子代理结果被注入回本轮上下文(尾部 `<system-reminder>`),父 agent 在
   **同一个 turn 内**基于结果继续作答(全程只出现一次 `turn_complete`);
5. 注入的结果**写入会话记录**—— 引擎每轮从 rollout 重建历史,不落盘的结果
   下一轮就只剩「已派发」占位回执;
6. 打断挂起中的父回合会**级联取消**在途异步子代理(与同步模式、与协议文档
   「打断父回合会取消其在途子代理」一致),且陈旧条目不波及后续回合。
"""

from __future__ import annotations

import json
import select
import socket
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from wire import first_event, rollout_files

PARENT_MARK = "异步派发测试"
CHILD_MARK = "子任务-找模块"
CHILD_RESULT = "调研结论:共 3 个模块"
# 注入消息的标识前缀(只有回填消息带它,可与「已完成调用记录」提醒区分)。
RESULT_MARK = "[子代理 explorer 已完成(session="
OPENAI_CONFIG = '\n[active]\nprovider = "openai"\n[openai]\napi_key = "test"\n'


def _sse(*chunks: dict) -> bytes:
    """把若干 chat-completion 增量帧拼成 SSE 响应体。"""
    return b"".join(("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\n").encode()
                    for chunk in chunks) + b"data: [DONE]\n\n"


def _text_chunk(chunk_id: str, text: str) -> dict:
    return {"id": chunk_id, "model": "gpt-4o", "choices": [{
        "index": 0, "delta": {"content": text}, "finish_reason": "stop"}]}


def _parent_requests(requests: list[dict], lock: threading.Lock) -> list[dict]:
    """父会话的模型请求(子代理会话拿不到父级原始用户输入,据此区分)。"""
    with lock:
        return [r for r in requests
                if any(PARENT_MARK in json.dumps(m, ensure_ascii=False)
                       for m in r["messages"])]


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


def test_异步子代理结果写入会话记录并在后续回合保留(sandbox, serve_session) -> None:
    """注入的子代理结果必须写进会话记录。

    引擎每轮从 rollout 重建历史:结果若只在内存里注入,下一轮模型看到的是
    「已派发」占位回执 + 自己引用了该结果的收口文本,却找不到结果本身。
    落盘形态与 steering 插话一致 —— `role=user` 的 `<system-reminder>` 记录。
    """
    requests: list[dict] = []
    lock = threading.Lock()
    parent_seq = 0
    second_mark = "第二轮追问"

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            nonlocal parent_seq
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            with lock:
                requests.append(request)
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
                body = _sse(_text_chunk("child", CHILD_RESULT))
            elif seq == 1:
                delta = {"tool_calls": [
                    {"index": 0, "id": "async-call", "type": "function", "function": {
                        "name": "call_explorer",
                        "arguments": json.dumps({"prompt": CHILD_MARK, "mode": "async"},
                                                ensure_ascii=False)}},
                ]}
                body = _sse({"id": "parent-1", "model": "gpt-4o", "choices": [{
                    "index": 0, "delta": delta, "finish_reason": "tool_calls"}]})
            elif seq == 2:
                body = _sse(_text_chunk("parent-2", "已根据子代理结果收口"))
            else:
                body = _sse(_text_chunk("parent-later", "第二轮收口"))
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
            OPENAI_CONFIG + f'base_url = "http://127.0.0.1:{gateway.server_port}"\n'
        )
        env = {"ZHUYE_MODEL": "openai/gpt-4o", "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            first = session.submit({"type": "user_input", "items": [
                {"type": "text", "text": PARENT_MARK}
            ]}, timeout=40)
            first_turn = first_event(first, "turn_started")["msg"]["turn_id"]
            assert first_event(first, "turn_complete")["msg"]["status"] == "success", first
            second = session.submit({"type": "user_input", "items": [
                {"type": "text", "text": second_mark}
            ]}, timeout=40)
            assert first_event(second, "turn_complete")["msg"]["status"] == "success", second

            # 第二轮只发一次模型请求(没有陈旧条目把回合重新激活)。
            later_requests = _parent_requests(requests, lock)
            assert len(later_requests) == 4, [len(later_requests)]
            later_messages = later_requests[3]["messages"]
            assert second_mark in json.dumps(later_messages, ensure_ascii=False)
            carried = [message for message in later_messages if message.get("role") == "user"
                       and RESULT_MARK in json.dumps(message, ensure_ascii=False)]
            assert len(carried) == 1, f"子代理结果没有跨回合保留:{later_messages}"
            assert CHILD_RESULT in json.dumps(carried[0], ensure_ascii=False), carried[0]

        # 落盘字段级断言:结果以 user 消息记录在派发它的那个回合名下。
        parent_file = next(f for f in rollout_files(sandbox.home) if PARENT_MARK in f.read_text())
        records = [json.loads(line) for line in parent_file.read_text().splitlines() if line]
        persisted = [record for record in records
                     if record.get("type") == "message" and record.get("role") == "user"
                     and RESULT_MARK in json.dumps(record.get("content"), ensure_ascii=False)]
        assert len(persisted) == 1, records
        assert persisted[0]["turn_id"] == first_turn, persisted[0]
        assert CHILD_RESULT in json.dumps(persisted[0]["content"], ensure_ascii=False), persisted[0]
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()


def test_中断挂起中的父回合会级联取消异步子代理且不波及后续回合(sandbox, serve_session) -> None:
    """打断父回合 = 取消它在途的异步子代理。

    同步模式下工具 future 被丢弃即级联取消子代理;异步模式的子代理由后台
    任务持有,**不**随父回合令牌自动取消 —— 必须显式级联。否则子代理在用户
    按下中断后继续跑,结果留在登记表里被之后某个回合注入;若它卡在审批上,
    之后每个回合收尾都会为它挂起。
    """
    requests: list[dict] = []
    lock = threading.Lock()
    parent_seq = 0
    child_started = threading.Event()
    child_disconnected = threading.Event()
    release_child = threading.Event()

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            nonlocal parent_seq
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            with lock:
                requests.append(request)
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
                # 子代理:一直不返回,直到父会话关闭它的模型连接。
                child_started.set()
                while not release_child.wait(timeout=0.05):
                    readable, _, _ = select.select([self.connection], [], [], 0)
                    try:
                        closed = bool(readable) and not self.connection.recv(1, socket.MSG_PEEK)
                    except ConnectionResetError:
                        closed = True
                    if closed:
                        child_disconnected.set()
                        break
                release_child.wait(timeout=15)
                return
            if seq == 1:
                delta = {"tool_calls": [
                    {"index": 0, "id": "async-call", "type": "function", "function": {
                        "name": "call_explorer",
                        "arguments": json.dumps({"prompt": CHILD_MARK, "mode": "async"},
                                                ensure_ascii=False)}},
                ]}
                body = _sse({"id": "parent-1", "model": "gpt-4o", "choices": [{
                    "index": 0, "delta": delta, "finish_reason": "tool_calls"}]})
            else:
                text = "子代理已派发,等待结果中" if seq == 2 else "后续回合收口"
                body = _sse(_text_chunk(f"parent-{seq}", text))
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
            OPENAI_CONFIG + f'base_url = "http://127.0.0.1:{gateway.server_port}"\n'
        )
        env = {"ZHUYE_MODEL": "openai/gpt-4o", "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            parent = session.send({"type": "user_input", "items": [
                {"type": "text", "text": PARENT_MARK}
            ]})
            assert child_started.wait(timeout=10), "子代理未启动"
            # 等父 agent 说出「等待结果中」—— 之后它不再发工具调用,回合进入挂起等待。
            deadline = time.monotonic() + 10
            while not any(event.get("id") == parent and "等待结果中" in json.dumps(event["msg"], ensure_ascii=False)
                          for event in session.all_events()):
                assert time.monotonic() < deadline, [e["msg"]["type"] for e in session.all_events()]
                time.sleep(0.05)
            session.submit({"type": "interrupt"})
            events = session.collect(parent, timeout=10)
            assert first_event(events, "turn_aborted")["msg"]["reason"]["type"] == "user_interrupt", events
            assert child_disconnected.wait(timeout=10), "中断父回合后异步子代理的模型连接没有关闭"
            statuses = first_event(session.submit({"type": "query_subagents"}), "subagent_status")["msg"]["children"]
            after = {entry["role"]: entry for entry in statuses}
            assert after["explorer"]["state"] == "cancelled", statuses
            assert after["explorer"]["finished_at"], statuses

            # 后续回合正常收尾,且不被陈旧条目重新激活。
            later = session.submit({"type": "user_input", "items": [
                {"type": "text", "text": "继续其他任务"}
            ]}, timeout=20)
            assert first_event(later, "turn_complete")["msg"]["status"] == "success", later
            later_requests = _parent_requests(requests, lock)
            assert len(later_requests) == 3, [
                "后续回合被陈旧条目重新激活",
                *(json.dumps(r["messages"], ensure_ascii=False)[-300:] for r in later_requests)]
            later_text = json.dumps(later_requests[2]["messages"], ensure_ascii=False)
            assert "继续其他任务" in later_text and RESULT_MARK not in later_text, later_text[-400:]
    finally:
        release_child.set()
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()
