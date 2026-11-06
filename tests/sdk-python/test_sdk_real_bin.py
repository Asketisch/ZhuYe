"""Python SDK 绑定层:SDK 客户端自身的行为(不测服务端语义)。

与能力覆盖层(`tests/functional/`,原始 wire 打真二进制)互补:
本文件驱动**真二进制**,但断言的是 **Python SDK 怎么反应** —— 迭代器何时
收尾、handler 怎么被调、回执怎么写、close 怎么收尾。

覆盖:
- K1 握手:spawn 返回即就绪(收到 `session_configured` 才返回);
- K2 单轮 / 多轮 prompt:文本回流、多轮脚本按序消耗;
- K3 远程工具:注册 → LLM 调用 → 本地 handler 执行 → 回执 → turn_complete;
- K4 非 turn op 收尾:`submit_op` 迭代器以 `submission_closed` 结束;
- K5 interrupt 打断:收到 `turn_aborted`,原 turn 随后 `turn_complete`;
- K6 close 幂等;
- K7 坏二进制:握手超时/EOF 时抛 `RuntimeError`;
- K8 handler 抛异常:SDK 转成 `is_error=true` 的错误回执(不让 turn 挂死)。

运行:tests/run_all.sh sdk-python,或
  ZHUYE_BIN=... PYTHONPATH=sdks/python python3 -m pytest tests/sdk-python -v
"""

from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Any

import pytest

from sdkharness import ZHUYE_BIN, drain, make_options, msg_types

from zhuye import ContentBlock, ZhuyeAgent, ToolOutput


# ── K1 握手 ─────────────────────────────────────────────────────────────


def test_K1_spawn_握手成功即返回(env_home: Path) -> None:
    """spawn 内部等待 `session_configured` 才返回;超时/失败会直接抛。"""
    agent = ZhuyeAgent.spawn(make_options())
    agent.close()


# ── K2 单轮 / 多轮 prompt ────────────────────────────────────────────────


def test_K2a_单轮_prompt_文本回流(env_home: Path) -> None:
    """prompt() 聚合 delta 返回最终文本。"""
    agent = ZhuyeAgent.spawn(make_options(['{"type":"text","text":"py-e2e-reply"}']))
    try:
        assert agent.prompt("第一轮") == "py-e2e-reply"
    finally:
        agent.close()


def test_K2b_多轮_prompt_上下文保持(env_home: Path) -> None:
    """两轮拿到 mock 脚本的第 1、2 行(脚本按模型调用顺序消耗)。"""
    agent = ZhuyeAgent.spawn(
        make_options(
            [
                '{"type":"text","text":"first-reply"}',
                '{"type":"text","text":"second-reply"}',
            ]
        )
    )
    try:
        assert agent.prompt("第一轮") == "first-reply"
        assert agent.prompt("第二轮") == "second-reply"
    finally:
        agent.close()


# ── K3 远程工具 ─────────────────────────────────────────────────────────


def test_K3_注册工具_本地执行_回执(env_home: Path) -> None:
    """注册 → LLM 调用 → 客户端 handler 执行 → 回执 → turn_complete。"""
    calls: list[dict[str, Any]] = []

    def handler(args: dict[str, Any]) -> ToolOutput:
        calls.append(args)
        return ToolOutput(
            content=[ContentBlock(type="text", text=f"echo:{args['q']}")],
            is_error=False,
            metadata={},
            elapsed_ms=0,
        )

    agent = ZhuyeAgent.spawn(
        make_options(
            [
                '{"type":"tool_call","name":"sdk_echo","args":{"q":"ping"}}',
                '{"type":"text","text":"tool-done"}',
            ]
        )
    )
    try:
        agent.register_tool("sdk_echo", "echo back", {"type": "object"}, handler)
        evs = drain(agent.submit("调工具"))
        assert calls == [{"q": "ping"}], f"handler 入参异常:{calls!r}"
        assert "turn_complete" in msg_types(evs), f"turn 未收尾:{msg_types(evs)}"
    finally:
        agent.close()


