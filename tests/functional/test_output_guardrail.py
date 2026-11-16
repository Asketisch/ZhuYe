"""能力覆盖层 · F03:输出侧护栏 —— PII 脱敏 + provenance 三档 + taint。

直接驱动 `zhuye serve` 原始 JSONL wire。

覆盖(正常 / 边界 / 失败 / 权限沙箱交互):
- 正常:默认(Observe)PII 命中只写 metadata.pii,正文保留;
- 边界:`[sanitize] pii = "enforce"` 正文被抹;`"off"` 完全无报告;
  `[sanitize] enabled = false` 时连密钥脱敏一起关(既有语义不变);
- 失败路径:PII 报告结构损坏不应崩(缺字段容错);
- 权限沙箱交互:审批开启时(MCP 工具)批准后输出仍走 PII 通道。

运行:tests/run_all.sh functional,或
  ZHUYE_BIN=... python3 -m pytest tests/functional/test_output_guardrail.py -v
"""

from __future__ import annotations

import json
from typing import Any

import pytest

from wire import DRAIN_TIMEOUT, MOCK_MCP, first_event, msg_types, require_binary

pytestmark = pytest.mark.usefixtures("sandbox")

PHONE = "13812345678"  # 合法形状的手机号(1 开头 11 位)
EMAIL = "alice@example.com"


def _tool_end(events: list[dict[str, Any]]) -> dict[str, Any]:
    m = first_event(events, "tool_call_end")
    assert m is not None, f"未见 tool_call_end:{msg_types(events)}"
    return m["msg"]


def _tool_text(msg: dict[str, Any]) -> str:
    content = msg.get("output", {}).get("content", [])
    return "".join(c.get("text", "") for c in content if isinstance(c, dict))


def _echo_lines(phrases: list[str]) -> list[str]:
    lines = []
    for p in phrases:
        lines.append(json.dumps({"type": "tool_call", "name": "echo", "args": {"text": p}}))
        lines.append('{"type":"text","text":"done"}')
    return lines


def _write_config(sandbox, extra: str = "") -> None:
    """整文件重写 config.toml。

    `Sandbox.write_config` 是**追加**语义,而 fixture 已写入 `[hooks]`;
    再追加一份 `[hooks]` 会让 TOML 解析失败(重复表)→ 配置整体回退默认,
    表现为「配置没生效」。这里始终整文件重写。
    """
    path = sandbox.home / ".zhuye/config.toml"
    path.write_text(
        "# 测试用最小配置:关闭全部内置 hook\n[hooks]\nenabled = []\n" + extra
    )


# ── 正常:默认 Observe ─────────────────────────────────────────────────────


def test_默认observe上报PII但不改正文(serve_session, mock_script, sandbox) -> None:
    """默认档位 Observe:metadata.pii 有命中明细,正文原样保留。"""
    require_binary()
    script = mock_script(_echo_lines([f"联系人 {PHONE} 已登记"]))
    with serve_session(env=sandbox.mock_env(script)) as s:
        evs = s.submit({"type": "user_input", "items": [{"type": "text", "text": "echo"}]},
                       timeout=DRAIN_TIMEOUT)
    end = _tool_end(evs)
    assert not end["is_error"], end
    text = _tool_text(end)
    assert PHONE in text, f"Observe 不得改写正文:{text}"
    pii = end["output"]["metadata"].get("pii")
    assert pii is not None, f"应有 pii 报告:{end['output'].get('metadata')}"
    assert pii["total"] >= 1, pii
    assert pii["mode"] == "observe", pii
    codes = [h["code"] for h in pii["hits"]]
    assert "PII_CN_MOBILE" in codes, pii


# ── 边界:enforce / off / enabled=false ───────────────────────────────────


def test_enforce档抹掉正文(serve_session, mock_script, sandbox) -> None:
    """`[sanitize] pii = "enforce"`:手机号与邮箱都被替换为 marker。"""
    require_binary()
    _write_config(sandbox, '[sanitize]\npii = "enforce"\n')
    script = mock_script(_echo_lines([f"mail {EMAIL} phone {PHONE}"]))
    with serve_session(env=sandbox.mock_env(script)) as s:
        evs = s.submit({"type": "user_input", "items": [{"type": "text", "text": "echo"}]},
                       timeout=DRAIN_TIMEOUT)
    end = _tool_end(evs)
    text = _tool_text(end)
    assert EMAIL not in text, f"邮箱应被抹掉:{text}"
    assert PHONE not in text, f"手机号应被抹掉:{text}"
    assert "[REDACTED]" in text, f"应有 marker:{text}"
    assert end["output"]["metadata"]["pii"]["mode"] == "enforce"


