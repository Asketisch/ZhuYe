"""跨组件场景:从模型协议到工具、持久化和重连的完整用户流程。"""

from __future__ import annotations

import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from wire import first_event, msg_types, rollout_files, rollout_thread_id


@pytest.mark.parametrize("interleaved", [False, True], ids=["完整参数", "交错分片"])
def test_并行远程工具逆序回执并恢复对话(sandbox, serve_session, interleaved) -> None:
    """真实 serve 经 SSE 网关并发调用工具,恢复会话后模型仍能看到结果。"""
    requests: list[dict] = []
    release_stream = threading.Event()

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append(request)
            if len(requests) == 1 and interleaved:
                # OpenAI 兼容流允许不同工具调用的参数分片交错到达。
                deltas = [
                    {"role": "assistant", "tool_calls": [
                        {"index": 0, "id": "call-a", "type": "function", "function": {
                            "name": "scenario_lookup", "arguments": '{"key":'}},
                        {"index": 1, "id": "call-b", "type": "function", "function": {
                            "name": "scenario_lookup", "arguments": '{"key":'}},
                    ]},
                    {"tool_calls": [{"index": 0, "function": {"arguments": '"alpha"}'}}]},
                    {"tool_calls": [{"index": 1, "function": {"arguments": '"beta"}'}}]},
                ]
                chunks = [{"id": "scenario", "model": "gpt-4o", "choices": [
                    {"index": 0, "delta": delta, "finish_reason": None}
                ]} for delta in deltas]
                chunks.append({"id": "scenario", "model": "gpt-4o", "choices": [
                    {"index": 0, "delta": {}, "finish_reason": "tool_calls"}
                ]})
            elif len(requests) == 1:
                delta = {"role": "assistant", "tool_calls": [
                    {"index": 0, "id": "call-a", "type": "function", "function": {
                        "name": "scenario_lookup", "arguments": '{"key":"alpha"}'}},
                    {"index": 1, "id": "call-b", "type": "function", "function": {
                        "name": "scenario_lookup", "arguments": '{"key":"beta"}'}},
                ]}
                finish = "tool_calls"
            else:
                delta = {"content": "结果已合并" if len(requests) == 2 else "已恢复上下文"}
                finish = "stop"
            if len(requests) != 1 or not interleaved:
                chunks = [{"id": "scenario", "model": "gpt-4o", "choices": [
                    {"index": 0, "delta": delta, "finish_reason": finish}
                ]}]
            frames = [("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\n").encode() for chunk in chunks]
            body = b"".join(frames) + b"data: [DONE]\n\n"
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            if len(requests) == 1 and interleaved:
                self.wfile.write(frames[0])
                self.wfile.flush()
                release_stream.wait(timeout=15)
                self.wfile.write(body[len(frames[0]):])
            else:
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
            session.wait_global(lambda e: e.get("msg", {}).get("type") == "session_configured")
            session.send({"type": "register_tools", "tools": [{
                "name": "scenario_lookup", "description": "按 key 查询",
                "parameters": {"type": "object", "properties": {"key": {"type": "string"}},
                               "required": ["key"]},
            }]})
            submission = session.send({"type": "user_input", "items": [
                {"type": "text", "text": "查询 alpha 与 beta 并合并"}
            ], "thread_settings": {"max_tool_concurrency": 2}})
            if interleaved:
                early = session.wait_event(submission, "tool_call_args_delta", timeout=5)["msg"]
                assert early["call_id"] == "call-a" and early["delta"] == '{"key":', early
                release_stream.set()
            pending = {}
            for key in ("alpha", "beta"):
                event = session.wait_global(lambda e: e.get("msg", {}).get("type") == "tool_execution_request"
                                            and (e["msg"].get("args") or {}).get("key") == key,
                                            timeout=5)
                assert event["id"] == ""
                assert event["msg"]["tool"] == "scenario_lookup"
                pending[key] = event["msg"]["call_id"]
            assert pending["alpha"] != pending["beta"]
            # 两个请求都到达后才回执,证明工具确实同时在途。
            for key in ("beta", "alpha"):
                session.send({"type": "tool_execution_response", "call_id": pending[key], "output": {
                    "content": [{"type": "text", "text": f"结果-{key}"}],
                    "is_error": False, "metadata": {}, "elapsed_ms": 0,
                }})
            events = session.collect(submission, timeout=30)
            ends = {e["msg"]["call_id"]: e["msg"] for e in events if e["msg"]["type"] == "tool_call_end"}
            assert set(ends) == {"call-a", "call-b"}, ends
            deltas = {call_id: "".join(e["msg"]["delta"] for e in events
                                       if e["msg"]["type"] == "tool_call_args_delta"
                                       and e["msg"]["call_id"] == call_id)
                      for call_id in ("call-a", "call-b")}
            assert {call_id: json.loads(parts) for call_id, parts in deltas.items()} == {
                "call-a": {"key": "alpha"}, "call-b": {"key": "beta"},
            }, deltas
            for call_id, key in (("call-a", "alpha"), ("call-b", "beta")):
                assert ends[call_id]["is_error"] is False
                assert ends[call_id]["output"]["content"] == [{"type": "text", "text": f"结果-{key}"}]
            assert first_event(events, "turn_complete")["msg"]["status"] == "success"
            assert "结果已合并" == "".join(e["msg"]["delta"] for e in events if e["msg"]["type"] == "agent_message_delta")

        assert len(requests) == 2, requests
        messages = requests[1]["messages"]
        tool_results = {m["tool_call_id"]: m["content"] for m in messages if m.get("role") == "tool"}
        assert tool_results == {"call-a": "结果-alpha", "call-b": "结果-beta"}, messages

        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and not rollout_files(sandbox.home):
            time.sleep(0.05)
        tid = rollout_thread_id(sandbox.home)
        records = [json.loads(line) for line in rollout_files(sandbox.home)[-1].read_text().splitlines() if line]
        assistant = next(r for r in records if r.get("type") == "message" and r.get("role") == "assistant")
        blocks = assistant["content"]
        assert [b["type"] for b in blocks] == ["tool_use", "tool_use", "tool_result", "tool_result", "text"], blocks
        assert [(b["id"], b["name"], b["args"]) for b in blocks[:2]] == [
            ("call-a", "scenario_lookup", {"key": "alpha"}),
            ("call-b", "scenario_lookup", {"key": "beta"}),
        ]
        assert [b["call_id"] for b in blocks[2:4]] == ["call-a", "call-b"]
        with serve_session(args=["--resume", tid], env=env) as resumed:
            events = resumed.submit({"type": "user_input", "items": [
                {"type": "text", "text": "请继续刚才的查询"}
            ]}, timeout=30)
            assert first_event(events, "turn_complete")["msg"]["status"] == "success", msg_types(events)
            assert "已恢复上下文" == "".join(e["msg"]["delta"] for e in events if e["msg"]["type"] == "agent_message_delta")
        assert len(requests) == 3, requests
        history = requests[2]["messages"]
        assert any(m.get("role") == "user" and "查询 alpha 与 beta" in str(m.get("content")) for m in history), history
        assert any(m.get("role") == "user" and "请继续刚才的查询" in str(m.get("content")) for m in history), history
        restored = {m["tool_call_id"]: m["content"] for m in history if m.get("role") == "tool"}
        assert restored == tool_results, history
    finally:
        release_stream.set()
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()