def test_K13_工具图片回执经SDK往返(env_home: Path) -> None:
    """SDK 能发送图片工具回执,并在事件迭代器读取同一字节。"""
    calls = []
    def handler(args: dict[str, Any]) -> ToolOutput:
        calls.append(args)
        return ToolOutput(content=[ContentBlock(type="image", data=[137, 80, 78, 71], mime_type="image/png")], is_error=False, metadata={}, elapsed_ms=0)
    agent = ZhuyeAgent.spawn(make_options([
        '{"type":"tool_call","name":"sdk_image","args":{}}',
        '{"type":"text","text":"done"}',
    ]))
    try:
        agent.register_tool("sdk_image", "图片", {"type":"object"}, handler)
        events = drain(agent.submit("图片"))
        assert calls == [{}]
        end = next(event["msg"] for event in events if event["msg"]["type"] == "tool_call_end")
        assert end["output"]["content"] == [{"type":"image", "data":[137, 80, 78, 71], "mime_type":"image/png"}]
        assert "turn_complete" in msg_types(events)
    finally:
        agent.close()


# ── K4 非 turn op 收尾 ──────────────────────────────────────────────────


def test_K4_非turn_op_迭代器以submission_closed收尾(env_home: Path) -> None:
    """`submit_op` 发非 turn 操作:迭代器必须以 `submission_closed` 结束。

    没有它,SDK 的迭代器会永久挂起(v1.3 起 serve 在 per-turn 通道排空后
    发这条收尾标记)。
    """
    agent = ZhuyeAgent.spawn(make_options(['{"type":"text","text":"t1"}']))
    try:
        evs = drain(agent.submit_op({"type": "set_effort", "effort": "high"}))
        assert msg_types(evs) == ["submission_closed"], f"收尾异常:{msg_types(evs)}"
    finally:
        agent.close()


def test_K4b_set_effort新档位max_ultra_真二进制透传(env_home: Path) -> None:
    """v1.10 功能4:additive 新档位 `max` / `ultra` 经真二进制透传。

    协议 additive 变更的绑定层回归:引擎对新档位与 high 同型静默
    (submission_closed 收尾),SDK 与 serve 都不得因新增字面量崩溃;
    `thread_settings.effort` 形态同样接受新档位并正常收尾 turn。
    """
    agent = ZhuyeAgent.spawn(make_options(['{"type":"text","text":"t1"}']))
    try:
        for effort in ("max", "ultra"):
            evs = drain(agent.submit_op({"type": "set_effort", "effort": effort}))
            assert msg_types(evs) == ["submission_closed"], f"{effort} 收尾异常:{msg_types(evs)}"
        # thread_settings 形态:随 user_input 下发,turn 正常收尾。
        evs = drain(
            agent.submit_op(
                {
                    "type": "user_input",
                    "items": [{"type": "text", "text": "hi"}],
                    "thread_settings": {"effort": "ultra"},
                }
            )
        )
        assert "turn_complete" in msg_types(evs), f"turn 未收尾:{msg_types(evs)}"
    finally:
        agent.close()


# ── K5 interrupt ────────────────────────────────────────────────────────


def test_K5_interrupt_打断在飞turn(env_home: Path) -> None:
    """interrupt 打断停在远程工具等待上的 turn。

    wire 语义(实测):`turn_aborted` 落在**被打断的那个 turn** 的通道上
    (core 取消在飞回合 → 该回合以 `turn_aborted` 收尾);
    interrupt 这条 submission 自己的通道只收到收尾标记 `submission_closed`。
    """
    import threading

    handler_started = threading.Event()
    release_handler = threading.Event()

    def handler(args: dict[str, Any]) -> ToolOutput:
        handler_started.set()
        release_handler.wait(timeout=10)
        return ToolOutput(
            content=[ContentBlock(type="text", text="slow result")],
            is_error=False,
            metadata={},
            elapsed_ms=0,
        )

    agent = ZhuyeAgent.spawn(
        make_options(
            [
                '{"type":"tool_call","name":"slow_tool","args":{}}',
                '{"type":"text","text":"after"}',
            ]
        )
    )
    try:
        agent.register_tool("slow_tool", "block until released", {"type": "object"}, handler)
        turn_events = agent.submit("调慢工具")
        assert handler_started.wait(timeout=10), "handler 未被调用"
        # interrupt 自己的通道:只收到收尾标记。
        intr_events = drain(
            agent.submit_op({"type": "interrupt"}, submission_id="intr-1")
        )
        assert msg_types(intr_events) == ["submission_closed"], (
            f"interrupt 通道应只有收尾标记:{msg_types(intr_events)}"
        )
        release_handler.set()
        # 被打断的 turn 以 turn_aborted 收尾。
        turn_types = msg_types(drain(turn_events))
        assert "turn_aborted" in turn_types, f"被打断的 turn 未收到 turn_aborted:{turn_types}"
    finally:
        release_handler.set()
        agent.close()


