"""guard 组场景:边界与策略(OS 沙箱 / hook 门控 / 回合级审批策略)。

引擎事实(与 zhuye-sandbox / zhuye-hooks / zhuye-core 对齐):

- OS 沙箱:配置 `[sandbox].os_level = true` 时 bash 在 macOS Seatbelt
  下执行 —— 只有 workspace(与 `writable_dirs`)可写,其余路径写入被
  内核拒绝(工具回执为错误);工作区内写入照常成功;
- `read_before_edit` hook(headless 默认启用):`write` / `edit` /
  `notebook_edit` 的写意图在本会话内**未读过目标文件**时被 deny,回执
  带原因;`read` 过后再写即放行。这是「防误覆盖」层,不是安全边界;
- 回合级审批策略:`Op::UserInput.thread_settings.approval_policy`:
  `full_access` 自动批准(含高危)、`deny` 直接拒绝需审批的工具
  (不弹审批),`prompt` 强制走审批门。

这三个场景都是**确定性**的(不依赖模型怎么想),因此 mock 自检与真实
模型都能跑。
"""

from __future__ import annotations

import json
import os

import cli_e2e_lib as lib


def _tool(name: str, args: dict) -> dict:
    return {"type": "tool_call", "name": name, "args": args}


def _text(value: str) -> dict:
    return {"type": "text", "text": value}


# ── sandbox_escape_blocked:OS 沙箱拦住越界写入 ─────────────────────────────


def _run_sandbox_escape(cx: lib.Ctx) -> None:
    # 越界目标放在临时目录(宿主的 /tmp 可达,但沙箱只放行 workspace)。
    outside = os.path.join(cx.work, "outside-write.txt")
    if os.path.exists(outside):
        os.remove(outside)
    env = cx.env()
    s = cx.session(env)

    turn = s.turn(
        f"请用 bash 工具依次执行两条命令:"
        f"1) `echo inside > inside.txt`;"
        f"2) `echo outside > {outside}`。"
        f"两条都要执行,分别告诉我成功还是失败。",
        timeout=cx.timeout(),
    )
    results = turn.tool_results()
    errors = [r for r in results if r["is_error"]]
    cx.checks.add("工作区内写入成功", cx.exists(env.ws, "inside.txt"),
                  f"inside.txt 存在={cx.exists(env.ws, 'inside.txt')}")
    cx.checks.add("越界写入被沙箱拒绝(工具回执为错误)", bool(errors),
                  f"失败工具数={len(errors)}/{len(results)},"
                  f"首个={errors[0]['text'][:160] if errors else None}")
    cx.checks.add("越界目标文件未产生", not os.path.exists(outside),
                  f"路径 {outside} 存在={os.path.exists(outside)}")
    cx.checks.add("回合正常收尾", turn.completed,
                  f"终态={turn.status or turn.aborted_reason}")


def _mock_sandbox_escape(params: dict) -> list[dict]:
    outside = params.get("_outside") or "/tmp/zhuye-e2e-outside.txt"
    return [
        _tool("bash", {"command": "echo inside > inside.txt"}),
        _tool("bash", {"command": f"echo outside > {outside}"}),
        _text("第一条成功,第二条失败。"),
    ]


SANDBOX_ESCAPE = lib.Scenario(
    "sandbox_escape_blocked",
    "OS 沙箱:工作区可写、越界写入被拒绝",
    _run_sandbox_escape,
    effort="low",
    defaults={},
    mock_script=_mock_sandbox_escape,
)


# ── hooks_read_before_edit:未读先写被 hook 拦 ──────────────────────────────


def _setup_read_before_edit(ws: str) -> None:
    with open(f"{ws}/a.txt", "w", encoding="utf-8") as f:
        f.write("v1\n")


def _run_read_before_edit(cx: lib.Ctx) -> None:
    env = cx.env(setup=_setup_read_before_edit)
    s = cx.session(env)

    # 第一回合:不读直接覆盖 → hook 拒绝,文件保持原样。
    denied_turn = s.turn(
        "请用 write 工具把 a.txt 的内容覆盖成 v2(不要先读,直接覆盖)。",
        timeout=cx.timeout(),
    )
    names = {lib._msg(e).get("call_id"): lib._msg(e).get("tool_name")
             for e in denied_turn.events if lib.etype(e) == "tool_call_begin"}
    denied = [r for r in denied_turn.tool_results() if r["is_error"]]
    cx.checks.add(
        "未读先写被 hook 拦下(工具回执为错误)",
        bool(denied),
        f"失败调用={[(names.get(r['call_id']), r['text'][:120]) for r in denied]}",
    )
    blob = " ".join(r["text"].lower() for r in denied)
    cx.checks.add(
        "拒绝原因指向「先读再写」(read before edit 提示)",
        ("read" in blob) or ("读" in blob),
        f"拒绝文案={blob[:200]}",
    )
    content = (cx.read_file(env.ws, "a.txt") or "").strip()
    cx.checks.add("被拦下时文件内容未被改写", content == "v1", f"a.txt={content!r}")

    # 第二回合:先读再写 → 放行,内容更新。
    ok_turn = s.turn("请先 read 一次 a.txt,然后把它覆盖成 v2。", timeout=cx.timeout())
    content = (cx.read_file(env.ws, "a.txt") or "").strip()
    cx.checks.add("读过之后写入放行(内容已更新为 v2)", content == "v2",
                  f"a.txt={content!r}")
    cx.checks.add("两个回合都正常收尾",
                  denied_turn.completed and ok_turn.completed,
                  f"终态={denied_turn.status}/{ok_turn.status}")


