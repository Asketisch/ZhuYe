"""串行回合的真实协议、问答分流与排队取消。"""

import json

from wire import rollout_records


def _text(text):
    return {"type": "user_input", "items": [{"type": "text", "text": text}]}


def _script(mock_script):
    return mock_script([
        json.dumps({"type": "tool_call", "name": "ask_user_question", "args": {"questions": [
            {"header": "选择", "question": "选哪项?", "options": [
                {"label": "甲", "description": "第一项"},
                {"label": "乙", "description": "第二项"}], "multi_select": False}
        ]}}, ensure_ascii=False),
        '{"type":"text","text":"第一轮完成"}',
        '{"type":"text","text":"第二轮完成"}',
    ])


def test_问答立即分流且混合文本排队直到上轮结束(sandbox, serve_session, mock_script):
    sandbox.write_config("\n[turn_scheduling]\nserial = true\n")
    with serve_session(env=sandbox.mock_env(_script(mock_script), ZHUYE_APPROVALS="1")) as session:
        first = session.send(_text("第一轮输入"))
        question = session.wait_event(first, "ask_user_question")["msg"]
        second = session.send({"type": "user_input", "items": [
            {"type": "question_answer", "request_id": question["request_id"],
             "answers": {"answers": [{"selected": [0]}]}},
            {"type": "text", "text": "第二轮输入"},
        ]})
        first_events = session.collect(first)
        second_events = session.collect(second)
        end = next(e["msg"] for e in first_events if e["msg"]["type"] == "tool_call_end")
        assert end["is_error"] is False
        assert '"selected": [0]' in json.dumps(end["output"], ensure_ascii=False)
        assert next(e["msg"] for e in first_events if e["msg"]["type"] == "turn_complete")["status"] == "success"
        assert next(e["msg"] for e in second_events if e["msg"]["type"] == "turn_complete")["status"] == "success"
        first_id = next(e["msg"]["turn_id"] for e in first_events if e["msg"]["type"] == "turn_started")
        second_id = next(e["msg"]["turn_id"] for e in second_events if e["msg"]["type"] == "turn_started")
    records = rollout_records(sandbox.home)
    snapshots = {r["turn_id"]: r for r in records if r.get("type") == "history_snapshot"}
    assert snapshots[first_id]["last_message_turn_id"] is None
    assert snapshots[second_id]["last_message_turn_id"] == first_id
    assert snapshots[second_id]["visible_messages"] > 0
    assert snapshots[second_id]["replay_record_count"] > snapshots[first_id]["replay_record_count"]
    messages = [r for r in records if r.get("type") == "message"]
    serialized = [json.dumps(r.get("content"), ensure_ascii=False) for r in messages]
    first_user = next(i for i, s in enumerate(serialized) if "第一轮输入" in s)
    first_answer = next(i for i, s in enumerate(serialized) if "第一轮完成" in s)
    second_user = next(i for i, s in enumerate(serialized) if "第二轮输入" in s)
    assert first_user < first_answer < second_user, serialized


def test_中断时排队输入得到明确终态(sandbox, serve_session, mock_script):
    sandbox.write_config("\n[turn_scheduling]\nserial = true\n")
    with serve_session(env=sandbox.mock_env(_script(mock_script), ZHUYE_APPROVALS="1")) as session:
        first = session.send(_text("第一轮输入"))
        session.wait_event(first, "ask_user_question")
        queued = session.send(_text("不能执行"))
        session.send({"type": "interrupt"})
        events = session.collect(queued)
        abort = next(e["msg"] for e in events if e["msg"]["type"] == "turn_aborted")
        assert abort["reason"]["type"] == "user_interrupt"
        assert abort["turn_id"]
        assert not any(e["msg"]["type"] == "turn_started" for e in events)
        first_events = session.collect(first)
        assert any(e["msg"]["type"] == "turn_aborted" for e in first_events)