# ── K6 close 幂等 ───────────────────────────────────────────────────────


def test_K6_close_幂等(env_home: Path) -> None:
    """二次 close 不抛错。"""
    agent = ZhuyeAgent.spawn(make_options())
    agent.close()
    agent.close()


# ── K7 坏二进制 ─────────────────────────────────────────────────────────


def test_K7_坏二进制_握手失败抛错(env_home: Path) -> None:
    """from_popen 路径:进程吐非 JSONL → 握手失败抛 `RuntimeError` 而非挂死。"""
    with pytest.raises(RuntimeError):
        ZhuyeAgent.from_popen(
            subprocess.Popen(
                ["/bin/sh", "-c", "echo not-jsonl && sleep 5"],
                stdout=subprocess.PIPE,
            ),
            3.0,
        )


# ── K8 handler 异常 ─────────────────────────────────────────────────────


def test_K8_handler抛异常_转错误回执(env_home: Path) -> None:
    """handler 抛异常 → SDK 把它转成 `is_error=true` 的文本回执(turn 不挂死)。

    依据:sdks/PROTOCOL.md §5(SDK 内 handler 抛异常时应转成 is_error=true
    的文本回执,让 LLM 看到错误而不是挂死)。
    """

    def handler(args: dict[str, Any]) -> ToolOutput:
        raise RuntimeError("handler-boom")

    agent = ZhuyeAgent.spawn(
        make_options(
            [
                '{"type":"tool_call","name":"boom_tool","args":{}}',
                '{"type":"text","text":"after-boom"}',
            ]
        )
    )
    try:
        agent.register_tool("boom_tool", "always raises", {"type": "object"}, handler)
        evs = drain(agent.submit("调会崩的工具"))
        ends = [ev for ev in evs if ev["msg"]["type"] == "tool_call_end"]
        assert ends, f"未见 tool_call_end:{msg_types(evs)}"
        assert ends[0]["msg"]["is_error"] is True, f"应回错误:{ends[0]['msg']}"
        text = "".join(
            c.get("text", "")
            for c in ends[0]["msg"].get("output", {}).get("content", [])
            if isinstance(c, dict)
        )
        assert "handler-boom" in text, f"错误文本未透传:{text!r}"
        assert "turn_complete" in msg_types(evs), f"turn 未收尾:{msg_types(evs)}"
    finally:
        agent.close()


def test_K9_全局事件公开读取口(env_home: Path) -> None:
    """全局事件通过公开 API 读取,不会混入 submission 的事件迭代器。"""
    agent = ZhuyeAgent.spawn(make_options())
    try:
        handshake = agent.next_global_event(timeout_secs=5)
        assert handshake["id"] == ""
        assert handshake["msg"]["type"] == "session_configured"
        drain(agent.submit_op({"type": "set_permission_mode", "mode": "plan"}))
        event = agent.next_global_event(timeout_secs=5)
        assert event["id"] == ""
        assert event["msg"]["type"] == "permission_mode_changed"
    finally:
        agent.close()


# ── K9 --plugin 显式插件加载 ─────────────────────────────────────────────


def test_K9_plugins选项_显式加载并上报PluginLoaded(env_home: Path) -> None:
    """`plugins=["demo@inline"]` 经 `--plugin` 传给 serve,启动时上报
    `plugin_loaded` 全局事件(覆盖 config,而非配置驱动)。

    依据:crates/runtime/zhuye-exec/src/headless.rs resolve_enabled_plugins、
    sdks/python/zhuye/agent.py _serve_args。
    """
    import os

    from zhuye import ZhuyeAgentOptions

    plugin_dir = Path(__file__).resolve().parents[2] / "plugins" / "plugin-demo"
    if not plugin_dir.is_dir():
        pytest.skip("plugins/plugin-demo 不存在(仓库不完整?)")

    # 先把 demo 插件装入隔离 HOME(等价 zhuye plugin install)。
    subprocess.run(
        [str(ZHUYE_BIN), "plugin", "install", str(plugin_dir), "--scope", "user"],
        env=dict(os.environ),
        check=True,
        capture_output=True,
        timeout=60,
    )

    opts = make_options()
    assert isinstance(opts, ZhuyeAgentOptions)
    opts.plugins = ["demo@inline"]
    agent = ZhuyeAgent.spawn(opts)
    try:
        # plugin_loaded 在 bootstrap 期发出,轮询全局队列(上限 ~15s)。
        # next_global_event 超时抛 queue.Empty(而非返回 None),逐次捕获。
        # 注意:队列里是整条 wire Event,类型在 ev["msg"]["type"]。
        import queue

        loaded = None
        for _ in range(15):
            try:
                ev = agent.next_global_event(timeout_secs=1)
            except queue.Empty:
                continue
            if ev is None:
                break
            if ev.get("msg", {}).get("type") == "plugin_loaded":
                loaded = ev["msg"]
                break
        assert loaded is not None, (
            f"未收到 plugin_loaded 全局事件:"
            f"qsize={agent._global_queue.qsize()} "
            f"reader_alive={agent._reader_thread.is_alive()} "
            f"proc={agent._proc.poll()}"
        )
        assert loaded.get("plugin") == "demo@inline", f"插件 id 不符:{loaded}"
        assert loaded.get("command_count", 0) >= 1, f"应有 demo:hello 命令:{loaded}"
    finally:
        agent.close()


