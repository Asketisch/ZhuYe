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
        # 审批链路用回合级 `approval_policy=prompt` 钉住「逐工具弹窗」;
        # 会话档 prompt(询问档)如今是只读语义(写类直接拒绝,不弹窗),
        # 不能再拿来制造 approval_request。
        for index, action in enumerate(actions):
            target = sandbox.cwd / f"out-{index}.txt"
            turn = session.send({"type": "user_input",
                                 "items": [{"type": "text", "text": f"执行任务 {index}"}],
                                 "thread_settings": {"approval_policy": "prompt"}})
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


@pytest.mark.parametrize("mode,expect", [("prompt", "run"), ("deny", "deny"), ("auto", "run")])
def test_工作模式对只读bash的闸门语义(mode, expect, sandbox, serve_session, mock_script):
    """三段工作模式 × F04 只读捷径的边界。

    - Ask(prompt):v1.x 询问档只读语义 —— Safe 只读命令自动放行(无弹窗);
      写类与危险命令直接拒绝(不弹窗)。回归:`printf hi` 曾在旧 Ask 语义下
      静默执行,现按设计放行且不冒充「已审批」;
    - Deny(已弃用档):禁止所有工具执行 → 只读 bash 直接被拒(不弹窗);
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
        if expect == "run":
            events = session.collect(turn, timeout=15)
            end = first_event(events, "tool_call_end")
            assert end is not None and end["msg"]["is_error"] is False, events
            assert "readonly-marker" in json.dumps(end["msg"]["output"]), end
            assert first_event(events, "approval_request") is None, events
        else:
            events = session.collect(turn, timeout=15)
            end = first_event(events, "tool_call_end")
            assert end is not None and end["msg"]["is_error"] is True, events
            assert "deny" in json.dumps(end["msg"]["output"]).lower(), end