def test_模型限流后切换凭证并继续下一轮(sandbox, serve_session) -> None:
    """真实 HTTP 429 触发故障转移,后续回合继续使用可用凭证。"""
    requests: list[tuple[str, dict]] = []

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            auth = self.headers.get("Authorization", "")
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append((auth, body))
            if auth == "Bearer primary-key":
                payload = b'{"error":{"message":"rate limited"}}'
                self.send_response(429)
                self.send_header("Content-Type", "application/json")
                self.send_header("Retry-After", "60")
            else:
                payload = (b'data: {"id":"s","model":"gpt-4o","choices":[{"index":0,'
                           b'"delta":{"content":"backup-ok"},"finish_reason":"stop"}]}\n\n'
                           b'data: [DONE]\n\n')
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
            '\n[active]\nprovider = "openai"\ncredential = "primary"\n'
            '[openai]\nmodel = "gpt-4o"\n'
            f'[[openai.credentials]]\nlabel = "primary"\napi_key = "primary-key"\nbase_url = "{url}"\n'
            f'[[openai.credentials]]\nlabel = "backup"\napi_key = "backup-key"\nbase_url = "{url}"\n'
        )
        env = {"ZHUYE_MODEL": "openai/gpt-4o", "ZHUYE_HOME": str(sandbox.home / ".zhuye")}
        with serve_session(env=env) as session:
            first = session.submit({"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]})
            limited = first_event(first, "stream_error")
            assert limited is not None and limited["msg"]["code"] == "RATE_LIMITED", first
            assert limited["msg"]["credential_label"] == "primary", limited
            switched = next((e for e in first if e["msg"]["type"] == "routing"
                             and e["msg"]["kind"] == "switched"), None)
            assert switched is not None, first
            assert switched["msg"]["from_credential"] == "primary", switched
            assert first_event(first, "turn_complete")["msg"]["status"] == "success", first
            second = session.submit({"type": "user_input", "items": [{"type": "text", "text": "第二轮"}]})
            assert first_event(second, "turn_complete")["msg"]["status"] == "success", second
        assert [auth for auth, _ in requests] == ["Bearer primary-key", "Bearer backup-key", "Bearer backup-key"]
        assert all(body["model"] == "gpt-4o" for _, body in requests)
        assert any("第一轮" in str(message.get("content")) for message in requests[-1][1]["messages"])
        assert any("第二轮" in str(message.get("content")) for message in requests[-1][1]["messages"])
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()


@pytest.mark.parametrize("decision", ["approve", "deny"], ids=["批准", "拒绝"])
def test_读取文件审批修改并执行验证(sandbox, serve_session, decision) -> None:
    """模拟编码任务的审批批准与拒绝,分别验证执行和无副作用。"""
    target = sandbox.cwd / "artifact.txt"
    target.write_text("version=old\n")
    requests: list[dict] = []
    calls = [
        ("read-call", "read", {"path": "artifact.txt"}),
        ("write-call", "write", {"path": "artifact.txt", "content": "version=new\n"}),
    ]
    if decision == "approve":
        calls.append(("verify-call", "bash", {"cmd": "test \"$(cat artifact.txt)\" = 'version=new' && printf verification-ok"}))

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            index = len(requests) - 1
            if index < len(calls):
                call_id, name, args = calls[index]
                delta = {"tool_calls": [{"index": 0, "id": call_id, "type": "function",
                                          "function": {"name": name, "arguments": json.dumps(args)}}]}
                finish = "tool_calls"
            else:
                delta, finish = {"content": "修改并验证完成" if decision == "approve" else "用户拒绝，文件未改"}, "stop"
            payload = {"id": "coding", "model": "gpt-4o", "choices": [
                {"index": 0, "delta": delta, "finish_reason": finish}
            ]}
            body = ("data: " + json.dumps(payload, ensure_ascii=False) + "\n\ndata: [DONE]\n\n").encode()
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
        env = {
            "ZHUYE_MODEL": "openai/gpt-4o", "ZHUYE_HOME": str(sandbox.home / ".zhuye"),
            "ZHUYE_APPROVALS": "1",
            # 本场景验证工具编排;OS 沙箱安全性由独立的 T8/T9/T13/T14 覆盖。
            "ZHUYE_SANDBOX_OS_LEVEL": "0", "ZHUYE_UNTRUSTED_ALLOW_BARE": "1",
        }
        approvals = []
        with serve_session(env=env) as session:
            submission = session.send({"type": "user_input", "items": [
                {"type": "text", "text": "更新 artifact.txt 并验证结果"}
            ]})

            def approve(event):
                msg = event["msg"]
                if msg["type"] == "approval_request":
                    approvals.append(msg)
                    if msg["kind"].get("tool_name") == "write":
                        assert target.read_text() == "version=old\n", "审批前不能修改文件"
                    outcome = ({"deny": {"reason": "用户拒绝更改"}}
                               if decision == "deny" and msg["kind"].get("tool_name") == "write"
                               else "approve")
                    session.send({"type": "tool_approval", "id": msg["request_id"], "decision": outcome})

            events = session.collect(submission, timeout=40, on_event=approve)
        assert any(m["kind"].get("tool_name") == "write" for m in approvals), approvals
        ends = {e["msg"]["call_id"]: e["msg"] for e in events if e["msg"]["type"] == "tool_call_end"}
        assert set(ends) == {call[0] for call in calls}, ends
        assert not ends["read-call"]["is_error"]
        assert "version=old" in str(ends["read-call"]["output"])
        if decision == "approve":
            assert not ends["write-call"]["is_error"] and not ends["verify-call"]["is_error"], ends
            assert "verification-ok" in str(ends["verify-call"]["output"])
            assert target.read_text() == "version=new\n"
        else:
            assert ends["write-call"]["is_error"]
            assert "用户拒绝更改" in str(ends["write-call"]["output"])
            assert target.read_text() == "version=old\n"
        assert first_event(events, "turn_complete")["msg"]["status"] == "success"
        final_text = "".join(e["msg"]["delta"] for e in events if e["msg"]["type"] == "agent_message_delta")
        assert final_text == ("修改并验证完成" if decision == "approve" else "用户拒绝，文件未改")
        assert len(requests) == len(calls) + 1, requests
        for index, (call_id, _name, _args) in enumerate(calls, start=1):
            assert any(m.get("role") == "tool" and m.get("tool_call_id") == call_id
                       for m in requests[index]["messages"]), requests[index]["messages"]
        assert ("verification-ok" if decision == "approve" else "用户拒绝更改") in str(requests[-1]["messages"])
    finally:
        gateway.shutdown()
        thread.join(timeout=5)
        gateway.server_close()


def test_中断远程工具后迟到回执不污染下一轮(sandbox, serve_session, mock_script) -> None:
    """用户中断在飞工具,迟到回执不得完成下一轮的新工具调用。"""
    script = mock_script([
        '{"type":"tool_call","name":"scenario_remote","args":{"round":"first"}}',
        '{"type":"tool_call","name":"scenario_remote","args":{"round":"second"}}',
        '{"type":"text","text":"第二轮已完成"}',
    ])
    env = sandbox.mock_env(script, ZHUYE_HOME=str(sandbox.home / ".zhuye"))
    with serve_session(env=env) as session:
        session.send({"type": "register_tools", "tools": [{
            "name": "scenario_remote", "description": "等待宿主回执",
            "parameters": {"type": "object"},
        }]})
        first_id = session.send({"type": "user_input", "items": [{"type": "text", "text": "执行第一项"}]})
        first_request = session.wait_global(lambda e: e["msg"].get("type") == "tool_execution_request"
                                            and (e["msg"].get("args") or {}).get("round") == "first")
        interrupt_id = session.send({"type": "interrupt"})
        interrupt_events = session.collect(interrupt_id)
        assert msg_types(interrupt_events)[-1] == "submission_closed", interrupt_events
        first_events = session.collect(first_id, timeout=5)
        aborted = first_event(first_events, "turn_aborted")
        assert aborted is not None and aborted["msg"]["reason"]["type"] == "user_interrupt", first_events
        assert first_event(first_events, "turn_complete") is None
        records = [json.loads(line) for line in rollout_files(sandbox.home)[-1].read_text().splitlines() if line]
        persisted = next(r for r in records if r.get("type") == "message" and r.get("role") == "assistant"
                         and r.get("turn_id") == aborted["msg"]["turn_id"])
        blocks = persisted["content"]
        assert [b["type"] for b in blocks] == ["tool_use", "tool_result"], blocks
        assert blocks[0]["id"] == blocks[1]["call_id"] and blocks[1]["output"]["is_error"] is True, blocks

        second_id = session.send({"type": "user_input", "items": [{"type": "text", "text": "执行第二项"}]})
        second_request = session.wait_global(lambda e: e["msg"].get("type") == "tool_execution_request"
                                             and (e["msg"].get("args") or {}).get("round") == "second")
        assert first_request["msg"]["call_id"] != second_request["msg"]["call_id"]

        def reply(request, text):
            session.send({"type": "tool_execution_response", "call_id": request["msg"]["call_id"], "output": {
                "content": [{"type": "text", "text": text}], "is_error": False,
                "metadata": {}, "elapsed_ms": 0,
            }})

        reply(first_request, "迟到的第一轮回执")
        # 查询回执保证服务端已顺序处理前面的孤儿回执。
        barrier = session.submit({"type": "query_subagents"})
        assert first_event(barrier, "subagent_status") is not None, barrier
        assert not any(e.get("id") == second_id and e["msg"]["type"] == "tool_call_end"
                       for e in session.all_events()), session.all_events()
        reply(second_request, "第二轮正确回执")
        second_events = session.collect(second_id)
        end = first_event(second_events, "tool_call_end")
        assert end is not None and end["msg"]["is_error"] is False, second_events
        assert end["msg"]["output"]["content"] == [{"type": "text", "text": "第二轮正确回执"}]
        assert first_event(second_events, "turn_complete")["msg"]["status"] == "success"
        assert "第二轮已完成" == "".join(e["msg"]["delta"] for e in second_events
                                       if e["msg"]["type"] == "agent_message_delta")


def test_中断待审批写入后迟到批准无副作用(sandbox, serve_session, mock_script) -> None:
    """等待写入审批时中断,迟到的批准不能执行写入或阻断后续回合。"""
    target = sandbox.cwd / "must-not-write.txt"
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "write", "args": {
            "path": target.name, "content": "不应写入",
        }}),
        '{"type":"text","text":"下一轮正常"}',
    ])
    env = sandbox.mock_env(script, ZHUYE_HOME=str(sandbox.home / ".zhuye"), ZHUYE_APPROVALS="1")
    with serve_session(env=env) as session:
        first_id = session.send({"type": "user_input", "items": [{"type": "text", "text": "创建文件"}]})
        approval = session.wait_event(first_id, "approval_request", timeout=10)["msg"]
        assert approval["kind"]["tool_name"] == "write", approval
        assert not target.exists()
        interrupt_id = session.send({"type": "interrupt"})
        assert msg_types(session.collect(interrupt_id))[-1] == "submission_closed"
        events = session.collect(first_id, timeout=5)
        aborted = first_event(events, "turn_aborted")
        assert aborted is not None and aborted["msg"]["reason"]["type"] == "user_interrupt", events
        assert not target.exists()
        session.send({"type": "tool_approval", "id": approval["request_id"], "decision": "approve"})
        next_events = session.submit({"type": "user_input", "items": [
            {"type": "text", "text": "继续其他任务"}
        ]})
        assert first_event(next_events, "turn_complete")["msg"]["status"] == "success", next_events
        assert "下一轮正常" == "".join(e["msg"]["delta"] for e in next_events
                                     if e["msg"]["type"] == "agent_message_delta")
        assert not target.exists()