# ── K10 side-channel 查询 Op(WP9 新能力面)──────────────────────────────


def test_K10_side_channel查询Op_真二进制返回status快照(env_home: Path) -> None:
    """真二进制:`submit_op({"type":"query_side_channels"})` → 真实 serve
    回 `side_channel_status`(channels 数组)+ `submission_closed` 收尾。

    与 K4(set_effort 静默)不同:这个 Op 有真实应答事件 —— exec 路径
    已装配 side-channel driver(WP13),查询不再走 "not configured" Error
    分支。纯逻辑层同型用例:sdks/python/tests/test_protocol.py(mock)。
    """
    agent = ZhuyeAgent.spawn(make_options(['{"type":"text","text":"t1"}']))
    try:
        evs = drain(agent.submit_op({"type": "query_side_channels"}))
        types = msg_types(evs)
        assert "side_channel_status" in types, f"缺 status 事件:{types}"
        status = next(e["msg"] for e in evs if e["msg"]["type"] == "side_channel_status")
        assert isinstance(status.get("channels"), list), f"channels 应为数组:{status}"
        assert types[-1] == "submission_closed", f"收尾异常:{types}"
    finally:
        agent.close()


def test_K11_side_channel终态进入全局事件队列(env_home: Path) -> None:
    """侧通道终态由 SDK 的全局事件接口读取,不会混入提交迭代器。"""
    agent = ZhuyeAgent.spawn(make_options(['{"type":"text","text":"side response"}']))
    try:
        agent.next_global_event(timeout_secs=5)
        events = drain(agent.submit_op({"type": "start_side_channel", "agent": "default", "prompt": "p"}))
        assert events[-1]["msg"]["type"] == "submission_closed"
        event = agent.next_global_event(timeout_secs=10)
        assert event["id"] == ""
        assert event["msg"]["type"] == "side_channel_finished"
    finally:
        agent.close()


def test_K12_side_channel审批经全局队列回执(env_home: Path) -> None:
    """侧通道审批请求通过公开全局队列读取,可用 SDK 审批接口回应。"""
    config = env_home / ".zhuye/config.toml"
    config.write_text(config.read_text() + '\n[[permissions.rule]]\ntool = "bash"\naction = "ask"\n')
    agent = ZhuyeAgent.spawn(make_options([
        '{"type":"tool_call","name":"bash","args":{"cmd":"echo test"}}',
        '{"type":"tool_call","name":"ask_user","args":{"prompt":"name?"}}',
        '{"type":"text","text":"done"}',
    ], extra_env={"ZHUYE_APPROVALS": "1"}))
    try:
        agent.next_global_event(timeout_secs=5)
        drain(agent.submit_op({"type": "start_side_channel", "agent": "default", "prompt": "p"}))
        event = agent.next_global_event(timeout_secs=10)
        assert event["id"] == ""
        assert event["msg"]["type"] == "approval_request"
        agent.approve(event["msg"]["request_id"], "approve")
        question = agent.next_global_event(timeout_secs=10)
        assert question["msg"]["type"] == "ask_user_input"
        drain(agent.submit_op({"type": "ask_user_input_response", "id": question["msg"]["request_id"], "text": "answer"}))
        finished = agent.next_global_event(timeout_secs=15)
        assert finished["msg"]["type"] == "side_channel_finished"
    finally:
        agent.close()
