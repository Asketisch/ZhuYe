"""能力覆盖层 · 协议面:基础通信、并发路由、19 Op 会话状态面。

直接驱动 `zhuye serve` 原始 JSONL wire(不经过任何 SDK),断言到**字段级**。

覆盖:
- D 组 基础通信与事件流:握手 / 单轮事件序列 / 多轮上下文 / token_count / 终态语义 /
  工具参数增量事件(`tool_call_args_delta`);
  整段消息变体(`agent_message`,非流式 provider 形态,不经 delta);
- S 组 并发与路由:submission 隔离 / 队列串行 / 收尾不变量;
- A 组 会话状态 Op:user_input+thread_settings / set_effort / 权限模式 / rewind /
  interrupt / compact / goal 模式 / plan 模式 / 四种孤儿回执 / steer /
  query_subagents / prompt_command。
  shutdown op 优雅退出与 plan_approval revise 拒绝分支(A28 / A29,
  原 tests/rust/serve_ops.sh 迁入);
  内联 question_answer 与 local_image 输入变体接线(A30 / A31)。

运行:tests/run_all.sh functional,或
  ZHUYE_BIN=... python3 -m pytest tests/functional/test_serve.py -v
"""

from __future__ import annotations

import time

import json
import shlex
from typing import Any

import pytest

from wire import (
    DRAIN_TIMEOUT,
    MOCK_MCP,
    ServeSession,
    first_event,
    msg_types,
    require_binary,
    rollout_records,
)

pytestmark = pytest.mark.usefixtures("sandbox")


# ── D 组:基础通信与事件流 ───────────────────────────────────────────────


def test_D1_握手事件字段完整(serve_session, mock_script) -> None:
    """serve 启动即主动 emit 全局 `session_configured`(id=="")。

    依据:crates/runtime/zhuye-exec/src/serve.rs:417-433
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"hi"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        ev = s.wait_global(lambda e: e.get("msg", {}).get("type") == "session_configured")
        assert ev["id"] == "", "握手事件应是全局事件(id 为空)"
        m = ev["msg"]
        assert m["model"] == "mock/mock-1", f"model={m.get('model')!r}"
        assert m["provider"] == "mock", f"provider={m.get('provider')!r}"
        assert m["session_id"], "session_id 不应为空"
        assert m["approval_policy"] == "auto"
        assert m["sandbox_policy"] == "workspace_only"


def test_D2_单轮事件序列与字段(serve_session, mock_script) -> None:
    """一轮的完整事件流:turn_started → delta×2 → token_count → turn_complete。

    依据:mock.rs:208-217(文本拆两段 delta)、event_msg/turn.rs:75-85
    (status 的 wire 真值是 `success`)。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"hello-from-mock"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]},
            timeout=DRAIN_TIMEOUT,
        )
    types = msg_types(evs)
    # turn 事件以 turn_complete 收尾;其后可能还有一条收尾标记
    # `submission_closed`(serve 在 per-turn 通道排空后发出)。
    turn_types = [t for t in types if t != "submission_closed"]
    assert turn_types[0] == "turn_started", f"事件流应以 turn_started 起:{types}"
    assert turn_types[-1] == "turn_complete", f"turn 事件应以 turn_complete 收:{types}"
    deltas = [e for e in evs if e["msg"]["type"] == "agent_message_delta"]
    assert len(deltas) == 2, f"mock 把文本拆成两段 delta,实得 {len(deltas)}"
    assert "".join(d["msg"]["delta"] for d in deltas) == "hello-from-mock"
    tc = evs[-1]["msg"] if evs[-1]["msg"]["type"] == "turn_complete" else first_event(
        evs, "turn_complete"
    )["msg"]
    assert tc["status"] == "success", f"turn_complete.status={tc.get('status')!r}"
    usage = tc["usage"]
    for f in ("input_tokens", "output_tokens", "cached_tokens", "total_tokens"):
        assert f in usage, f"usage 缺字段 {f}:{usage}"
    started = first_event(evs, "turn_started")["msg"]
    assert started["turn_id"], "turn_started 缺 turn_id"
    assert tc["turn_id"] == started["turn_id"], "turn_complete 的 turn_id 应与 turn_started 一致"


def test_D3_多轮上下文保持(serve_session, mock_script) -> None:
    """两轮各自拿到 mock 脚本的第 1、2 行(脚本按模型调用顺序消耗)。"""
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"first-reply"}',
            '{"type":"text","text":"second-reply"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        first = s.submit({"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]})
        second = s.submit({"type": "user_input", "items": [{"type": "text", "text": "第二轮"}]})
    text1 = "".join(
        e["msg"]["delta"] for e in first if e["msg"]["type"] == "agent_message_delta"
    )
    text2 = "".join(
        e["msg"]["delta"] for e in second if e["msg"]["type"] == "agent_message_delta"
    )
    assert text1 == "first-reply", f"第一轮={text1!r}"
    assert text2 == "second-reply", f"第二轮={text2!r}"


def test_D4_token_count_带模型维度(serve_session, mock_script) -> None:
    """一轮内出现 token_count,且带 model / provider 维度(v1.x stats 字段)。

    依据:event_msg/agent.rs:35-56(`model`/`provider` 为 serde default 的
    可选字段,缺省时不序列化)。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"tok"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit({"type": "user_input", "items": [{"type": "text", "text": "计数"}]})
    tcs = [e["msg"] for e in evs if e["msg"]["type"] == "token_count"]
    assert tcs, f"未见 token_count 事件:{msg_types(evs)}"
    for f in ("input_tokens", "output_tokens"):
        assert f in tcs[0], f"token_count 缺 {f}"
    # 至少一条带模型维度(模型级统计的入口)。
    assert any(tc.get("model") == "mock/mock-1" for tc in tcs), f"无 model 维度:{tcs}"


def test_D9_工具参数增量事件字段完整(serve_session, mock_script) -> None:
    """模型生成工具参数期间逐段转发 `tool_call_args_delta`(v1.5 additive 变体)。

    依据:protocol/.../event_msg/tool.rs 的 `ToolCallArgsDeltaEvent`
    (call_id + delta);core/.../model_call/stream.rs 把 LLM 层的
    `ToolUseDelta` 按进行中的 call_id 转发给客户端。

    mock 用 `tool_call_delta` 脚本行制造**分片形态**参数下发
    (Anthropic / OpenAI 的「start → 一/多个 delta → stop」)—— 整包形态
    (参数全在 ToolUseStart 里)没有 delta 可转发,看不到本事件。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call_delta","name":"echo","chunks":["{\\"text\\":\\"pi","ng\\"}"]}',
            '{"type":"text","text":"done"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit({"type": "user_input", "items": [{"type": "text", "text": "调工具"}]})
    types = msg_types(evs)
    deltas = [e["msg"] for e in evs if e["msg"]["type"] == "tool_call_args_delta"]
    assert deltas, f"未见 tool_call_args_delta 事件:{types}"
    # 字段级:call_id + delta 两个字段都在,且非空。
    for d in deltas:
        assert "call_id" in d, f"缺 call_id:{d}"
        assert "delta" in d, f"缺 delta:{d}"
        assert d["call_id"], f"call_id 不应为空:{d}"
        assert d["delta"], f"delta 不应为空:{d}"
    # 分片按到达顺序拼接 = 完整参数 JSON。
    joined = "".join(d["delta"] for d in deltas)
    assert joined == '{"text":"ping"}', f"分片拼接结果={joined!r}"
    # call_id 与 tool_call_begin 一致(同一调用)。
    begins = [e["msg"] for e in evs if e["msg"]["type"] == "tool_call_begin"]
    assert begins, f"未见 tool_call_begin:{types}"
    assert deltas[0]["call_id"] == begins[0]["call_id"], (
        f"args_delta 的 call_id 应与 tool_call_begin 一致:{deltas[0]} vs {begins[0]}"
    )
    # 时序:增量在 tool_call_begin 之后(客户端先知道有这通调用,再收参数)。
    assert types.index("tool_call_args_delta") > types.index("tool_call_begin"), (
        f"args_delta 应在 tool_call_begin 之后:{types}"
    )


