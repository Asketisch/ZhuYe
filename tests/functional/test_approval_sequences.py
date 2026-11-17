"""固定种子的审批与中断交错回归。"""

import json
import random

import pytest

from wire import first_event


@pytest.mark.parametrize("seed", [7, 29, 103])
def test_审批与中断交错只产生单一终态(seed, sandbox, serve_session, mock_script):
    actions = ["approve", "deny", "interrupt"]
    random.Random(seed).shuffle(actions)
    script = []
    for index, action in enumerate(actions):
        script.append(json.dumps({"type": "tool_call", "name": "write", "args": {
            "path": f"out-{index}.txt", "content": f"seed-{seed}-{index}"
        }}))
        if action != "interrupt":
            script.append(json.dumps({"type": "text", "text": f"完成-{index}"}))
    script.append('{"type":"text","text":"后续回合成功"}')
    env = sandbox.mock_env(mock_script(script), ZHUYE_APPROVALS="1")
    with serve_session(env=env) as session:
        for index, action in enumerate(actions):
            target = sandbox.cwd / f"out-{index}.txt"
            turn = session.send({"type": "user_input", "items": [{"type": "text", "text": f"执行任务 {index}"}]})
            request = session.wait_event(turn, "approval_request", timeout=15)["msg"]
            assert not target.exists(), (seed, actions, index)
            if action == "interrupt":
                session.collect(session.send({"type": "interrupt"}), timeout=15)
                events = session.collect(turn, timeout=15)
                assert first_event(events, "turn_aborted") is not None, (seed, actions, events)
                session.send({"type": "tool_approval", "id": request["request_id"], "decision": "approve"})
                session.submit({"type": "query_subagents"})
            else:
                decision = "approve" if action == "approve" else {"deny": {"reason": "测试拒绝"}}
                session.send({"type": "tool_approval", "id": request["request_id"], "decision": decision})
                events = session.collect(turn, timeout=15)
                assert first_event(events, "turn_complete")["msg"]["status"] == "success", (seed, actions, events)
            terminals = [event for event in session.all_events() if event.get("id") == turn
                         and event.get("msg", {}).get("type") in {"turn_complete", "turn_aborted"}]
            assert len(terminals) == 1, (seed, actions, terminals)
            assert target.exists() == (action == "approve"), (seed, actions, index)
            if action == "approve":
                assert target.read_text() == f"seed-{seed}-{index}"
        final = session.submit({"type": "user_input", "items": [{"type": "text", "text": "继续"}]})
        assert first_event(final, "turn_complete")["msg"]["status"] == "success", (seed, actions, final)
        assert "后续回合成功" == "".join(event["msg"].get("delta", "") for event in final
                                       if event["msg"]["type"] == "agent_message_delta")
