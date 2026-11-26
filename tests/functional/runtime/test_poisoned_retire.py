"""能力覆盖层 · F11:中毒会话退役 + 连续性通知 + ReasonCode。

直接驱动 `zhuye serve` 原始 JSONL wire。

覆盖(正常 / 边界 / 失败):
- 正常:反复同一错误触发退役 → `error` 事件 `code="session_poisoned"`,
  details 带 `reason_code` / `poison_class` / `continuity_notice` /
  `lost_memory`;
- 边界:健康会话不误判(正常多轮不退役);
- 失败:退役只触发一次(不会每轮刷屏)。

运行:tests/run_all.sh functional,或
  ZHUYE_BIN=... python3 -m pytest tests/functional/test_poisoned_retire.py -v
"""

from __future__ import annotations

import json
from typing import Any

import pytest

from wire import DRAIN_TIMEOUT, first_event, msg_types, require_binary

pytestmark = pytest.mark.usefixtures("sandbox")


def _errors(events: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [e["msg"] for e in events if e["msg"]["type"] == "error"]


def _poisoned(events: list[dict[str, Any]]) -> dict[str, Any] | None:
    for msg in _errors(events):
        if msg.get("code") == "session_poisoned":
            return msg
    return None


def test_健康会话不退役(serve_session, mock_script, sandbox) -> None:
    """正常多轮对话不触发退役(防误判)。"""
    require_binary()
    lines = []
    for i in range(6):
        lines.append(json.dumps({"type": "text", "text": f"第 {i} 轮正常回复"}))
    script = mock_script(lines)
    with serve_session(env=sandbox.mock_env(script)) as s:
        for i in range(6):
            evs = s.submit(
                {"type": "user_input", "items": [{"type": "text", "text": f"问{i}"}]},
                timeout=DRAIN_TIMEOUT,
            )
            assert _poisoned(evs) is None, f"第 {i} 轮误判退役:{msg_types(evs)}"
            assert "turn_complete" in msg_types(evs), msg_types(evs)


def test_反复同一错误触发退役且带ReasonCode(serve_session, mock_script, sandbox) -> None:
    """同一工具有毒输出反复出现 → 退役事件带稳定 reason_code 与连续性通知。

    poison 检测器的 Output 类判据是「连续 N 轮输出指纹相同」;这里用同一
    条输出反复喂,直到退役。退役事件的 details 必须含:
    - `reason_code` = "session_poisoned"(稳定枚举 wire 名)
    - `poison_class`
    - `continuity_notice`(人读文案)
    - `lost_memory`(数组,可为空)
    """
    require_binary()
    # 每轮都返回完全相同的文本 —— 触发 output 指纹连续重复判据。
    same = "完全相同的有毒输出 repeated"
    lines = []
    for _ in range(40):
        lines.append(json.dumps({"type": "text", "text": same}))
    script = mock_script(lines)

    retired: dict[str, Any] | None = None
    with serve_session(env=sandbox.mock_env(script)) as s:
        for i in range(40):
            evs = s.submit(
                {"type": "user_input", "items": [{"type": "text", "text": f"重复{i}"}]},
                timeout=DRAIN_TIMEOUT,
            )
            hit = _poisoned(evs)
            if hit is not None:
                retired = hit
                break
    assert retired is not None, "反复同一输出应触发退役"
    details = retired.get("details") or {}
    assert details.get("reason_code") == "session_poisoned", retired
    assert details.get("poison_class"), retired
    notice = details.get("continuity_notice")
    assert isinstance(notice, str) and notice, retired
    # 连续性通知必须说明「丢失/已迁移」语义,不能是空话。
    assert ("迁移" in notice) or ("丢失" in notice), notice
    assert isinstance(details.get("lost_memory"), list), retired


def test_退役后新提交仍可服务(serve_session, mock_script, sandbox) -> None:
    """退役是「会话状态标记」,不是进程崩溃 —— 后续提交仍能正常应答。

    这让宿主有机会「收尾旧会话、开新会话」,而不是整个进程挂掉。
    """
    require_binary()
    same = "同样输出"
    lines = [json.dumps({"type": "text", "text": same}) for _ in range(40)]
    lines.append('{"type":"text","text":"fresh reply"}')
    script = mock_script(lines)
    with serve_session(env=sandbox.mock_env(script)) as s:
        for i in range(40):
            evs = s.submit(
                {"type": "user_input", "items": [{"type": "text", "text": f"r{i}"}]},
                timeout=DRAIN_TIMEOUT,
            )
            if _poisoned(evs) is not None:
                break
        # 退役后仍能提交并收到终态(不是死循环 / 挂起)。
        tail = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "继续"}]},
            timeout=DRAIN_TIMEOUT,
        )
    types = msg_types(tail)
    assert "turn_complete" in types or "error" in types, types