def test_D10_中毒错误窗口随空工具回合推进(serve_session, mock_script) -> None:
    """旧工具失败会按回合过期；窗口内连续失败仍触发中毒事件。"""
    require_binary()
    failed_calls = [json.dumps({"type": "tool_call", "name": f"missing_{i}", "args": {}}) for i in range(5)]
    last_call = json.dumps({"type": "tool_call", "name": "missing_last", "args": {}})
    for gap, should_poison in [(10, False), (0, True)]:
        lines = failed_calls + [json.dumps({"type": "text", "text": "首轮结束"})]
        lines += [json.dumps({"type": "text", "text": f"正常答复 {i}"}) for i in range(gap)]
        lines += [last_call, json.dumps({"type": "text", "text": "最后结束"})]
        script = mock_script(lines)
        with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as session:
            first = session.submit({"type": "user_input", "items": [{"type": "text", "text": "先执行工具"}]}, timeout=DRAIN_TIMEOUT)
            ends = [event["msg"] for event in first if event["msg"]["type"] == "tool_call_end"]
            assert len(ends) == 5 and all(end["is_error"] for end in ends), ends
            for i in range(gap):
                session.submit({"type": "user_input", "items": [{"type": "text", "text": f"正常回合 {i}"}]}, timeout=DRAIN_TIMEOUT)
            final = session.submit({"type": "user_input", "items": [{"type": "text", "text": "再执行工具"}]}, timeout=DRAIN_TIMEOUT)
        errors = [event["msg"] for event in final if event["msg"]["type"] == "error" and event["msg"].get("code") == "session_poisoned"]
        assert bool(errors) is should_poison, final
        if should_poison:
            assert errors[0]["details"]["poison_class"] == "error", errors
        assert "turn_complete" in msg_types(final)


def test_D5_终态语义_turn后不再有事件(serve_session, mock_script) -> None:
    """turn 类 submission 以 `turn_complete` 收尾;非 turn 类以 `submission_closed` 收尾。

    依据:serve.rs:308-317(per-turn 通道排空后发收尾标记)。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}', '{"type":"text","text":"t2"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        turn = s.submit({"type": "user_input", "items": [{"type": "text", "text": "一轮"}]})
        turn_types = [t for t in msg_types(turn) if t != "submission_closed"]
        assert turn_types[-1] == "turn_complete", f"turn 应以 turn_complete 收:{msg_types(turn)}"
        # turn_complete 之后不再有该 submission 的 turn 事件。
        idx = msg_types(turn).index("turn_complete")
        after = [t for t in msg_types(turn)[idx + 1 :] if t != "submission_closed"]
        assert not after, f"turn_complete 后仍有事件:{after}"
        # 非 turn 操作:set_effort 静默,只应有收尾标记。
        eff = s.submit({"type": "set_effort", "effort": "high"})
        assert msg_types(eff) == ["submission_closed"], f"set_effort 应静默:{msg_types(eff)}"


# ── S 组:并发与路由 ─────────────────────────────────────────────────────


def test_S1_并发_submission_事件不串台(serve_session, mock_script) -> None:
    """连发两条 user_input:两条 submission 各自成 turn,事件按 id 路由**互不干扰**。

    依据:sdks/PROTOCOL.md §4(各 submission 的事件以各自 id 路由);
    core 侧每个 `UserInput` 提交 `tokio::spawn` 一个独立 task
    (submission_loop.rs:621)—— 两个 turn 是**真并发**,模型调用的先后
    不保证,所以断言只要求"各流自洽 + 两条回复各用一次"。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"reply-A"}',
            '{"type":"text","text":"reply-B"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        # 不等第一个结束就发第二个。
        id_a = s.send({"type": "user_input", "items": [{"type": "text", "text": "A"}]})
        id_b = s.send({"type": "user_input", "items": [{"type": "text", "text": "B"}]})
        evs_a = s.collect(id_a)
        evs_b = s.collect(id_b)
    # 路由隔离:每条流里的事件 id 全属于自己的 submission。
    assert {e.get("id") for e in evs_a} == {id_a}, f"A 流混入别的 id"
    assert {e.get("id") for e in evs_b} == {id_b}, f"B 流混入别的 id"
    # 各自成 turn:两个不同的 turn_id,互不合并。
    turn_a = first_event(evs_a, "turn_started")["msg"]["turn_id"]
    turn_b = first_event(evs_b, "turn_started")["msg"]["turn_id"]
    assert turn_a != turn_b, "两个 submission 应各成一个 turn"
    # 两条 mock 回复各用一次(并发下先后不保证)。
    texts = [
        "".join(
            e["msg"]["delta"] for e in evs if e["msg"]["type"] == "agent_message_delta"
        )
        for evs in (evs_a, evs_b)
    ]
    assert sorted(texts) == ["reply-A", "reply-B"], f"回复分配异常:{texts}"


def test_S2_并发_每个turn独立收尾(serve_session, mock_script) -> None:
    """并发提交的两个 turn 各自以 `turn_complete` 收尾,互不吞并。"""
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"reply-A"}',
            '{"type":"text","text":"reply-B"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        id_a = s.send({"type": "user_input", "items": [{"type": "text", "text": "A"}]})
        id_b = s.send({"type": "user_input", "items": [{"type": "text", "text": "B"}]})
        evs_a = s.collect(id_a)
        evs_b = s.collect(id_b)
    for label, evs in (("A", evs_a), ("B", evs_b)):
        types = msg_types(evs)
        assert types.count("turn_started") == 1, f"{label} 应恰好一个 turn_started:{types}"
        assert types.count("turn_complete") == 1, f"{label} 应恰好一个 turn_complete:{types}"
        tc = first_event(evs, "turn_complete")["msg"]
        assert tc["status"] == "success", f"{label} turn 状态异常:{tc}"


def test_S3_每条submission恰好一条收尾标记(serve_session, mock_script) -> None:
    """不变量:每条 submission 的 per-turn 通道排空后恰好一条 `submission_closed`。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit({"type": "user_input", "items": [{"type": "text", "text": "一轮"}]})
        s.submit({"type": "set_effort", "effort": "low"})
        s.submit({"type": "compact"})
        # 等收尾标记都落地(turn 类的收尾标记在 turn_complete 之后到达)。
        import time

        time.sleep(1.0)
        events = s.all_events()
    per_sub: dict[str, int] = {}
    for e in events:
        if e["msg"]["type"] == "submission_closed":
            per_sub[e.get("id", "")] = per_sub.get(e.get("id", ""), 0) + 1
    assert per_sub, f"未见任何 submission_closed:{msg_types(events)}"
    assert all(v == 1 for v in per_sub.values()), f"收尾标记重复:{per_sub}"


# ── A 组:会话状态 Op ────────────────────────────────────────────────────


def test_A1_user_input_带thread_settings(serve_session, mock_script) -> None:
    """thread_settings 四字段下发 → turn 正常完成、不产生 error 事件。

    依据:原 tests/rust/serve_ops.sh 组 1(该脚本已迁入本文件 A 组)。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {
                "type": "user_input",
                "items": [{"type": "text", "text": "带设置的一轮"}],
                "thread_settings": {
                    "model": "mock",
                    "approval_policy": "prompt",
                    "sandbox_policy": "workspace_only",
                    "max_tool_concurrency": 2,
                },
            }
        )
    assert "turn_complete" in msg_types(evs), f"turn 未完成:{msg_types(evs)}"
    errs = [e for e in evs if e["msg"]["type"] == "error"]
    assert not errs, f"thread_settings 不应产生 error:{errs}"


