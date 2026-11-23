"""能力覆盖层 · F04:命令准入(只读语义 + OPAQUE)。

直接驱动 `zhuye serve` 原始 JSONL wire。

覆盖(正常 / 边界 / 失败 / 权限沙箱交互):
- 正常:纯读命令(`git status` / `ls` / `grep`)在审批开启时不弹窗直接执行;
- 边界:写命令(`git commit` / `rm`)仍弹审批;混合链(`a && b`)任一段写
  即整条审批;含引号的命令(argv 不可静态确定)仍审批;
- 失败:未知程序走审批(保守);
- 权限交互:审批开启时纯读不产生 approval_request,写会产生。

运行:tests/run_all.sh functional,或
  ZHUYE_BIN=... python3 -m pytest tests/functional/test_bash_readonly.py -v
"""

from __future__ import annotations

import json
from typing import Any

import pytest

from wire import DRAIN_TIMEOUT, msg_types, require_binary

pytestmark = pytest.mark.usefixtures("sandbox")


def _tool_end(events: list[dict[str, Any]]) -> dict[str, Any]:
    for e in events:
        if e["msg"]["type"] == "tool_call_end":
            return e["msg"]
    raise AssertionError(f"未见 tool_call_end:{msg_types(events)}")


def _approvals(events: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [e["msg"] for e in events if e["msg"]["type"] == "approval_request"]


def _run_bash(serve_session, mock_script, sandbox, cmd: str, approvals: bool = True):
    """跑一次 bash 工具调用,返回 (事件流, 是否出现审批请求)。"""
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "bash", "args": {"cmd": cmd}}),
        '{"type":"text","text":"done"}',
    ])
    env = {"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}
    if approvals:
        env["ZHUYE_APPROVALS"] = "1"
    seen: list[dict[str, Any]] = []

    with serve_session(env=env) as s:
        def _on_event(ev: dict[str, Any]) -> None:
            m = ev.get("msg", {})
            if m.get("type") == "approval_request":
                seen.append(m)
                s.send({"type": "tool_approval", "id": m["request_id"], "decision": "approve"})

        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "跑一下"}]},
            timeout=40.0,
            on_event=_on_event,
        )
    return evs, bool(seen)


# ── 正常:纯读自动放行 ─────────────────────────────────────────────────────


@pytest.mark.parametrize(
    "cmd",
    [
        "ls -la",
        "echo hi",
        "grep -r zhuye .",
        "cat /dev/null",
        "wc -l /dev/null",
    ],
)
def test_纯读命令不弹审批(serve_session, mock_script, sandbox, cmd: str) -> None:
    """纯读命令跳过审批,直接执行并收到输出。"""
    require_binary()
    evs, had_approval = _run_bash(serve_session, mock_script, sandbox, cmd)
    assert not had_approval, f"{cmd} 不应弹审批:{msg_types(evs)}"
    end = _tool_end(evs)
    assert not end["is_error"], end


def test_git只读子命令不弹审批即使非仓库(serve_session, mock_script, sandbox) -> None:
    """`git status` 在非仓库目录会报错,但**权限层不该弹审批** ——
    只读语义判定与命令执行成功与否是两件事。"""
    require_binary()
    evs, had_approval = _run_bash(serve_session, mock_script, sandbox, "git status")
    assert not had_approval, f"git status 不应弹审批:{msg_types(evs)}"


# ── 边界:写命令仍审批 ─────────────────────────────────────────────────────


@pytest.mark.parametrize(
    "cmd",
    [
        "git commit -m probe",
        "git config user.name probe",
        "rm -rf /tmp/zhuye-f04-probe",
        "sh -c 'echo x'",
        "git status && git commit -m x",
    ],
)
def test_写命令与不透明命令仍弹审批(serve_session, mock_script, sandbox, cmd: str) -> None:
    """写 / OPAQUE / 混合链必须走审批。"""
    require_binary()
    evs, had_approval = _run_bash(serve_session, mock_script, sandbox, cmd)
    assert had_approval, f"{cmd} 应需审批:{msg_types(evs)}"


@pytest.mark.parametrize("cmd", ["some-unknown-binary --flag", "git log --grep='a b'"])
def test_未知程序与含引号命令保守审批(serve_session, mock_script, sandbox, cmd: str) -> None:
    """判不准的命令一律走审批(不因"看着像读"放行)。"""
    require_binary()
    evs, had_approval = _run_bash(serve_session, mock_script, sandbox, cmd)
    assert had_approval, f"{cmd} 应保守审批:{msg_types(evs)}"


@pytest.mark.parametrize("cmd", ["echo hi>marker", "sed -i s/a/b/ marker", "sort -omarker input", "find . -fprint marker", "curl -o marker http://127.0.0.1/"])
def test_可写命令拒绝审批后无副作用(serve_session, mock_script, sandbox, cmd: str) -> None:
    """附着重定向、写入选项和可执行程序不得绕过审批。"""
    script = mock_script([json.dumps({"type": "tool_call", "name": "bash", "args": {"cmd": cmd}}), '{"type":"text","text":"done"}'])
    seen = []
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script), "ZHUYE_APPROVALS": "1"}) as session:
        def deny(event: dict[str, Any]) -> None:
            msg = event.get("msg", {})
            if msg.get("type") == "approval_request":
                seen.append(msg)
                session.send({"type": "tool_approval", "id": msg["request_id"], "decision": {"deny": {"reason": "blocked"}}})
        events = session.submit({"type": "user_input", "items": [{"type": "text", "text": "检查命令"}]}, timeout=DRAIN_TIMEOUT, on_event=deny)
    assert len(seen) == 1, (cmd, msg_types(events))
    end = _tool_end(events)
    assert end["is_error"] is True and "blocked" in str(end["output"])
    assert not (sandbox.cwd / "marker").exists()


# ── 失败路径 ──────────────────────────────────────────────────────────────


def test_审批关闭时纯读仍能执行(serve_session, mock_script, sandbox) -> None:
    """approvals 关闭时,纯读命令照常执行(不因权限层缺省而失败)。"""
    require_binary()
    evs, _ = _run_bash(serve_session, mock_script, sandbox, "echo readonly-probe", approvals=False)
    end = _tool_end(evs)
    assert not end["is_error"], end
    text = "".join(
        c.get("text", "") for c in end.get("output", {}).get("content", []) if isinstance(c, dict)
    )
    assert "readonly-probe" in text, text
