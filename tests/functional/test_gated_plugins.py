"""能力覆盖层 · 门控型 bundled 插件(兼容性金测试 + 门控行为)。

直接驱动 `zhuye serve` 原始 JSONL wire(不经过任何 SDK)。门控型插件
(形态见根 AGENTS.md「形态判断」决策矩阵第三行):能力本体留引擎
crate,注册由插件启用状态决定,判定单一事实源是
`zhuye-plugin::bundled_plugin_enabled`(默认开关表 BUNDLED_PLUGIN_DEFAULTS)。

覆盖:
- G1 默认态零变化(金测试):空 `[plugins]` 配置下 web_search /
  notebook_edit 照常注册 —— tool_search 命中(来源标注 builtin)、
  直调 web_search 报 BRAVE_API_KEY 缺失(执行错误)而非 tool not
  found(未注册错误),证明「默认开 + 外部用法零影响」;
- G2 插件禁用 = 注册门控:`[plugins].disabled_plugins` 写入后
  tool_search 不再命中(No tools matched)、直调报
  `tool not found: web_search`;同配置下 notebook_edit 不受影响
  (插件级粒度);
- G3 分层叠加(可见性过滤 ≠ 注册门控):`[active].disallowed_tools =
  ["web_search"]` 只裁 LLM 可见性、不动注册面 —— tool_search 仍命中
  web_search(与 G2 的差异即两层语义分界);
- G4 verify-hooks 双路径(插件路径):`enabled_plugins` 启用 + 失败
  校验命令(`exit 1`)→ Stop 被否决两轮(MAX_ATTEMPTS=2)后续作放行,
  单 turn 消费 3 条 mock 回复;未启用的对照组只消费 1 条。

运行:tests/run_all.sh functional,或
  ZHUYE_BIN=... python3 -m pytest tests/functional/test_gated_plugins.py -v
"""

from __future__ import annotations

import json
from typing import Any

import pytest

from wire import DRAIN_TIMEOUT, require_binary

pytestmark = pytest.mark.usefixtures("sandbox")


def _ends(events: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """按序取全部 tool_call_end 事件。"""
    return [e["msg"] for e in events if e["msg"]["type"] == "tool_call_end"]


def _text(msg: dict[str, Any]) -> str:
    """tool_call_end 输出里的文本拼接。"""
    content = msg.get("output", {}).get("content", [])
    return "".join(c.get("text", "") for c in content if isinstance(c, dict))


def _agent_text(events: list[dict[str, Any]]) -> str:
    """聚合事件流里模型回复文本(delta + 整段)—— 统计 mock 消费条数。"""
    parts: list[str] = []
    for ev in events:
        m = ev.get("msg", {})
        if m.get("type") == "agent_message_delta":
            parts.append(m.get("delta", ""))
        elif m.get("type") == "agent_message":
            parts.append(m.get("text", ""))
    return "".join(parts)


def _write_config(sandbox, body: str) -> None:
    """覆写沙箱 HOME 的 config.toml(保留 watcher 要求的文件存在性)。"""
    (sandbox.home / ".zhuye" / "config.toml").write_text(body, encoding="utf-8")


def test_G1_默认态门控插件零变化(serve_session, mock_script, sandbox) -> None:
    """空 [plugins] 配置:web_search / notebook_edit 照常注册且可发现。

    这是「默认开 + 外部用法零影响」的金测试 —— 插件化前后该配置下的
    wire 行为必须逐字段一致。
    """
    require_binary()
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "tool_search",
                    "args": {"query": "web_search"}}),
        json.dumps({"type": "tool_call", "name": "web_search",
                    "args": {"query": "rust async"}}),
        json.dumps({"type": "tool_call", "name": "tool_search",
                    "args": {"query": "notebook"}}),
        json.dumps({"type": "text", "text": "完成"}),
    ])
    with serve_session(env={
        "ZHUYE_MODEL": "mock",
        "ZHUYE_MOCK_SCRIPT": str(script),
        # 空串在工具内被过滤 → 走「注册了但缺 key」分支(免疫宿主环境)。
        "BRAVE_API_KEY": "",
    }) as session:
        events = session.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "检查工具面"}]},
            timeout=DRAIN_TIMEOUT)
    ends = _ends(events)
    assert len(ends) == 3, f"应恰有 3 个 tool_call_end:{[e['msg']['type'] for e in events]}"
    # tool_search 命中 web_search,来源标注 builtin(字段级)。
    assert any(line.startswith("web_search [builtin]:") for line in _text(ends[0]).splitlines()), \
        f"tool_search 应命中 web_search [builtin]:{_text(ends[0])!r}"
    assert ends[0]["is_error"] is False
    # 直调 web_search:已注册但缺 key → 执行错误文案;绝不能是未注册错误。
    assert ends[1]["is_error"] is True, ends[1]
    assert "BRAVE_API_KEY not set" in _text(ends[1]), _text(ends[1])
    assert "tool not found" not in _text(ends[1]), "默认态不应出现未注册错误"
    # notebook_edit 同样照常注册。
    assert any(line.startswith("notebook_edit [builtin]:") for line in _text(ends[2]).splitlines()), \
        f"tool_search 应命中 notebook_edit [builtin]:{_text(ends[2])!r}"


