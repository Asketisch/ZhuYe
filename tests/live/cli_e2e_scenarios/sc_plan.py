"""plan 组场景:plan 模式的三条路径。

- plan_dev_commit:plan → 确认(自动批准)→ 正常开发 → git init + commit
  (原 cli_e2e 场景;沙箱 .git 归属快照的端到端回归点);
- plan_revise:plan → 打回(revise)→ 按反馈修订 → 再次提交 → 确认 → 开发;
- plan_readonly:plan 模式只读门控 —— 写工具与高风险 bash 被拦,不落盘。

共同事实:`ExitPlanMode` 工具调用会阻塞等待审批,批准 / 打回后工具
返回、**同一回合**继续(模型接着开发或回应反馈)。所以这三个场景都
在一次 `turn()` 内完成,终态按 turn_id 配对。
"""

from __future__ import annotations

import subprocess

import cli_e2e_lib as lib

README = """# 烽火 Fanfire

烽火是一个(虚构的)高性能日志采集器,单二进制、零依赖、吞吐 100 万行/秒。

## 特性
- 单二进制分发,开箱即用
- 内存占用 < 30MB
- 内置 Prometheus 指标端点

## 快速开始
fanfire --config fanfire.toml
"""

COMMIT_SUBJECT = "feat: 烽火介绍单页"


def _setup(ws: str) -> None:
    with open(f"{ws}/README.md", "w", encoding="utf-8") as f:
        f.write(README)


def _mode_flow(turn: lib.Turn) -> list[str]:
    return [f"{m.get('from')}→{m.get('to')}" for m in turn._msgs("permission_mode_changed")]


