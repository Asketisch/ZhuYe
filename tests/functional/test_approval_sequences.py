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
        # 默认档(Auto)如今自动批准文件编辑(write);本用例验证的是
        # 审批/中断交错链路,先切 Ask(prompt)钉住"每工具询问"语义。
        session.submit({"type": "set_permission_mode", "mode": "prompt"})
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


@pytest.mark.parametrize("mode,expect", [("prompt", "ask"), ("deny", "deny"), ("auto", "run")])
def test_工作模式对只读bash的闸门语义(mode, expect, sandbox, serve_session, mock_script):
    """三段工作模式 × F04 只读捷径的边界(回归:`printf hi` 曾在 Ask/Deny 下静默执行)。

    - Ask(prompt):承诺每次调用前询问 → 只读 bash 也必须弹 approval_request;
    - Deny:禁止所有工具执行 → 只读 bash 直接被拒(不弹窗、不执行);
    - 默认(auto):只读捷径正常放行(无审批事件、命令执行)。
    """
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "bash",
                    "args": {"command": "printf readonly-marker"}}),
        '{"type":"text","text":"完成"}',
    ])
    env = sandbox.mock_env(script, ZHUYE_APPROVALS="1")
    with serve_session(env=env) as session:
        session.submit({"type": "set_permission_mode", "mode": mode})
        turn = session.send({"type": "user_input", "items": [{"type": "text", "text": "跑"}]})
        if expect == "ask":
            request = session.wait_event(turn, "approval_request", timeout=15)["msg"]
            assert request["kind"]["tool_name"] == "bash", request
            session.send({"type": "tool_approval", "id": request["request_id"],
                          "decision": "approve"})
            events = session.collect(turn, timeout=15)
            assert first_event(events, "turn_complete")["msg"]["status"] == "success", events
        else:
            events = session.collect(turn, timeout=15)
            if expect == "deny":
                end = first_event(events, "tool_call_end")
                assert end is not None and end["msg"]["is_error"] is True, events
                assert "deny" in json.dumps(end["msg"]["output"]).lower(), end
            else:
                end = first_event(events, "tool_call_end")
                assert end is not None and end["msg"]["is_error"] is False, events
                assert "readonly-marker" in json.dumps(end["msg"]["output"]), end
                assert first_event(events, "approval_request") is None, events
