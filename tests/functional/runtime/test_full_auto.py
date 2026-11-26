"""能力覆盖层 · v1.11 基建#11:显式完全托管模式(字段级断言)。

覆盖:
- `--full-auto`(及别名 `--yolo`)→ 会话以 Bubble 模式启动,首个 turn 的
  SessionConfigured 之后发全局 `permission_mode_changed`
  `{from:"auto", to:"bubble"}`(客户端可感知完全托管态)
- 行为差异:`ZHUYE_APPROVALS=1`(审批门启用)+ 内置 `write` 工具
  (Prompt 权限)→ **无** `approval_request` 阻塞,出现
  `permission_bubble` 通知,工具真执行成功 —— 这是与默认 Auto 模式
  (落 modal)的本质区别
- `--full-auto` 与 `--plan-mode` 互斥(clap 层拒绝)
- 默认(无旗标)不发模式通告 —— 向后兼容,既有会话事件流不变

依据:AGENTS.md 开发流程检查表 —— 能力断言到字段级。
v1.3 安全基线不因本旗标松动:高危工具(risk == High)仍强制审批,
不在本文件断言(由 gate 单测覆盖)。
"""

from __future__ import annotations

import subprocess
from typing import Any

import pytest

from wire import ZHUYE_BIN, msg_types, require_binary

pytestmark = pytest.mark.usefixtures("sandbox")


def _tool_end(events: list[dict[str, Any]]) -> dict[str, Any]:
    """取 tool_call_end 事件(找不到直接失败,附上已见类型)。"""
    for ev in events:
        m = ev.get("msg", {})
        if m.get("type") == "tool_call_end":
            return m
    raise AssertionError(f"未见 tool_call_end:{msg_types(events)}")


def test_启动即通告bubble模式(serve_session, mock_script) -> None:
    """`serve --full-auto` → 全局 permission_mode_changed
    {from:"auto", to:"bubble"},且 turn 正常收尾。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"ok"}'])
    with serve_session(
        args=["--full-auto"],
        env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)},
    ) as s:
        events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "hi"}]})
    changed = [
        e["msg"]
        for e in s.all_events()
        if e.get("msg", {}).get("type") == "permission_mode_changed"
    ]
    assert changed, f"未见 permission_mode_changed:{msg_types(s.all_events())}"
    # 与运行时模式切换(apply_permission_mode_change)同款双通路
    # (session 扇出 + turn 通道),wire 上可能出现两条同载荷事件,
    # 断言字段正确即可。
    assert all(c["from"] == "auto" and c["to"] == "bubble" for c in changed), changed
    assert "turn_complete" in msg_types(events)


def test_yolo别名等效(serve_session, mock_script) -> None:
    """`serve --yolo` 与 `--full-auto` 同效(同一通告事件)。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"ok"}'])
    with serve_session(
        args=["--yolo"],
        env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)},
    ) as s:
        s.submit({"type": "user_input", "items": [{"type": "text", "text": "hi"}]})
    changed = [
        e["msg"]
        for e in s.all_events()
        if e.get("msg", {}).get("type") == "permission_mode_changed"
        and e["msg"].get("to") == "bubble"
    ]
    assert changed, "别名 --yolo 未触发 bubble 模式通告"


def test_完全托管下Prompt工具自动批准(serve_session, mock_script, sandbox) -> None:
    """`ZHUYE_APPROVALS=1` + `--full-auto` + 内置 `write`(Prompt 权限)
    → 无 approval_request 阻塞、发 permission_bubble 通知、工具真执行。"""
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call","name":"write","args":{"path":"out.txt","content":"done"}}',
            '{"type":"text","text":"written"}',
        ]
    )
    with serve_session(
        args=["--full-auto"],
        env={
            "ZHUYE_MODEL": "mock",
            "ZHUYE_MOCK_SCRIPT": str(script),
            "ZHUYE_APPROVALS": "1",
        },
    ) as s:
        events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "写文件"}]})
    types = msg_types(events)
    assert "approval_request" not in types, f"完全托管不得弹阻塞审批:{types}"
    bubbles = [
        e["msg"] for e in events if e.get("msg", {}).get("type") == "permission_bubble"
    ]
    assert bubbles, f"未见 permission_bubble 通知:{types}"
    assert bubbles[0].get("tool_name") == "write", bubbles[0]
    end = _tool_end(events)
    assert end["is_error"] is False, f"Bubble 模式应自动批准执行:{end}"
    assert (sandbox.cwd / "out.txt").read_text() == "done", "工具必须真执行落盘"
    assert "turn_complete" in types, f"turn 未收尾:{types}"


def test_默认模式不发通告(serve_session, mock_script) -> None:
    """无旗标启动(默认 Auto)→ 不发 permission_mode_changed(向后兼容)。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"ok"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit({"type": "user_input", "items": [{"type": "text", "text": "hi"}]})
    changed = [
        e
        for e in s.all_events()
        if e.get("msg", {}).get("type") == "permission_mode_changed"
    ]
    assert not changed, f"默认启动不得发模式通告:{[e['msg'] for e in changed]}"


def test_与plan_mode互斥(sandbox) -> None:
    """`--full-auto --plan-mode` 同给 → clap 拒绝(退出码 2)。"""
    require_binary()
    proc = subprocess.run(
        [
            str(ZHUYE_BIN),
            "serve",
            "--full-auto",
            "--plan-mode",
        ],
        cwd=str(sandbox.cwd),
        env=sandbox.env({"ZHUYE_MODEL": "mock"}),
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert proc.returncode == 2, f"互斥旗标应被 clap 拒绝:{proc.stdout}{proc.stderr}"