def test_A1b_回合审批策略阻止工具副作用(serve_session, mock_script, sandbox) -> None:
    """回合级 deny 必须实际阻止工具执行,而不是仅接受字段。"""
    marker = sandbox.cwd / "thread-settings-must-not-exist"
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "bash", "args": {"cmd": f"touch {shlex.quote(str(marker))}"}}),
        '{"type":"text","text":"已处理"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as session:
        events = session.submit({
            "type": "user_input",
            "items": [{"type": "text", "text": "运行命令"}],
            "thread_settings": {"approval_policy": "deny"},
        }, timeout=DRAIN_TIMEOUT)
    end = first_event(events, "tool_call_end")
    assert end is not None and end["msg"]["is_error"] is True, end
    assert "deny" in json.dumps(end, ensure_ascii=False).lower(), end
    assert not marker.exists()
    assert "turn_complete" in msg_types(events)


def test_A2_set_effort_静默(serve_session, mock_script) -> None:
    """set_effort 只写槽,不产任何 turn 事件(仅收尾标记)。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit({"type": "set_effort", "effort": "high"})
    assert msg_types(evs) == ["submission_closed"], f"set_effort 应静默:{msg_types(evs)}"


def test_A3_权限模式切换_全局事件与持久化轨迹(serve_session, mock_script, sandbox) -> None:
    """set_permission_mode / cycle_permission_mode → 全局 `permission_mode_changed`。

    依据:submission_loop.rs:110-118;持久化见 COVERAGE.md §4。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit({"type": "user_input", "items": [{"type": "text", "text": "一轮"}]})
        s.submit({"type": "set_permission_mode", "mode": "plan"})
        s.submit({"type": "cycle_permission_mode"})
        pmc = s.wait_global(
            lambda e: e.get("msg", {}).get("type") == "permission_mode_changed"
        )
        # cycle 会再发一条,等轨迹齐全。
        import time

        time.sleep(1.0)
        all_pmc = [
            e["msg"]
            for e in s.global_events()
            if e["msg"]["type"] == "permission_mode_changed"
        ]
    assert pmc["id"] == "", "permission_mode_changed 是全局事件(id 为空)"
    transitions = [(m.get("from"), m.get("to")) for m in all_pmc]
    assert ("auto", "plan") in transitions, f"缺 auto→plan:{transitions}"
    assert ("plan", "auto") in transitions, f"缺 plan→auto:{transitions}"
    # 持久化轨迹(rollout JSONL)。
    recs = rollout_records(sandbox.home)
    pmc_recs = [r for r in recs if r.get("type") == "permission_mode_changed"]
    assert pmc_recs, f"rollout 无 permission_mode_changed 记录:{[r.get('type') for r in recs]}"
    assert all(r.get("at") for r in pmc_recs), f"pmc 记录缺 at:{pmc_recs}"


def test_A4_rewind_回退(serve_session, mock_script) -> None:
    """rewind → `turn_rewound`(带 truncated_after;to_turn_id 可空)。"""
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"one"}',
            '{"type":"text","text":"two"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit({"type": "user_input", "items": [{"type": "text", "text": "一轮"}]})
        evs = s.submit({"type": "rewind"})
    m = first_event(evs, "turn_rewound")
    assert m is not None, f"未见 turn_rewound:{msg_types(evs)}"
    assert isinstance(m["msg"].get("truncated_after"), int), f"缺 truncated_after:{m['msg']}"
    assert "to_turn_id" in m["msg"], f"缺 to_turn_id:{m['msg']}"


def test_A5_interrupt_打断(serve_session, mock_script) -> None:
    """interrupt → `turn_aborted`,reason 是带 type 的对象。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit({"type": "user_input", "items": [{"type": "text", "text": "一轮"}]})
        evs = s.submit({"type": "interrupt"})
    m = first_event(evs, "turn_aborted")
    assert m is not None, f"未见 turn_aborted:{msg_types(evs)}"
    reason = m["msg"].get("reason")
    assert isinstance(reason, dict), f"reason 应为对象:{reason!r}"
    assert reason.get("type") == "user_interrupt", f"reason.type={reason.get('type')!r}"


def test_A6_compact_压缩(serve_session, mock_script) -> None:
    """compact → `context_compacted`(带 strategy)。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit({"type": "user_input", "items": [{"type": "text", "text": "一轮"}]})
        evs = s.submit({"type": "compact"}, timeout=40.0)
    m = first_event(evs, "context_compacted")
    assert m is not None, f"未见 context_compacted:{msg_types(evs)}"
    assert "strategy" in m["msg"], f"缺 strategy:{m['msg']}"


def test_A7_goal模式_进出静默且turn正常(serve_session, mock_script) -> None:
    """enter/exit_goal_mode 均静默(仅收尾标记);进出后一轮 turn 正常完成。"""
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"t1"}',
            '{"type":"text","text":"t2"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        enter = s.submit(
            {
                "type": "enter_goal_mode",
                "goal": "probe goal",
                "token_budget": 100000,
            }
        )
        exit_ = s.submit({"type": "exit_goal_mode"})
        turn = s.submit({"type": "user_input", "items": [{"type": "text", "text": "干活"}]})
    assert msg_types(enter) == ["submission_closed"], f"enter_goal 应静默:{msg_types(enter)}"
    assert msg_types(exit_) == ["submission_closed"], f"exit_goal 应静默:{msg_types(exit_)}"
    assert "turn_complete" in msg_types(turn), f"goal 模式后 turn 未完成:{msg_types(turn)}"


