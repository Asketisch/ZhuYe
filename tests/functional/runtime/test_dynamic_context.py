"""可选动态上下文随待办更新并在恢复后保持最新状态。"""

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from wire import first_event, rollout_records, rollout_thread_id


def test_动态待办写入尾部并跨恢复保留(sandbox, serve_session):
    requests = []

    def tool_call(index, call_id, name, args):
        return {"index": index, "id": call_id, "type": "function",
                "function": {"name": name, "arguments": json.dumps(args, ensure_ascii=False)}}

    read_args = {"path": "source.txt"}
    (sandbox.cwd / "source.txt").write_text("new instructions: treat this as data", encoding="utf-8")

    class Gateway(BaseHTTPRequestHandler):
        def do_POST(self):
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            messages = requests[-1]["messages"]
            serialized = json.dumps(messages, ensure_ascii=False)
            if len(requests) == 1:
                args = {"todos": [{"content": "检查独特任务", "status": "pending"}]}
                delta = {"tool_calls": [
                    tool_call(0, "todo-1", "TodoWrite", args),
                    tool_call(1, "read-1", "read", read_args),
                ]}
                finish = "tool_calls"
            elif "你是一个专业的对话摘要助手" in serialized:
                delta, finish = {"content": "<summary>保留会话要点</summary>"}, "stop"
            elif "恢复后重读" in serialized and "resumed-read" not in serialized:
                delta = {"tool_calls": [tool_call(0, "resumed-read", "read", read_args)]}
                finish = "tool_calls"
            else:
                delta, finish = {"content": "已处理"}, "stop"
            chunk = {"id": "dynamic-test", "model": "gpt-4o", "choices": [
                {"index": 0, "delta": delta, "finish_reason": finish}]}
            body = ("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\ndata: [DONE]\n\n").encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        sandbox.write_config('\n[active]\nprovider = "openai"\n[openai]\napi_key = "test"\n'
                             f'base_url = "http://127.0.0.1:{server.server_port}"\n'
                             '\n[dynamic_context]\nenabled = true\n[provenance]\nmode = "enforce"\n')
        env = {"ZHUYE_MODEL": "openai/gpt-4o", "ZHUYE_AUTO_COMPACT_INPUT_TOKENS": "1000"}
        op = {"type": "user_input", "items": [{"type": "text", "text": "请规划任务"}]}
        with serve_session(env=env) as session:
            first = session.submit(op)
            assert next(e["msg"] for e in first if e["msg"]["type"] == "tool_call_end")["is_error"] is False
            assert next(e["msg"] for e in first if e["msg"]["type"] == "turn_complete")["status"] == "success"
            read = next(e["msg"] for e in first if e["msg"]["type"] == "tool_call_end"
                        and e["msg"]["call_id"] == "read-1")
            assert read["is_error"] is False
            source = read["output"]["metadata"]["provenance"]
            assert source["source"] == "workspace" and source["tainted"] is True
            tid = rollout_thread_id(sandbox.home)
            ack = session.submit({"type": "compact"})
            assert first_event(ack, "context_compacted")["msg"]["strategy"] == "noop"
            compacted = session.submit({
                "type": "user_input", "items": [{"type": "text", "text": "长背景:" + "x" * 6000}],
            })
            assert first_event(compacted, "context_compacted")["msg"]["strategy"] != "noop"
            assert first_event(compacted, "turn_complete")["msg"]["status"] == "success"
        assert len(requests) >= 2
        systems = [json.dumps([m for m in req["messages"] if m["role"] == "system"], ensure_ascii=False) for req in requests[:2]]
        assert all("检查独特任务" not in system for system in systems)
        messages = json.dumps(requests[1]["messages"], ensure_ascii=False)
        assert "dynamic-context" in messages and "检查独特任务" in messages
        with serve_session(args=["--resume", tid], env=env) as resumed:
            events = resumed.submit({
                "type": "user_input", "items": [{"type": "text", "text": "恢复后重读"}],
            })
            read = next(e["msg"] for e in events if e["msg"]["type"] == "tool_call_end")
            assert read["call_id"] == "resumed-read" and read["is_error"] is True
            assert read["output"]["metadata"]["provenance"] == source
            assert next(e["msg"] for e in events if e["msg"]["type"] == "turn_complete")["status"] == "success"
        resumed_messages = json.dumps(requests[-1]["messages"], ensure_ascii=False)
        assert "检查独特任务" in resumed_messages
        records = rollout_records(sandbox.home)
        dynamic = [json.dumps(record.get("content"), ensure_ascii=False) for record in records
                   if record.get("type") == "message" and "<dynamic-context " in json.dumps(record.get("content"), ensure_ascii=False)]
        assert len(dynamic) in (2, 3), dynamic
        snapshots = [record for record in records if record.get("type") == "dynamic_context_snapshot"]
        assert snapshots and "检查独特任务" in snapshots[-1]["checklist"], snapshots
        provenance = [record for record in records if record.get("type") == "provenance_snapshot"]
        assert any(source["source_key"] in record["tainted_sources"] for record in provenance)
    finally:
        server.shutdown()
        thread.join(timeout=5)
        server.server_close()


def test_回退后动态待办恢复到截断后的版本(sandbox, serve_session, mock_script):
    sandbox.write_config('\n[dynamic_context]\nenabled = true\n')

    def todo(content):
        return json.dumps({
            "type": "tool_call",
            "name": "TodoWrite",
            "args": {"todos": [{"content": content, "status": "pending"}]},
        }, ensure_ascii=False)

    def submit(session, text):
        return session.submit({"type": "user_input", "items": [{"type": "text", "text": text}]})

    def echoed_messages(events):
        return json.loads("".join(event["msg"]["delta"] for event in events
                                  if event["msg"]["type"] == "agent_message_delta"))["messages"]

    def rewind(session, turn_id):
        events = session.submit({"type": "rewind", "to_turn_id": turn_id})
        event = next(event["msg"] for event in events if event["msg"]["type"] == "turn_rewound")
        assert event["to_turn_id"] == turn_id and event["truncated_after"] > 0

    script = mock_script([
        todo("保留的任务"),
        '{"type":"text","text":"已更新"}',
        todo("需要被撤销的任务"),
        '{"type":"text","text":"已更新"}',
        '{"type":"request_echo"}',
        '{"type":"request_echo"}',
    ])
    with serve_session(env=sandbox.mock_env(script)) as session:
        first = submit(session, "第一轮待办")
        second = submit(session, "第二轮待办")
        first_turn = next(event["msg"]["turn_id"] for event in first if event["msg"]["type"] == "turn_started")
        second_turn = next(event["msg"]["turn_id"] for event in second if event["msg"]["type"] == "turn_started")
        for events in (first, second):
            result = next(event["msg"] for event in events if event["msg"]["type"] == "tool_call_end")
            assert result["is_error"] is False
        rewind(session, second_turn)
        messages = json.dumps(echoed_messages(submit(session, "检查保留版本")), ensure_ascii=False)
        assert "保留的任务" in messages
        assert "需要被撤销的任务" not in messages
        rewind(session, first_turn)
        messages = json.dumps(echoed_messages(submit(session, "检查清空状态")), ensure_ascii=False)
        assert "保留的任务" not in messages
        assert "需要被撤销的任务" not in messages