def _has_commit(ws: str, subject: str) -> bool:
    try:
        out = subprocess.run(
            ["git", "-C", ws, "log", "--format=%s"],
            capture_output=True,
            text=True,
            timeout=20,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return out.returncode == 0 and subject in out.stdout.splitlines()


# ── plan_dev_commit ─────────────────────────────────────────────────────────


def _task_fields(cx: lib.Ctx) -> dict:
    """任务文本与验收期望:顶层 [task] 为默认,[scenarios.plan_dev_commit] 可覆盖。"""
    task = cx.cfg.get("task") or {}
    return {
        "prompt": cx.p("prompt") or task.get("prompt") or "",
        "continue_prompt": cx.p("continue_prompt") or task.get("continue_prompt") or "",
        "expect_artifact": cx.p("expect_artifact") or task.get("expect_artifact") or "index.html",
        "expect_commit": cx.p("expect_commit_subject")
        or task.get("expect_commit_subject")
        or COMMIT_SUBJECT,
    }


def _run_plan_dev(cx: lib.Ctx) -> None:
    fields = _task_fields(cx)
    artifact = fields["expect_artifact"]
    subject = fields["expect_commit"]
    env = cx.env(serve_args=["--plan-mode"], setup=_setup)

    plans: list[dict] = []
    approve = lib.on_plan_ready("auto_mode", log=plans)

    s = cx.session(env)
    turns = [s.turn(fields["prompt"], timeout=cx.timeout(), respond=[approve])]

    cx.checks.add("会话以 plan 模式启动", bool(s.model), f"model={s.model}")
    markdown = (plans[0].get("markdown") or "") if plans else ""
    cx.checks.add(
        "plan 正式产出(markdown 非空)",
        bool(plans) and bool(markdown.strip()),
        f"plan_ready 事件数={len(plans)},markdown 字符数={len(markdown)}",
    )
    first = turns[0]
    approved = bool(first._msgs("plan_approved"))
    modes = _mode_flow(first)
    cx.checks.add(
        "plan 审批生效(切到 accept_edits 开发模式)",
        approved and any("accept_edits" in m for m in modes),
        f"plan_approved={approved},权限切换={modes}",
    )

    # 开发可能分多个回合(模型自行收尾并列出剩余步骤):按任务配置的
    # continue_prompt 续推,直到产物与提交都到位或达到续推上限。
    max_continue = int(cx.p("max_continue_turns", 2))
    while (
        len(turns) <= max_continue
        and turns[-1].completed
        and not (cx.exists(env.ws, artifact) and _has_commit(env.ws, subject))
    ):
        turns.append(s.turn(fields["continue_prompt"], timeout=cx.timeout()))

    codes = sorted({c for t in turns for c in t.error_codes if c in lib.FATAL_CODES})
    cx.checks.add(
        "全程无结构性错误(含 compact_rapid_refill)",
        not codes,
        f"错误码={codes}" if codes else "无",
    )
    cx.checks.add(
        f"产物存在({artifact})",
        cx.exists(env.ws, artifact),
        f"路径 {artifact}",
    )
    committed = _has_commit(env.ws, subject)
    cx.checks.add(
        "git 仓库含 agent 提交(沙箱归属快照回归点)",
        committed,
        f"git log 主题行命中「{subject}」" if committed else f"未找到提交「{subject}」",
    )
    cx.metrics.update(
        model_calls=sum(len(t.inputs) for t in turns),
        turns=len(turns),
        compactions=sum(len(t.compactions) for t in turns),
    )


def _mock_plan_dev(params: dict) -> list[dict]:
    subject = params.get("expect_commit_subject") or COMMIT_SUBJECT

    def bash(command: str) -> dict:
        return {"type": "tool_call", "name": "bash", "args": {"command": command}}

    return [
        {
            "type": "tool_call",
            "name": "ExitPlanMode",
            "args": {"markdown": "# 计划\n\n1. 创建 index.html\n2. git init + commit"},
        },
        {
            "type": "tool_call",
            "name": "write",
            "args": {"path": "index.html", "content": "<!doctype html><title>烽火</title>"},
        },
        bash("git init -b main"),
        bash("git add README.md index.html"),
        bash(
            "git -c user.name=e2e -c user.email=e2e@example.com "
            f"commit -m '{subject}'"
        ),
        {"type": "text", "text": "站点已创建并提交。"},
    ]


PLAN_DEV_COMMIT = lib.Scenario(
    "plan_dev_commit",
    "plan → 确认 → 开发 → git init + commit",
    _run_plan_dev,
    effort="xhigh",
    defaults={"max_continue_turns": 2},
    mock_script=_mock_plan_dev,
)


# ── plan_revise ─────────────────────────────────────────────────────────────

REVISE_PROMPT = "为产品创建 a.txt(内容 hello)。先给出计划,不要直接动手。"
REVISE_FEEDBACK = "不要创建 a.txt。改为创建 b.txt(内容 revised)。其余不变,重新提交计划。"
PLAN_V1 = "# 计划 v1\n\n- 创建 a.txt(初版方案)"
PLAN_V2 = "# 计划 v2\n\n- 创建 b.txt(按反馈,不创建 a.txt)"


def _run_plan_revise(cx: lib.Ctx) -> None:
    env = cx.env(serve_args=["--plan-mode"], setup=_setup)

    plans: list[dict] = []
    rejected: list[dict] = []

    def note_reject(s, evt):
        if lib.etype(evt) == "plan_rejected":
            rejected.append(lib._msg(evt))
        return False

    s = cx.session(env)
    # 第一轮:plan_ready 一律打回。
    revise = lib.on_plan_ready("revise", log=plans)
    first = s.turn(cx.p("prompt") or REVISE_PROMPT, timeout=cx.timeout(), respond=[revise, note_reject])

    same_id = bool(rejected) and bool(plans) and rejected[0].get("plan_id") == plans[0].get("plan_id")
    cx.checks.add(
        "打回 plan:plan_rejected 事件与 plan_id 配对",
        same_id,
        f"plan_ready={len(plans)},plan_rejected={len(rejected)}",
    )
    modes = _mode_flow(first)
    cx.checks.add(
        "打回后仍停留在 plan 模式(未解锁写工具)",
        first.completed and "plan→accept_edits" not in modes,
        f"终态={'完成' if first.completed else '中断'},权限切换={modes}",
    )
    leaked = [n for n in ("a.txt", "b.txt") if cx.exists(env.ws, n)]
    cx.checks.add("打回后未产生任何文件", not leaked, f"已落盘={leaked}" if leaked else "a.txt / b.txt 均不存在")

    # 第二轮:按反馈修订 → 新 plan → 批准 → 开发(同一回合内继续)。
    plans.clear()
    approve = lib.on_plan_ready("auto_mode", log=plans)
    second = s.turn(REVISE_FEEDBACK, timeout=cx.timeout(), respond=[approve])

    new_id = bool(plans) and bool(rejected) and plans[0].get("plan_id") != rejected[0].get("plan_id")
    cx.checks.add(
        "修订后产出新 plan(plan_id 更新)",
        new_id,
        f"新 plan_id={plans[0].get('plan_id') if plans else None}",
    )
    modes = _mode_flow(second)
    cx.checks.add(
        "修订版 plan 获批并进入开发模式",
        bool(second._msgs("plan_approved")) and any("accept_edits" in m for m in modes),
        f"plan_approved={len(second._msgs('plan_approved'))},权限切换={modes}",
    )
    cx.checks.add("按反馈产出 b.txt", cx.exists(env.ws, "b.txt"), "路径 b.txt")
    cx.checks.add("被否决的 a.txt 未创建", not cx.exists(env.ws, "a.txt"), "a.txt 不存在")
    cx.checks.add(
        "修订回合正常收尾",
        second.completed,
        f"终态={second.status or second.aborted_reason}",
    )


def _mock_plan_revise(params: dict) -> list[dict]:
    return [
        {"type": "tool_call", "name": "ExitPlanMode", "args": {"markdown": PLAN_V1}},
        {"type": "text", "text": "计划被打回,请告诉我修改意见。"},
        {"type": "tool_call", "name": "ExitPlanMode", "args": {"markdown": PLAN_V2}},
        {"type": "tool_call", "name": "write", "args": {"path": "b.txt", "content": "revised"}},
        {"type": "text", "text": "已按反馈创建 b.txt。"},
    ]


PLAN_REVISE = lib.Scenario(
    "plan_revise",
    "plan → 打回 → 按反馈修订 → 确认 → 开发",
    _run_plan_revise,
    effort="medium",
    defaults={"prompt": REVISE_PROMPT},
    mock_script=_mock_plan_revise,
)


# ── plan_readonly ───────────────────────────────────────────────────────────

READONLY_PROMPT = (
    "请直接创建文件 leak.txt(内容 hello),不需要规划,现在就创建。"
    "如果无法创建,就改为提交一份创建它的计划。"
)


def _run_plan_readonly(cx: lib.Ctx) -> None:
    """plan 模式只读门控。

    plan 的提交**必须打回**(revise):批准会合法地切出 plan 模式,之后的
    写入就不再受只读门控约束 —— 那样测的就不是门控了。打回后会话仍停在
    plan 模式,任何写路径都必须失败、工作区不落盘。
    """
    env = cx.env(serve_args=["--plan-mode"], setup=_setup)

    plans: list[dict] = []
    revise = lib.on_plan_ready("revise", log=plans)

    s = cx.session(env)
    turn = s.turn(cx.p("prompt") or READONLY_PROMPT, timeout=cx.timeout(), respond=[revise])

    modes = [f"{m.get('from')}→{m.get('to')}" for m in turn._msgs("permission_mode_changed")]
    left_plan = [m for m in modes if m.startswith("plan→") and m != "plan→plan"]
    cx.checks.add(
        "全程未离开 plan 模式(打回而非批准)",
        not left_plan,
        f"离开 plan 的切换={left_plan},全部切换={modes}",
    )
    leaked = [n for n in ("leak.txt",) if cx.exists(env.ws, n)]
    cx.checks.add(
        "plan 模式下工作区未被写入",
        not leaked,
        f"泄漏文件={leaked}" if leaked else "无文件落盘",
    )
    failed = [r for r in turn.tool_results() if r["is_error"]]
    # 真实模型可能发现工具集里没有 write 就直接改走规划,不硬性要求
    # 出现失败的工具调用;mock 自检则必须观察到两条写路径都被拦。
    ok = len(failed) >= 2 if cx.mock else True
    cx.checks.add(
        "写操作被只读门控拦截(工具结果为错误)",
        ok,
        f"失败的工具调用={len(failed)}" + ("" if cx.mock else "(真实模型下不作硬性要求)"),
    )
    cx.checks.add(
        "最终仍产出 plan(plan_ready)",
        bool(plans),
        f"plan_ready 事件数={len(plans)}",
    )
    cx.checks.add(
        "回合正常收尾",
        turn.completed,
        f"终态={turn.status or turn.aborted_reason}",
    )


def _mock_plan_readonly(params: dict) -> list[dict]:
    return [
        {"type": "tool_call", "name": "write", "args": {"path": "leak.txt", "content": "hello"}},
        {"type": "tool_call", "name": "bash", "args": {"command": "echo hello > leak.txt"}},
        {"type": "tool_call", "name": "ExitPlanMode", "args": {"markdown": "# 计划\n\n- 创建 leak.txt"}},
        {"type": "text", "text": "已提交创建 leak.txt 的计划。"},
    ]


PLAN_READONLY = lib.Scenario(
    "plan_readonly",
    "plan 模式只读门控:写工具与高风险 bash 被拦,不落盘",
    _run_plan_readonly,
    effort="medium",
    defaults={"prompt": READONLY_PROMPT},
    mock_script=_mock_plan_readonly,
)