def test_G2_插件禁用即不注册(serve_session, mock_script, sandbox) -> None:
    """disabled_plugins = ["web-search@inline"] → 工具不进注册表。

    与 G3 对照:注册门控(插件级)是彻底移除 —— tool_search 搜不到、
    调用报 tool not found;同配置下 notebook 插件不受影响(粒度 = 插件)。
    """
    require_binary()
    _write_config(sandbox, "\n".join([
        "[hooks]",
        "enabled = []",
        "",
        "[plugins]",
        'disabled_plugins = ["web-search@inline"]',
        "",
    ]))
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "tool_search",
                    "args": {"query": "web_search"}}),
        json.dumps({"type": "tool_call", "name": "web_search",
                    "args": {"query": "rust"}}),
        json.dumps({"type": "tool_call", "name": "tool_search",
                    "args": {"query": "notebook"}}),
        json.dumps({"type": "text", "text": "完成"}),
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as session:
        events = session.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "检查禁用"}]},
            timeout=DRAIN_TIMEOUT)
    ends = _ends(events)
    assert len(ends) == 3
    # 注册门控:搜不到(精确文案)。
    assert _text(ends[0]) == "No tools matched query 'web_search'", _text(ends[0])
    # 调用报未注册(core 统一文案)。
    assert ends[1]["is_error"] is True, ends[1]
    assert "tool not found: web_search" in _text(ends[1]), _text(ends[1])
    # 粒度 = 插件级:notebook 不受影响。
    assert any(line.startswith("notebook_edit [builtin]:") for line in _text(ends[2]).splitlines()), \
        f"notebook_edit 不应被 web-search 禁用波及:{_text(ends[2])!r}"


def test_G3_可见性过滤不动注册面(serve_session, mock_script, sandbox) -> None:
    """[active].disallowed_tools 只裁 LLM 可见性 —— 注册面不变。

    与 G2 对照:同样「关掉 web_search」,disallowed_tools 路径下
    tool_search 仍能命中(工具已注册),证明两层入口语义分界。
    """
    require_binary()
    _write_config(sandbox, "\n".join([
        "[hooks]",
        "enabled = []",
        "",
        "[active]",
        'disallowed_tools = ["web_search"]',
        "",
    ]))
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "tool_search",
                    "args": {"query": "web_search"}}),
        json.dumps({"type": "text", "text": "完成"}),
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as session:
        events = session.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "检查过滤"}]},
            timeout=DRAIN_TIMEOUT)
    ends = _ends(events)
    assert len(ends) == 1
    # 可见性过滤 ≠ 注册门控:注册面仍含 web_search。
    assert any(line.startswith("web_search [builtin]:") for line in _text(ends[0]).splitlines()), \
        f"disallowed_tools 不应把 web_search 移出注册表:{_text(ends[0])!r}"


def test_G4_verify_hooks_插件路径否决与续作(serve_session, mock_script, sandbox) -> None:
    """enabled_plugins 启用 verify-hooks + 失败校验命令 → Stop 两轮否决。

    对照组(未启用):单 turn 只消费 1 条 mock 回复即结束;
    插件路径(启用):Stop 被否决 MAX_ATTEMPTS(2)次,共消费 3 条
    回复后 attempt 到顶放行 —— 两条路径的事件流差异即双路径生效证据。
    """
    require_binary()

    def run(config_body: str, expect_consumed: str) -> None:
        _write_config(sandbox, config_body)
        script = mock_script([
            json.dumps({"type": "text", "text": "第一次"}),
            json.dumps({"type": "text", "text": "第二次"}),
            json.dumps({"type": "text", "text": "第三次"}),
        ])
        with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as session:
            events = session.submit(
                {"type": "user_input", "items": [{"type": "text", "text": "收尾"}]},
                timeout=DRAIN_TIMEOUT)
        assert _agent_text(events) == expect_consumed, \
            f"mock 回复消费量不符:{_agent_text(events)!r}"

    # 对照组:未启用 verification(config 显式空)→ 1 条即结束。
    run("\n".join([
        "[hooks]",
        "enabled = []",
        "",
        "[hooks.verification]",
        'test_command = "exit 1"',
        "",
    ]), expect_consumed="第一次")

    # 插件路径:enabled_plugins 写 verify-hooks(无安装动作,判定读配置)
    # → 注入 verification → 两轮否决 + 第三轮放行。
    run("\n".join([
        "[hooks]",
        "enabled = []",
        "",
        "[hooks.verification]",
        'test_command = "exit 1"',
        "",
        "[plugins]",
        'enabled_plugins = ["verify-hooks@inline"]',
        "",
    ]), expect_consumed="第一次第二次第三次")