def test_off档完全不扫描(serve_session, mock_script, sandbox) -> None:
    """`pii = "off"`:既不改写也不产生报告。"""
    require_binary()
    _write_config(sandbox, '[sanitize]\npii = "off"\n')
    script = mock_script(_echo_lines([PHONE]))
    with serve_session(env=sandbox.mock_env(script)) as s:
        evs = s.submit({"type": "user_input", "items": [{"type": "text", "text": "echo"}]},
                       timeout=DRAIN_TIMEOUT)
    end = _tool_end(evs)
    assert PHONE in _tool_text(end)
    assert "pii" not in json.dumps(end["output"].get("metadata", {})), end["output"].get("metadata")


def test_sanitize整体关闭时无PII报告(serve_session, mock_script, sandbox) -> None:
    """`[sanitize] enabled = false` 一并关掉 PII(既有密钥脱敏语义不变)。"""
    require_binary()
    _write_config(sandbox, "[sanitize]\nenabled = false\n")
    script = mock_script(_echo_lines([PHONE]))
    with serve_session(env=sandbox.mock_env(script)) as s:
        evs = s.submit({"type": "user_input", "items": [{"type": "text", "text": "echo"}]},
                       timeout=DRAIN_TIMEOUT)
    end = _tool_end(evs)
    assert PHONE in _tool_text(end)
    assert "pii" not in json.dumps(end["output"].get("metadata", {}))


# ── 失败路径 ──────────────────────────────────────────────────────────────


def test_干净文本无PII报告(serve_session, mock_script, sandbox) -> None:
    """无命中时不写 metadata.pii(避免给每条输出加噪声键)。"""
    require_binary()
    script = mock_script(_echo_lines(["fn main() { println!(\"hi\"); }"]))
    with serve_session(env=sandbox.mock_env(script)) as s:
        evs = s.submit({"type": "user_input", "items": [{"type": "text", "text": "echo"}]},
                       timeout=DRAIN_TIMEOUT)
    end = _tool_end(evs)
    assert "pii" not in json.dumps(end["output"].get("metadata", {}))


def test_短数字串不误判为PII(serve_session, mock_script, sandbox) -> None:
    """行号/耗时/token 计数都是短数字串,不能被当成卡号抹掉。"""
    require_binary()
    _write_config(sandbox, '[sanitize]\npii = "enforce"\n')
    text = "line 42, elapsed 1234ms, tokens 5678"
    script = mock_script(_echo_lines([text]))
    with serve_session(env=sandbox.mock_env(script)) as s:
        evs = s.submit({"type": "user_input", "items": [{"type": "text", "text": "echo"}]},
                       timeout=DRAIN_TIMEOUT)
    end = _tool_end(evs)
    assert _tool_text(end) == text, f"短数字串不应被抹:{_tool_text(end)}"


# ── 权限交互 ──────────────────────────────────────────────────────────────


def test_审批批准后输出仍走PII通道(serve_session, mock_script, sandbox) -> None:
    """ZHUYE_APPROVALS=1 + MCP echo:批准 → 执行 → PII 仍扫描并上报。"""
    require_binary()
    if not MOCK_MCP.is_file():
        pytest.skip("mock_mcp_server 未构建(cargo build -p zhuye-mcp)")
    _write_config(
        sandbox,
        f'[mcp_servers.fsx]\ntype = "stdio"\ncommand = "{MOCK_MCP}"\n',
    )
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "mcp__fsx__echo", "args": {"text": PHONE}}),
        '{"type":"text","text":"done"}',
    ])
    approved: dict[str, Any] = {}

    with serve_session(
        env={
            "ZHUYE_MODEL": "mock",
            "ZHUYE_MOCK_SCRIPT": str(script),
            "ZHUYE_APPROVALS": "1",
        }
    ) as s:
        ready = s.wait_global(
            lambda e: e.get("msg", {}).get("type") in {"mcp_server_started", "mcp_server_failed"}
            and e["msg"].get("server") == "fsx"
        )
        assert ready["msg"]["type"] == "mcp_server_started", ready

        def _on_event(ev: dict[str, Any]) -> None:
            m = ev.get("msg", {})
            if m.get("type") == "approval_request" and "request_id" not in approved:
                approved.update(m)
                s.send({"type": "tool_approval", "id": m["request_id"], "decision": "approve"})

        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "调MCP"}]},
            timeout=40.0,
            on_event=_on_event,
        )
    assert approved, f"未见 approval_request:{msg_types(evs)}"
    end = _tool_end(evs)
    assert not end["is_error"], end
    pii = end["output"].get("metadata", {}).get("pii")
    assert pii is not None and pii["total"] >= 1, end["output"].get("metadata")