def test_默认并发时记录各回合的历史快照边界(sandbox, serve_session, mock_script):
    with serve_session(env=sandbox.mock_env(_script(mock_script), ZHUYE_APPROVALS="1")) as session:
        first = session.send(_text("第一轮输入"))
        question = session.wait_event(first, "ask_user_question")["msg"]
        first_id = session.wait_event(first, "turn_started")["msg"]["turn_id"]
        second = session.send(_text("并发输入"))
        second_events = session.collect(second)
        second_id = next(e["msg"]["turn_id"] for e in second_events if e["msg"]["type"] == "turn_started")
        session.send({"type": "ask_user_question_response", "id": question["request_id"],
                      "answers": {"answers": [{"selected": [0]}]}})
        assert any(e["msg"]["type"] == "turn_complete" for e in session.collect(first))
    snapshots = {r["turn_id"]: r for r in rollout_records(sandbox.home)
                 if r.get("type") == "history_snapshot"}
    assert snapshots[first_id]["last_message_turn_id"] is None
    assert snapshots[second_id]["last_message_turn_id"] == first_id
    assert snapshots[second_id]["replay_record_count"] > snapshots[first_id]["replay_record_count"]
    assert snapshots[second_id]["visible_messages"] > 0


def test_串行模式回退清空后不复活内存历史(sandbox, serve_session, mock_script):
    sandbox.write_config("\n[turn_scheduling]\nserial = true\n")
    script = mock_script([
        '{"type":"text","text":"旧回复"}',
        '{"type":"text","text":"新回复"}',
    ])
    with serve_session(env=sandbox.mock_env(script)) as session:
        first = session.submit(_text("将被回退的输入"))
        first_id = next(e["msg"]["turn_id"] for e in first if e["msg"]["type"] == "turn_started")
        events = session.submit({"type": "rewind", "to_turn_id": first_id})
        rewound = next(e["msg"] for e in events if e["msg"]["type"] == "turn_rewound")
        assert rewound["truncated_after"] >= 2

        second = session.submit(_text("回退后的新输入"))
        second_id = next(e["msg"]["turn_id"] for e in second if e["msg"]["type"] == "turn_started")
        assert next(e["msg"] for e in second if e["msg"]["type"] == "turn_complete")["status"] == "success"

    snapshot = next(r for r in rollout_records(sandbox.home)
                    if r.get("type") == "history_snapshot" and r["turn_id"] == second_id)
    assert snapshot["last_message_turn_id"] is None
    assert snapshot["visible_messages"] == 0


def test_串行排队时工具审批立即解除当前回合(sandbox, serve_session, mock_script):
    sandbox.write_config("\n[turn_scheduling]\nserial = true\n")
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "write", "args": {
            "path": "approved.txt", "content": "审批通过",
        }}, ensure_ascii=False),
        '{"type":"text","text":"写入完成"}',
        '{"type":"request_echo"}',
    ])
    with serve_session(env=sandbox.mock_env(script, ZHUYE_APPROVALS="1")) as session:
        first = session.send({
            **_text("写入"), "thread_settings": {"approval_policy": "prompt"},
        })
        approval = session.wait_event(first, "approval_request")["msg"]
        queued = session.send(_text("读取上一轮上下文"))
        session.send({"type": "tool_approval", "id": approval["request_id"], "decision": "approve"})
        first_events = session.collect(first)
        queued_events = session.collect(queued)
        outcome = next(e["msg"] for e in first_events if e["msg"]["type"] == "tool_call_end")
        assert outcome["is_error"] is False
        assert (sandbox.cwd / "approved.txt").read_text() == "审批通过"
        assert next(e["msg"] for e in queued_events if e["msg"]["type"] == "turn_complete")["status"] == "success"
        echo = "".join(e["msg"]["delta"] for e in queued_events if e["msg"]["type"] == "agent_message_delta")
        assert "写入完成" in echo and "读取上一轮上下文" in echo