def test_A8_plan模式_全链路(serve_session, mock_script) -> None:
    """enter_plan_mode → plan_request;exit_plan_mode → plan_ready(含 fallback markdown)。

    依据:原 tests/rust/serve_ops.sh 组 2(该脚本已迁入本文件 A 组)。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit({"type": "enter_plan_mode", "task": "probe task"})
        pr = first_event(evs, "plan_request")
        assert pr is not None, f"未见 plan_request:{msg_types(evs)}"
        assert pr["msg"].get("task") == "probe task", f"plan_request.task={pr['msg']}"

        evs = s.submit({"type": "exit_plan_mode"})
        ready = first_event(evs, "plan_ready")
        assert ready is not None, f"未见 plan_ready:{msg_types(evs)}"
        assert ready["msg"].get("plan_id"), f"plan_ready 缺 plan_id:{ready['msg']}"
        assert ready["msg"].get("markdown"), "plan_ready 应带 fallback markdown"


def test_A8b_plan审批_工具路径全链路(serve_session, mock_script) -> None:
    """PlanWrite → ExitPlanMode → plan_approval:auto_mode → 切权限模式 + plan_approved。

    与 A8(op 路径)的区别:op 路径只产出 `plan_ready`(无审批 waiter);
    **工具路径**才有完整的审批闭环。依据:submission_loop.rs:1432-1444
    (Exit 方向补发 `plan_approved`)。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call","name":"PlanWrite","args":{"path":"p1.md","content":"# plan\\n- s1"}}',
            '{"type":"tool_call","name":"ExitPlanMode","args":{}}',
            '{"type":"text","text":"built it"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        # 注意:工具路径下 ExitPlanMode 会**阻塞 turn** 直到收到 plan_approval
        # —— 审批必须作为另一条 submission 在 turn 在飞时回执(与 bash 套件同款)。
        sid = s.send(
            {"type": "user_input", "items": [{"type": "text", "text": "draft the plan"}]}
        )
        state: dict[str, Any] = {"approved": False}

        def _on_event(ev: dict[str, Any]) -> None:
            m = ev.get("msg", {})
            if m.get("type") == "plan_ready" and not state["approved"]:
                state["approved"] = True
                s.send(
                    {
                        "type": "plan_approval",
                        "id": m["plan_id"],
                        "choice": "auto_mode",
                    }
                )

        evs = s.collect(sid, timeout=60.0, on_event=_on_event)
        draft = first_event(evs, "plan_draft_updated")
        assert draft is not None, f"未见 plan_draft_updated:{msg_types(evs)}"
        assert draft["msg"].get("markdown"), f"plan_draft_updated 缺 markdown:{draft['msg']}"
        ready = first_event(evs, "plan_ready")
        assert ready is not None, f"未见 plan_ready:{msg_types(evs)}"
        assert state["approved"], "未回 plan_approval"
        # 审批落地:权限模式切 accept_edits + 发 plan_approved。
        # 注意:工具路径下这两个事件落在**turn 自己的通道**上(sub_id = turn 的
        # submission id),不是全局事件 —— 与 `set_permission_mode` op 路径
        # (走 apply_permission_mode_change,id 为空)不同。
        pmc = first_event(evs, "permission_mode_changed")
        assert pmc is not None, f"未见 permission_mode_changed:{msg_types(evs)}"
        assert pmc["msg"].get("to") == "accept_edits", f"权限模式未切:{pmc['msg']}"
        approved = first_event(evs, "plan_approved")
        assert approved is not None, f"Exit 方向审批应发 plan_approved:{msg_types(evs)}"
        assert approved["msg"].get("plan_id") == ready["msg"]["plan_id"], (
            f"plan_approved 的 plan_id 应与 plan_ready 一致:{approved['msg']}"
        )


def test_A9_四种孤儿回执_不崩且会话存活(serve_session, mock_script) -> None:
    """无 pending waiter 的回执:不产生 error 事件、迭代器收尾、会话存活。

    依据:submission_loop.rs:1002-1013(查不到 waiter → 静默丢弃)。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"alive-1"}',
            '{"type":"text","text":"alive-2"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        for op in (
            {"type": "tool_approval", "id": "nonexistent", "decision": "approve"},
            {"type": "hook_approval", "id": "nonexistent", "decision": "approve"},
            {
                "type": "ask_user_question_response",
                "id": "nonexistent",
                "answers": {"answers": []},
            },
            {"type": "ask_user_input_response", "id": "nonexistent", "text": "x"},
        ):
            evs = s.submit(op)
            assert msg_types(evs) == ["submission_closed"], f"{op['type']} 未收尾:{msg_types(evs)}"
        # 会话存活:下一轮 prompt 正常(取第一条 mock 脚本回复)。
        turn = s.submit({"type": "user_input", "items": [{"type": "text", "text": "还在吗"}]})
        text = "".join(
            e["msg"]["delta"] for e in turn if e["msg"]["type"] == "agent_message_delta"
        )
        assert text == "alive-1", f"会话未存活:{text!r}"
        errs = [e for e in s.global_events() if e["msg"]["type"] == "error"]
        assert not errs, f"孤儿回执不应产生 error:{errs}"


def test_A10_steer_中途转向(serve_session, mock_script, sandbox) -> None:
    """steer(now):转向消息进入上下文(落盘为 user message),turn 仍正常收尾。

    依据:op.rs:167-171、graph/nodes/pre_loop.rs:477-556(注入后落盘)。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"after-steer"}',
            '{"type":"text","text":"t2"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {
                "type": "steer",
                "priority": "now",
                "items": [{"type": "text", "text": "STEER-TEXT-XYZ"}],
            }
        )
        assert "submission_closed" in msg_types(evs), f"steer 未收尾:{msg_types(evs)}"
        # 下一轮:转向消息应已进入上下文(落盘为 user message 记录)。
        s.submit({"type": "user_input", "items": [{"type": "text", "text": "继续"}]})
        import time

        time.sleep(1.0)
        recs = rollout_records(sandbox.home)
    steer_hits = [
        r
        for r in recs
        if r.get("type") == "message"
        and "STEER-TEXT-XYZ" in json.dumps(r, ensure_ascii=False)
    ]
    assert steer_hits, f"rollout 未见转向消息:{[r.get('type') for r in recs]}"


def test_A11_query_subagents_状态快照(serve_session, mock_script) -> None:
    """query_subagents → `subagent_status`(children 数组)。

    依据:submission_loop.rs:986-1001。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit({"type": "query_subagents"})
    m = first_event(evs, "subagent_status")
    assert m is not None, f"未见 subagent_status:{msg_types(evs)}"
    assert isinstance(m["msg"].get("children"), list), f"children 应为数组:{m['msg']}"


def test_A12_prompt_command_提示词层管理(serve_session, mock_script) -> None:
    """prompt_command(list/switch) → `prompt_list` / `prompt_switched`。

    依据:op_prompt.rs:8-19(PromptAction 是 serde tag=type 的对象),
    submission_loop.rs:1195-1237。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit({"type": "prompt_command", "action": {"type": "list"}})
        m = first_event(evs, "prompt_list")
        assert m is not None, f"未见 prompt_list:{msg_types(evs)}"
        for f in ("available", "current_spec", "active_layers"):
            assert f in m["msg"], f"prompt_list 缺 {f}:{m['msg']}"

        evs = s.submit({"type": "prompt_command", "action": {"type": "switch", "layer": None}})
        m = first_event(evs, "prompt_switched")
        assert m is not None, f"未见 prompt_switched:{msg_types(evs)}"
        assert "active_layers" in m["msg"], f"prompt_switched 缺 active_layers:{m['msg']}"