def _mock_read_before_edit(params: dict) -> list[dict]:
    return [
        # 回合 1:不读直接覆盖 → hook 拒绝。
        _tool("write", {"path": "a.txt", "content": "v2\n"}),
        _text("写入被拒。"),
        # 回合 2:先 read 再写 → 放行。
        _tool("read", {"path": "a.txt"}),
        _tool("write", {"path": "a.txt", "content": "v2\n"}),
        _text("已改为 v2。"),
    ]


READ_BEFORE_EDIT = lib.Scenario(
    "hooks_read_before_edit",
    "read_before_edit hook:未读先写被拒,读后放行",
    _run_read_before_edit,
    effort="low",
    defaults={},
    mock_script=_mock_read_before_edit,
)


# ── permission_policy:回合级审批策略 ───────────────────────────────────────


def _policy_turn(s: lib.Session, policy: str, command: str, timeout: float) -> lib.Turn:
    """带 thread_settings 覆盖投递一条 UserInput 并等回合终态。"""
    sub_id = s._new_id(f"turn-{policy}")
    s.send(sub_id, {
        "type": "user_input",
        "items": [{"type": "text",
                   "text": f"请用 bash 工具执行命令 `{command}`(必须走 bash 工具)。"}],
        "thread_settings": {"approval_policy": policy},
    })
    started = s.wait(lambda e: lib.etype(e) == "turn_started", timeout,
                     what="turn_started")
    turn_id = lib._msg(started).get("turn_id")
    terminal = s.wait(
        lambda e: lib.etype(e) in lib.TURN_TERMINAL and lib._msg(e).get("turn_id") == turn_id,
        timeout, what="回合终态",
    )
    s.wait(lambda e: lib.etype(e) == "submission_closed" and e.get("id") == sub_id,
           timeout, what="submission_closed")
    start = next(i for i, e in enumerate(s.events) if e is started)
    end = s.events.index(terminal) + 1
    return lib.Turn(s.events[start:end], sub_id, terminal)


def _run_permission_policy(cx: lib.Ctx) -> None:
    # 关掉 ApprovalGate 的全局审批,让「需审批」由回合级策略决定
    # (deny 直接拒、full_access 自动批)。
    env = cx.env(env_extra={"ZHUYE_APPROVALS": "1"})
    s = cx.session(env)

    deny_turn = _policy_turn(s, "deny", "echo denied > policy-denied.txt", cx.timeout())
    denied_errs = [r for r in deny_turn.tool_results() if r["is_error"]]
    cx.checks.add(
        "approval_policy=deny:需审批的工具被直接拒绝(回执为错误)",
        bool(denied_errs),
        f"失败工具数={len(denied_errs)},文案="
        f"{denied_errs[0]['text'][:160] if denied_errs else None}",
    )
    cx.checks.add("approval_policy=deny:命令未执行(无落盘)",
                  not cx.exists(env.ws, "policy-denied.txt"),
                  "路径 policy-denied.txt")

    full_turn = _policy_turn(s, "full_access", "echo ok > policy-ok.txt", cx.timeout())
    cx.checks.add(
        "approval_policy=full_access:命令被执行",
        cx.exists(env.ws, "policy-ok.txt"),
        f"policy-ok.txt 存在={cx.exists(env.ws, 'policy-ok.txt')},"
        f"失败工具数={len([r for r in full_turn.tool_results() if r['is_error']])}",
    )
    for turn, label in ((deny_turn, "deny"), (full_turn, "full_access")):
        cx.checks.add(f"approval_policy={label}:回合有终态",
                      turn.completed or bool(turn.aborted_reason),
                      f"终态={turn.status or turn.aborted_reason}")


def _mock_permission_policy(params: dict) -> list[dict]:
    return [
        _tool("bash", {"command": "echo denied > policy-denied.txt"}),
        _text("第一条被拒。"),
        _tool("bash", {"command": "echo ok > policy-ok.txt"}),
        _text("第二条成功。"),
    ]


PERMISSION_POLICY = lib.Scenario(
    "permission_policy",
    "回合级审批策略:deny 直接拒,full_access 自动批",
    _run_permission_policy,
    effort="low",
    defaults={},
    mock_script=_mock_permission_policy,
)