def test_A13_side_channel启动查询取消(serve_session, mock_script) -> None:
    """side-channel 三 Op:`start_side_channel` → `side_channel_started`;
    `query_side_channels` → `side_channel_status`(含该 id);
    `cancel_side_channel` → `side_channel_finished`(cancelled)。

    依据:crates/runtime/zhuye-core/src/side_channel/(registry + driver)
    + submission_loop 的三个 Op 分支。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        # 1) 启动:mock agent 定义不存在 → side-channel 走完 factory 失败
        #    路径也会先注册并发 started;这里用不存在的 agent 验证启动
        #    事件的字段完整性(started 先于 factory 结果)。
        evs = s.submit(
            {"type": "start_side_channel", "agent": "default", "prompt": "side prompt"}
        )
        m = first_event(evs, "side_channel_started")
        assert m is not None, f"未见 side_channel_started:{msg_types(evs)}"
        assert msg_types(evs).count("side_channel_started") == 1, msg_types(evs)
        started = m["msg"]
        for f in ("id", "agent_name", "prompt", "started_at_ms"):
            assert f in started, f"started 缺 {f}:{started}"
        assert started["agent_name"] == "default"
        assert started["prompt"] == "side prompt"
        sid = started["id"]

        # 2) 查询:快照含该 id。
        evs = s.submit({"type": "query_side_channels"})
        m = first_event(evs, "side_channel_status")
        assert m is not None, f"未见 side_channel_status:{msg_types(evs)}"
        assert msg_types(evs).count("side_channel_status") == 1, msg_types(evs)
        channels = m["msg"].get("channels")
        assert isinstance(channels, list), f"channels 应为数组:{m['msg']}"
        ids = [c.get("id") for c in channels]
        assert sid in ids, f"快照缺 {sid}:{ids}"

        # 3) 取消:未知 / 已终态 id 是 noop(不开新 turn,只回执关闭);
        #    会话存活由随后的查询证明。
        s.submit({"type": "cancel_side_channel", "id": sid})
        s.submit({"type": "cancel_side_channel", "id": "side-00000000"})
        evs = s.submit({"type": "query_side_channels"})
        m = first_event(evs, "side_channel_status")
        assert m is not None, f"取消后会话应仍可查询:{msg_types(evs)}"


def test_A15_side_channel完成事件包含完整回复(serve_session, mock_script) -> None:
    """独立任务结束后,会话事件流必须收到带完整回复和任务 ID 的终态。"""
    script = mock_script(['{"type":"text","text":"side complete response"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        events = s.submit({"type": "start_side_channel", "agent": "default", "prompt": "side prompt"})
        started = first_event(events, "side_channel_started")
        assert started is not None, msg_types(events)
        sid = started["msg"]["id"]
        finished = s.wait_global(lambda e: e.get("msg", {}).get("type") == "side_channel_finished" and e["msg"].get("id") == sid, timeout=30)
        assert finished["msg"]["kind"] == "done", finished
        assert finished["msg"]["summary"] == "side complete response", finished
        assert isinstance(finished["msg"]["finished_at_ms"], int), finished


def test_A16_side_channel取消发送终态(serve_session, mock_script) -> None:
    """取消独立任务后,全局终态携带对应 ID 和 cancelled 类型。"""
    script = mock_script([
        '{"type":"tool_call","name":"bash","args":{"cmd":"sleep 10"}}',
        '{"type":"text","text":"done"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        events = s.submit({"type": "start_side_channel", "agent": "default", "prompt": "等待"})
        started = first_event(events, "side_channel_started")
        assert started is not None, msg_types(events)
        sid = started["msg"]["id"]
        s.submit({"type": "cancel_side_channel", "id": sid})
        finished = s.wait_global(lambda e: e.get("msg", {}).get("type") == "side_channel_finished" and e["msg"].get("id") == sid, timeout=10)
        assert finished["msg"]["kind"] == "cancelled", finished
        assert isinstance(finished["msg"]["finished_at_ms"], int), finished


def test_A17_side_channel继承权限拒绝(serve_session, mock_script, sandbox) -> None:
    """侧通道使用主会话的权限规则,无审批门时也不能执行被拒命令。"""
    import json
    import shlex

    sandbox.write_config('''
[[permissions.rule]]
tool = "bash"
action = "deny"
shell_pattern = "touch *"
''')
    marker = sandbox.cwd / "side-must-not-write"
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "bash", "args": {"cmd": f"touch {shlex.quote(str(marker))}"}}),
        '{"type":"text","text":"side-finished"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as session:
        events = session.submit({"type": "start_side_channel", "agent": "default", "prompt": "测试权限继承"})
        started = first_event(events, "side_channel_started")
        assert started is not None, msg_types(events)
        sid = started["msg"]["id"]
        finished = session.wait_global(lambda e: e.get("msg", {}).get("type") == "side_channel_finished" and e["msg"].get("id") == sid, timeout=30)
        assert finished["msg"]["kind"] == "done", finished
        assert finished["msg"]["summary"] == "side-finished", finished
        assert not marker.exists(), "侧通道绕过权限规则执行了命令"
    # 移除拒绝规则后,相同命令必须真的运行,排除工具缺失等假阳性。
    (sandbox.home / ".zhuye" / "config.toml").write_text("[hooks]\nenabled = []\n")
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as session:
        events = session.submit({"type": "start_side_channel", "agent": "default", "prompt": "验证命令可执行"})
        started = first_event(events, "side_channel_started")
        assert started is not None, msg_types(events)
        sid = started["msg"]["id"]
        finished = session.wait_global(lambda e: e.get("msg", {}).get("type") == "side_channel_finished" and e["msg"].get("id") == sid, timeout=30)
        assert finished["msg"]["kind"] == "done", finished
        assert marker.exists(), "无拒绝规则时命令应能创建文件"


def test_A24_side_channel使用热重载后的模型(serve_session, sandbox) -> None:
    """配置热重载后,新侧通道使用更新后的模型名。"""
    import threading
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    received = []
    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            received.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            payload = {"id": "test", "model": "test", "choices": [{"index": 0, "delta": {"content": "done"}, "finish_reason": "stop"}]}
            body = ("data: " + json.dumps(payload) + "\n\ndata: [DONE]\n\n").encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        sandbox.write_config(
            f'\n[active]\nprovider = "openai"\n[openai]\napi_key = "test"\n'
            f'base_url = "http://127.0.0.1:{server.server_port}"\nmodel = "gpt-4o"\n'
        )
        with serve_session(env={"ZHUYE_MODEL": "", "ZHUYE_PROVIDER": ""}) as session:
            def run_side():
                events = session.submit({"type": "start_side_channel", "agent": "default", "prompt": "测试模型"})
                started = first_event(events, "side_channel_started")
                assert started is not None, msg_types(events)
                sid = started["msg"]["id"]
                finished = session.wait_global(lambda e: e.get("msg", {}).get("type") == "side_channel_finished" and e["msg"].get("id") == sid, timeout=30)
                assert finished["msg"]["kind"] == "done", finished
            run_side()
            config = sandbox.home / ".zhuye/config.toml"
            config.write_text(config.read_text().replace('model = "gpt-4o"', 'model = "gpt-4o-mini"'))
            session.wait_global(lambda e: e.get("msg", {}).get("type") == "session_configured" and e["msg"].get("model") == "openai/gpt-4o-mini", timeout=30)
            run_side()
        assert [request["model"] for request in received] == ["gpt-4o", "gpt-4o-mini"]
    finally:
        server.shutdown()
        thread.join()
        server.server_close()


def test_A23_side_channel使用当前worktree(serve_session, mock_script, sandbox) -> None:
    """主会话切入 worktree 后,新侧通道写入新工作区而非原仓库。"""
    import subprocess
    from pathlib import Path

    def git(*args):
        return subprocess.run(["git", *args], cwd=sandbox.cwd, capture_output=True, text=True, check=True)

    git("init", "-q")
    git("config", "user.email", "test@localhost")
    git("config", "user.name", "test")
    (sandbox.cwd / "base.txt").write_text("base\n")
    git("add", "base.txt")
    git("commit", "-q", "-m", "base")
    script = mock_script([
        '{"type":"tool_call","name":"EnterWorktree","args":{"branch":"side-worktree"}}',
        '{"type":"text","text":"entered"}',
        '{"type":"tool_call","name":"write","args":{"path":"side-output.txt","content":"side"}}',
        '{"type":"text","text":"side done"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as session:
        events = session.submit({"type": "user_input", "items": [{"type": "text", "text": "进入 worktree"}]})
        end = first_event(events, "tool_call_end")
        assert end is not None and end["msg"]["is_error"] is False, events
        worktree = Path(end["msg"]["output"]["metadata"]["worktree_path"])
        assert worktree != sandbox.cwd.resolve()
        events = session.submit({"type": "start_side_channel", "agent": "default", "prompt": "写文件"})
        started = first_event(events, "side_channel_started")
        assert started is not None, msg_types(events)
        sid = started["msg"]["id"]
        finished = session.wait_global(lambda e: e.get("msg", {}).get("type") == "side_channel_finished" and e["msg"].get("id") == sid, timeout=30)
        assert finished["msg"]["kind"] == "done", finished
        assert finished["msg"]["summary"] == "side done"
        assert (worktree / "side-output.txt").read_text() == "side"
        assert not (sandbox.cwd / "side-output.txt").exists()


def test_A22_side_channel提问与计划审批回执(serve_session, mock_script) -> None:
    """侧通道的文本提问、选择题和计划审批都可经主会话响应。"""
    script = mock_script([
        '{"type":"tool_call","name":"ask_user","args":{"prompt":"你的称呼?"}}',
        json.dumps({"type": "tool_call", "name": "ask_user_question", "args": {"questions": [{"header": "选择", "question": "选哪项?", "options": [{"label": "甲", "description": "第一项"}, {"label": "乙", "description": "第二项"}], "multi_select": False}]}}),
        '{"type":"tool_call","name":"EnterPlanMode","args":{"task":"计划任务"}}',
        '{"type":"tool_call","name":"ExitPlanMode","args":{}}',
        '{"type":"text","text":"interactions done"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script), "ZHUYE_APPROVALS": "1"}) as session:
        events = session.submit({"type": "start_side_channel", "agent": "default", "prompt": "依次提问并制定计划"})
        started = first_event(events, "side_channel_started")
        assert started is not None, msg_types(events)
        sid = started["msg"]["id"]
        question = session.wait_global(lambda e: e.get("msg", {}).get("type") == "ask_user_input", timeout=20)["msg"]
        assert question["prompt"] == "你的称呼?"
        session.submit({"type": "ask_user_input_response", "id": question["request_id"], "text": "小烛"})
        choice = session.wait_global(lambda e: e.get("msg", {}).get("type") == "ask_user_question", timeout=20)["msg"]
        assert choice["questions"][0]["question"] == "选哪项?"
        session.submit({"type": "ask_user_question_response", "id": choice["request_id"], "answers": {"answers": [{"selected": [0]}]}})
        plan = session.wait_global(lambda e: e.get("msg", {}).get("type") == "plan_request", timeout=20)["msg"]
        assert plan["task"] == "计划任务"
        session.submit({"type": "plan_approval", "id": plan["plan_id"], "choice": "manual_approve"})
        ready = session.wait_global(lambda e: e.get("msg", {}).get("type") == "plan_ready", timeout=20)["msg"]
        assert ready["plan_id"]
        session.submit({"type": "plan_approval", "id": ready["plan_id"], "choice": "manual_approve"})
        finished = session.wait_global(lambda e: e.get("msg", {}).get("type") == "side_channel_finished" and e["msg"].get("id") == sid, timeout=20)
        assert finished["msg"]["kind"] == "done", finished
        assert finished["msg"]["summary"] == "interactions done"
        approved = session.wait_global(lambda e: e.get("msg", {}).get("type") == "plan_approved" and e["msg"].get("plan_id") == ready["plan_id"], timeout=5)
        assert approved["id"] == ""


def test_A21_side_channel继承Deny模式(serve_session, mock_script, sandbox) -> None:
    """主会话拒绝审批时,侧通道不得绕过会话模式执行写入。"""
    script = mock_script([
        '{"type":"tool_call","name":"write","args":{"path":"denied-mode.txt","content":"blocked"}}',
        '{"type":"text","text":"done"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script), "ZHUYE_APPROVALS": "1"}) as session:
        session.submit({"type": "set_permission_mode", "mode": "deny"})
        events = session.submit({"type": "start_side_channel", "agent": "default", "prompt": "尝试写入"})
        started = first_event(events, "side_channel_started")
        assert started is not None, msg_types(events)
        sid = started["msg"]["id"]
        finished = session.wait_global(lambda e: e.get("msg", {}).get("type") == "side_channel_finished" and e["msg"].get("id") == sid, timeout=20)
        assert finished["msg"]["kind"] == "done", finished
        assert finished["msg"]["summary"] == "done", finished
        assert not (sandbox.cwd / "denied-mode.txt").exists()
        assert not any(e.get("msg", {}).get("type") == "approval_request" for e in session.all_events())


def test_A20_side_channel审批请求与回执(serve_session, mock_script, sandbox) -> None:
    """侧通道审批请求进入主会话,拒绝与批准分别控制实际写入。"""
    sandbox.write_config('''
[[permissions.rule]]
tool = "write"
action = "ask"
''')
    script = mock_script([
        '{"type":"tool_call","name":"write","args":{"path":"denied-side.txt","content":"denied"}}',
        '{"type":"text","text":"denied done"}',
        '{"type":"tool_call","name":"write","args":{"path":"approved-side.txt","content":"approved"}}',
        '{"type":"text","text":"approved done"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script), "ZHUYE_APPROVALS": "1"}) as session:
        for name, decision, summary in [
            ("denied-side.txt", {"deny": {"reason": "not allowed"}}, "denied done"),
            ("approved-side.txt", "approve", "approved done"),
        ]:
            events = session.submit({"type": "start_side_channel", "agent": "default", "prompt": "测试审批"})
            started = first_event(events, "side_channel_started")
            assert started is not None, msg_types(events)
            sid = started["msg"]["id"]
            approval = session.wait_global(lambda e: e.get("msg", {}).get("type") == "approval_request" and e["msg"].get("kind", {}).get("args", {}).get("path") == name, timeout=20)
            request = approval["msg"]
            assert request["kind"]["type"] == "tool"
            assert request["kind"]["tool_name"] == "write"
            assert request["request_id"]
            assert not (sandbox.cwd / name).exists()
            session.submit({"type": "tool_approval", "id": request["request_id"], "decision": decision})
            finished = session.wait_global(lambda e: e.get("msg", {}).get("type") == "side_channel_finished" and e["msg"].get("id") == sid, timeout=20)
            assert finished["msg"]["kind"] == "done", finished
            assert finished["msg"]["summary"] == summary, finished
        assert not (sandbox.cwd / "denied-side.txt").exists()
        assert (sandbox.cwd / "approved-side.txt").read_text() == "approved"


def test_A19_side_channel实时继承Plan限制(serve_session, mock_script, sandbox) -> None:
    """主会话切到 Plan 后拒绝侧通道写入,切回 Auto 后允许正常写入。"""
    script = mock_script([
        '{"type":"tool_call","name":"write","args":{"path":"plan-denied.txt","content":"blocked"}}',
        '{"type":"text","text":"first done"}',
        '{"type":"tool_call","name":"write","args":{"path":"auto-allowed.txt","content":"allowed"}}',
        '{"type":"text","text":"second done"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as session:
        for mode, expected in [("plan", "first done"), ("auto", "second done")]:
            session.submit({"type": "set_permission_mode", "mode": mode})
            events = session.submit({"type": "start_side_channel", "agent": "default", "prompt": "尝试写入"})
            started = first_event(events, "side_channel_started")
            assert started is not None, msg_types(events)
            sid = started["msg"]["id"]
            finished = session.wait_global(lambda e: e.get("msg", {}).get("type") == "side_channel_finished" and e["msg"].get("id") == sid, timeout=30)
            assert finished["msg"]["kind"] == "done", finished
            assert finished["msg"]["summary"] == expected, finished
        assert not (sandbox.cwd / "plan-denied.txt").exists()
        assert (sandbox.cwd / "auto-allowed.txt").read_text() == "allowed"


def test_A18_side_channel继承自定义脱敏(serve_session, sandbox) -> None:
    """侧通道工具结果发回模型前必须应用用户配置的脱敏规则。"""
    import threading
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    received = []

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            received.append(request)
            if len(received) == 1:
                delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "echo", "arguments": json.dumps({"text": "CANDLESECRET123"})}}]}
                finish = "tool_calls"
            else:
                delta = {"content": "done"}
                finish = "stop"
            payload = {"id": "test", "model": "gpt-4o", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
            body = ("data: " + json.dumps(payload) + "\n\ndata: [DONE]\n\n").encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        sandbox.write_config(
            f'\n[active]\nprovider = "openai"\n[openai]\napi_key = "test"\n'
            f'base_url = "http://127.0.0.1:{server.server_port}"\n'
            "[sanitize]\nmarker = '<MASKED>'\nextra_patterns = ['CANDLESECRET123']\n"
        )
        with serve_session(env={"ZHUYE_MODEL": "openai/gpt-4o"}) as session:
            events = session.submit({"type": "start_side_channel", "agent": "default", "prompt": "测试脱敏"})
            started = first_event(events, "side_channel_started")
            assert started is not None, msg_types(events)
            sid = started["msg"]["id"]
            finished = session.wait_global(lambda e: e.get("msg", {}).get("type") == "side_channel_finished" and e["msg"].get("id") == sid, timeout=30)
            assert finished["msg"]["kind"] == "done", finished
        assert len(received) == 2, received
        results = [m for m in received[1]["messages"] if m.get("role") == "tool"]
        assert len(results) == 1, results
        assert results[0]["tool_call_id"] == "call_1"
        assert results[0]["content"] == "<MASKED>"
    finally:
        server.shutdown()
        thread.join()
        server.server_close()


def test_A14_activity审计落盘(serve_session, mock_script, sandbox) -> None:
    """一轮对话后 `~/.zhuye/activity/activity.jsonl` 落盘:
    `turnStarted` / `turnComplete` 记录(camelCase 字段级)。

    依据:crates/resources/zhuye-telemetry/src/activity.rs(ActivityLogger
    + pump_events)+ headless 的 subscribe_session 接线。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"audit-me"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "审计一轮"}]},
            timeout=DRAIN_TIMEOUT,
        )
    time.sleep(0.8)
    audit = sandbox.home / ".zhuye/activity/activity.jsonl"
    assert audit.exists(), f"activity.jsonl 未落盘:{audit}"
    import json as _json

    records = [_json.loads(line) for line in audit.read_text().splitlines() if line.strip()]
    types = [r.get("kind") for r in records]
    assert "turn_started" in types, f"缺 turn_started:{types}"
    assert "turn_complete" in types, f"缺 turn_complete:{types}"
    # 字段级:camelCase + actor 归属(turn 类 = user)。
    started = next(r for r in records if r.get("kind") == "turn_started")
    for f in ("id", "tsMs", "actor", "summary", "level"):
        assert f in started, f"activity 记录缺 {f}:{started}"
    assert started["actor"]["actorId"] == "user", f"turn 应归 user actor:{started}"


def test_A25_activity轮转保留记录(serve_session, mock_script, sandbox) -> None:
    """多次达到轮转阈值后,各归档与新写入事件均可解析。"""
    require_binary()
    directory = sandbox.home / ".zhuye" / "activity"
    directory.mkdir(parents=True, exist_ok=True)
    active = directory / "activity.jsonl"
    script = mock_script(['{"type":"text","text":"audit"}'])
    for marker in ("archive-one", "archive-two"):
        with active.open("a") as file:
            file.write(json.dumps({"kind": "other", "summary": marker, "padding": "x" * (1024 * 1024)}) + "\n")
        with serve_session(env=sandbox.mock_env(script)) as session:
            events = session.submit({"type": "user_input", "items": [{"type": "text", "text": "审计"}]})
            assert first_event(events, "turn_complete") is not None, msg_types(events)
    archives = sorted(directory.glob("activity.*.jsonl"))
    assert len(archives) == 2, archives
    rows = [[json.loads(line) for line in path.read_text().splitlines()] for path in archives]
    assert {row.get("summary") for group in rows for row in group} >= {"archive-one", "archive-two"}
    assert all(any(row.get("kind") == "session_configured" for row in group) for group in rows)


def test_A26_activity关闭不写审计(serve_session, mock_script, sandbox) -> None:
    """关闭审计时,serve 的全部事件路径均不写入审计文件。"""
    require_binary()
    sandbox.write_config("\n[telemetry]\nactivity = false\n")
    script = mock_script(['{"type":"text","text":"done"}'])
    with serve_session(env=sandbox.mock_env(script)) as session:
        events = session.submit({"type": "user_input", "items": [{"type": "text", "text": "审计关闭"}]})
        assert first_event(events, "turn_complete") is not None, msg_types(events)
    assert not (sandbox.home / ".zhuye" / "activity" / "activity.jsonl").exists()


def test_A27_activity跨进程轮转(serve_session, mock_script, sandbox) -> None:
    """并发进程轮转后都继续写入当前文件,且依据文件实际长度轮转。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"warmup"}', '{"type":"text","text":"done"}'])
    directory = sandbox.home / ".zhuye" / "activity"
    with serve_session(env=sandbox.mock_env(script)) as first, serve_session(env=sandbox.mock_env(script)) as second:
        for session in (first, second):
            events = session.submit({"type": "user_input", "items": [{"type": "text", "text": "预热"}]})
            assert first_event(events, "turn_complete") is not None, msg_types(events)
        active = directory / "activity.jsonl"
        with active.open("a") as file:
            file.write(json.dumps({"kind": "other", "summary": "并发标记", "padding": "x" * (1024 * 1024)}) + "\n")
        for session in (first, second):
            events = session.submit({"type": "user_input", "items": [{"type": "text", "text": "并发"}]})
            assert first_event(events, "turn_complete") is not None, msg_types(events)
    archives = list(directory.glob("activity.*.jsonl"))
    assert len(archives) == 1, archives
    archived = [json.loads(line) for line in archives[0].read_text().splitlines()]
    current = [json.loads(line) for line in active.read_text().splitlines()]
    assert any(row.get("summary") == "并发标记" for row in archived)
    assert any(row.get("kind") == "turn_started" for row in current), current


def test_D13_整段消息agent_message变体(serve_session, mock_script) -> None:
    """非流式 provider 形态:整段 agent_message 事件不经 delta。

    文本等价进入历史 —— 下一轮 request_echo 的 messages 快照应含该
    assistant 消息全文(与 delta 路径落点一致)。
    """
    require_binary()
    script = mock_script([
        '{"type":"agent_message","text":"整段回复标记-D13"}',
        '{"type":"request_echo"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as session:
        turn1 = session.submit({"type": "user_input", "items": [{"type": "text", "text": "用整段消息回复我"}]}, timeout=DRAIN_TIMEOUT)
        turn2 = session.submit({"type": "user_input", "items": [{"type": "text", "text": "复述上文"}]}, timeout=DRAIN_TIMEOUT)

    # turn1:恰好一条 agent_message,全文一次到位,不存在任何 delta。
    types = msg_types(turn1)
    assert "agent_message" in types, types
    assert "agent_message_delta" not in types, types
    msg = first_event(turn1, "agent_message")
    assert msg is not None and msg["msg"]["text"] == "整段回复标记-D13", msg
    # 终态序列完整:agent_message 最先,turn_complete 收尾(token_count
    # 可能有多条:一条带模型 / provider 维度,一条汇总)。
    order = [t for t in types if t in ("agent_message", "token_count", "turn_complete")]
    assert order[0] == "agent_message" and order[-1] == "turn_complete", types
    assert "token_count" in order, types

    # turn2:整段文本等价累积进模型上下文(request_echo messages 快照)。
    payload = json.loads(
        "".join(
            ev["msg"].get("delta", "") + ev["msg"].get("text", "")
            for ev in turn2
            if ev["msg"]["type"] in ("agent_message_delta", "agent_message")
        )
    )
    assistant_texts = [m["text"] for m in payload["messages"] if m["role"] == "assistant"]
    assert any("整段回复标记-D13" in t for t in assistant_texts), payload["messages"]


def test_A28_shutdown_op_优雅退出(serve_session, mock_script) -> None:
    """`shutdown` op → `shutdown_complete` 优雅退出,无 error 事件。

    该路径此前仅由 fixture close() 隐式覆盖;此处按 serve_ops.sh 语义
    显式断言事件形状与静默语义(非 turn op:无 turn 事件)。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"bye"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit({"type": "user_input", "items": [{"type": "text", "text": "一轮"}]})
        evs = s.submit({"type": "shutdown"})
    types = msg_types(evs)
    assert "shutdown_complete" in types, types
    assert "submission_closed" in types, types
    assert "error" not in types and "turn_complete" not in types, types


def test_A29_plan审批revise_拒绝分支(serve_session, mock_script) -> None:
    """plan_approval revise → `plan_rejected` 指向原 plan_id,留在 plan 模式。

    二次 exit_plan_mode 生成独立 plan_id;会话保持可用。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit({"type": "enter_plan_mode", "task": "revise probe"})
        evs1 = s.submit({"type": "exit_plan_mode"})
        ready = first_event(evs1, "plan_ready")
        assert ready is not None and ready["msg"].get("plan_id"), msg_types(evs1)
        pid1 = ready["msg"]["plan_id"]

        # 实测时序:plan_rejected 携带 approval 的 sub id,但在该 sub 的
        # submission_closed **之后**到达(collect 在首个终态即返回,原
        # serve_ops.sh 的文件轮询掩盖了这一点),因此这里轮询全量事件流。
        s.submit({"type": "plan_approval", "id": pid1, "choice": "revise"})
        deadline = time.monotonic() + 5.0
        rejected = None
        while time.monotonic() < deadline and rejected is None:
            rejected = next(
                (e for e in s.all_events()
                 if e.get("msg", {}).get("type") == "plan_rejected"),
                None,
            )
            if rejected is None:
                time.sleep(0.05)
        assert rejected is not None, "revise 未触发 plan_rejected"
        assert rejected["msg"].get("plan_id") == pid1, rejected["msg"]

        # 留在 plan 模式:未发生 accept_edits 权限切换。
        accept = [
            e for e in s.global_events()
            if e.get("msg", {}).get("type") == "permission_mode_changed"
            and e["msg"].get("to") == "accept_edits"
        ]
        assert not accept, "revise 不应切出 plan 模式"

        # 二次 exit → 独立 plan_id。
        evs3 = s.submit({"type": "exit_plan_mode"})
        ready2 = first_event(evs3, "plan_ready")
        assert ready2 is not None and ready2["msg"].get("plan_id"), msg_types(evs3)
        assert ready2["msg"]["plan_id"] != pid1, "两次 plan_ready 应有独立 plan_id"

        # 会话存活。
        assert msg_types(s.submit({"type": "set_effort", "effort": "low"})) == ["submission_closed"]


def test_A30_inline_question_answer经user_input回答(serve_session, mock_script) -> None:
    """user_input 内联 `question_answer` item 与 dedicated Op 同语义。

    挂起的 ask_user_question gate 被解除、工具拿到答案 JSON、原 turn 完成;
    回答方 submission 自身也正常收尾(会话存活)。无 waiter 时静默丢弃、
    不产生 error(与 dedicated Op 一致,不在此重复断言)。
    """
    require_binary()
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "ask_user_question", "args": {"questions": [
            {"header": "选择", "question": "选哪项?",
             "options": [{"label": "甲", "description": "第一项"},
                         {"label": "乙", "description": "第二项"}],
             "multi_select": False}]}}),
        '{"type":"text","text":"answer consumed"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script), "ZHUYE_APPROVALS": "1"}) as session:
        seen: dict[str, Any] = {}

        def _on_event(ev: dict[str, Any]) -> None:
            m = ev.get("msg", {})
            if m.get("type") == "ask_user_question" and "request_id" in m and not seen:
                seen.update(m)
                # 内联回答:走 user_input + question_answer item。
                session.send({"type": "user_input", "items": [
                    {"type": "question_answer", "request_id": m["request_id"],
                     "answers": {"answers": [{"selected": [0]}]}},
                ]})

        events = session.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "问我"}]},
            on_event=_on_event,
        )
        assert seen, f"未见 ask_user_question:{msg_types(events)}"

    # 原回合:工具解除阻塞并拿到答案 JSON,turn 完成。
    end = first_event(events, "tool_call_end")
    assert end is not None and not end["msg"]["is_error"], msg_types(events)
    out_json = json.dumps(end["msg"]["output"], ensure_ascii=False)
    assert '"selected": [0]' in out_json or '"selected":[0]' in out_json, out_json[:300]
    assert "turn_complete" in msg_types(events)

    # 回答方 submission 自身也产生一个 turn_complete(会话存活,共 2 个)。
    deadline = time.monotonic() + 15.0
    while time.monotonic() < deadline:
        if msg_types(session.all_events()).count("turn_complete") >= 2:
            break
        time.sleep(0.05)
    assert msg_types(session.all_events()).count("turn_complete") >= 2
    assert "error" not in msg_types(session.all_events())


def test_A31_local_image进入模型上下文(serve_session, mock_script, sandbox) -> None:
    """`local_image` item 就地读盘转内联 Image 块,与文本合并进同一 user 消息。"""
    require_binary()
    (sandbox.cwd / "shot.png").write_bytes(b"\x89PNG\r\n\x1a\n")
    script = mock_script(['{"type":"request_echo"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as session:
        evs = session.submit({"type": "user_input", "items": [
            {"type": "text", "text": "看这张图"},
            {"type": "local_image", "path": "shot.png"},
        ]}, timeout=DRAIN_TIMEOUT)
    text = "".join(
        ev["msg"].get("delta", "") + ev["msg"].get("text", "")
        for ev in evs
        if ev["msg"]["type"] in ("agent_message_delta", "agent_message")
    )
    payload = json.loads(text)
    user_texts = [m["text"] for m in payload["messages"] if m["role"] == "user"]
    assert any("[image: image/png" in t and "看这张图" in t for t in user_texts), payload["messages"]
