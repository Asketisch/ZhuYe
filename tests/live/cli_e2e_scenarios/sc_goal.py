"""goal 组场景:目标模式(goal mode)的自校验闭环。

引擎事实(与 zhuye-goal / zhuye-core 对齐,改动需同步改本文件):

- `Op::EnterGoalMode {goal, verify_command, token_budget}` 建立
  `GoalController`;无 LLM client 时发 `GOAL_NO_CLIENT` 错误事件
  (不再静默丢弃目标模式);
- 每回合结束后 controller 跑一次 **LLM 自校验**(verifier 单独发一次
  模型请求 —— 会消耗 mock 脚本的一行!),回复须是结构化 JSON:
  `{"verdict":"met","evidence":[...]}` / `{"verdict":"unmet","remaining":[...]}`
  / `{"verdict":"blocked","reason":"..."}`;解析失败 → 校验失败(warn,
  不续作);
- 判定顺序:先算 verdict,再累加 `tokens_used` → 预算用尽
  (`BudgetLimited`) > verdict=met 且 verify_command 通过(`Complete`)
  > verdict=blocked 且连续 3 次(`Blocked`,BLOCKED_THRESHOLD=3)
  > 其余 `Active`;
- 未达成的续作文本由引擎推进 steering 队列,**不自行开新回合** ——
  等下一条 UserInput 时合并注入;
- 终态(Complete / Blocked / BudgetLimited)会清空 state,只把终态留在
  `last_outcome`;遥测 `goal.turn.verified` 落在
  `$ZHUYE_HOME/traces/log/zhuye-<date>.jsonl`(context 里带 verdict/status)。

mock 脚本行序:模型回复在前、verifier 的 JSON 在后(verifier 在回合
结束后才发请求)。
"""

from __future__ import annotations

import json

import cli_e2e_lib as lib


def _text(value: str) -> dict:
    return {"type": "text", "text": value}


def _tool(name: str, args: dict) -> dict:
    return {"type": "tool_call", "name": name, "args": args}


VERDICT_MET = '{"verdict":"met","evidence":["目标产物已生成并通过校验命令"]}'
VERDICT_UNMET = '{"verdict":"unmet","remaining":["产物尚未创建"]}'


def _goal_records(env: lib.Env) -> list[dict]:
    return env.span_events("goal.turn.verified")


def _ctx_of(record: dict) -> dict:
    ctx = record.get("context")
    return ctx if isinstance(ctx, dict) else {}


# ── goal_met:目标达成 ──────────────────────────────────────────────────────


def _run_goal_met(cx: lib.Ctx) -> None:
    env = cx.env()
    s = cx.session(env)

    s.op({"type": "enter_goal_mode", "goal": cx.p("goal") or "创建 out.txt",
          "verify_command": cx.p("verify_command") or "test -f out.txt"})

    turn = s.turn("请完成目标:创建文件 out.txt(内容 done)。", timeout=cx.timeout())
    cx.checks.add("回合正常收尾", turn.completed,
                  f"终态={turn.status or turn.aborted_reason}")
    cx.checks.add("产物已创建(out.txt)", cx.exists(env.ws, "out.txt"), "路径 out.txt")

    records = _goal_records(env)
    cx.checks.add("遥测记录 goal.turn.verified", bool(records),
                  f"记录数={len(records)}")
    ctx = _ctx_of(records[-1]) if records else {}
    cx.checks.add("判定为 met 且状态 complete",
                  ctx.get("verdict") == "met" and ctx.get("status") == "complete",
                  f"context={json.dumps(ctx, ensure_ascii=False)}")


def _mock_goal_met(params: dict) -> list[dict]:
    return [
        _tool("write", {"path": "out.txt", "content": "done\n"}),
        _text("已创建 out.txt。"),
        _text(VERDICT_MET),
    ]


GOAL_MET = lib.Scenario(
    "goal_met",
    "目标模式:LLM 自校验判定达成 + 校验命令通过 → complete",
    _run_goal_met,
    effort="low",
    defaults={"goal": "创建 out.txt", "verify_command": "test -f out.txt"},
    mock_script=_mock_goal_met,
)


# ── goal_unmet:未达成 → 续作文本入队 ───────────────────────────────────────


def _run_goal_unmet(cx: lib.Ctx) -> None:
    env = cx.env()
    s = cx.session(env)

    s.op({"type": "enter_goal_mode", "goal": cx.p("goal") or "创建 out.txt"})
    first = s.turn("请先创建文件 out.txt(内容 done)。", timeout=cx.timeout())

    records = _goal_records(env)
    cx.checks.add("首回合后有 goal.turn.verified 记录", bool(records),
                  f"记录数={len(records)}")
    ctx = _ctx_of(records[-1]) if records else {}
    if ctx.get("verdict") == "met":
        # 真模型可能首轮就判 met —— 那就没有续作路径可观察(协议面本身
        # 无异常),判跳过而不是失败。
        lib.skip_all(cx, ["判定为 unmet(状态仍 active)", "续作文本已注入下一回合上下文"],
                     "真模型首轮即判 met,未进入续作路径")
        cx.checks.add("续作后判定 met(目标闭环)", True, "首轮即达成")
        cx.checks.add("目标达成后产物出现", cx.exists(env.ws, "out.txt"), "路径 out.txt")
        return
    cx.checks.add("判定为 unmet(状态仍 active)",
                  ctx.get("verdict") == "unmet" and ctx.get("status") == "active",
                  f"context={json.dumps(ctx, ensure_ascii=False)}")

    # 未达成 → 续作文本进 steering 队列,等下一条 UserInput 合并注入。
    # 下一回合用 request_echo 回显「模型实际看到的 messages」。
    second = s.turn("继续(检查产物是否已就绪)。", timeout=cx.timeout())
    echo_text = ""
    for evt in second.events:
        m = lib._msg(evt)
        if m.get("type") == "agent_message":
            echo_text = m.get("text") or ""
        elif m.get("type") == "agent_message_delta":
            echo_text += m.get("delta") or ""
    seen = " ".join(lib.echo_user_texts(echo_text))
    # 续作文本作为用户输入项落进 rollout —— 真模型下这是不依赖
    # 「模型回显请求」的观测口。
    rollout = env.rollout_text(s.session_id or "")
    in_rollout = "Continue working toward the active goal" in rollout
    cx.checks.add(
        "续作文本已注入下一回合上下文",
        in_rollout or "Continue working toward the active goal" in seen
        or "<objective>" in seen,
        f"rollout 含续作文本={in_rollout},模型看到的 user 文本片段={seen[:200]}",
    )
    records2 = _goal_records(env)
    ctx2 = _ctx_of(records2[-1]) if records2 else {}
    cx.checks.add("续作后判定 met(目标闭环)",
                  ctx2.get("verdict") == "met",
                  f"context={json.dumps(ctx2, ensure_ascii=False)}")
    cx.checks.add("目标达成后产物出现", cx.exists(env.ws, "out.txt"), "路径 out.txt")


def _mock_goal_unmet(params: dict) -> list[dict]:
    return [
        _tool("write", {"path": "out.txt", "content": "done\n"}),
        _text("已创建 out.txt。"),
        _text(VERDICT_UNMET),          # 第一回合的 verifier
        {"type": "request_echo"},      # 第二回合:回显模型看到的 messages
        _text(VERDICT_MET),            # 第二回合的 verifier
    ]


GOAL_UNMET = lib.Scenario(
    "goal_unmet",
    "目标模式:未达成 → 续作文本入队,下一回合合并注入后达成",
    _run_goal_unmet,
    effort="low",
    defaults={"goal": "创建 out.txt"},
    mock_script=_mock_goal_unmet,
)


# ── goal_budget:token 预算耗尽 ─────────────────────────────────────────────


def _run_goal_budget(cx: lib.Ctx) -> None:
    env = cx.env()
    s = cx.session(env)

    s.op({"type": "enter_goal_mode", "goal": cx.p("goal") or "做一个长期任务",
          "token_budget": int(cx.p("token_budget", 1))})
    turn = s.turn("先做一点(例如创建一个临时文件 work.txt)。", timeout=cx.timeout())

    records = _goal_records(env)
    ctx = _ctx_of(records[-1]) if records else {}
    cx.checks.add(
        "token 预算耗尽 → budget_limited",
        ctx.get("status") == "budget_limited",
        f"context={json.dumps(ctx, ensure_ascii=False)}",
    )
    cx.checks.add("回合本身正常收尾(预算收口不等于回合失败)",
                  turn.completed, f"终态={turn.status or turn.aborted_reason}")


def _mock_goal_budget(params: dict) -> list[dict]:
    return [_text("做了一点。"), _text(VERDICT_UNMET)]


GOAL_BUDGET = lib.Scenario(
    "goal_budget",
    "目标模式:token 预算耗尽 → budget_limited 终态",
    _run_goal_budget,
    effort="low",
    defaults={"goal": "做一个长期任务", "token_budget": 1},
    mock_script=_mock_goal_budget,
)


# ── goal_plugin:goal-queue 插件(goal / queue 工具 + 自动续作)──────────────


def _run_goal_plugin(cx: lib.Ctx) -> None:
    env = cx.env(plugins=["goal-queue"])
    s = cx.session(env)

    turn = s.turn("先用 tool_search 找到 goal/queue 工具,再用 goal 设定目标、"
                  "用 queue 排一个任务。", timeout=cx.timeout())

    tool_names = list(turn.tool_names)
    if "goal" not in tool_names and "queue" not in tool_names:
        lib.skip_all(cx, ["goal 工具被调用", "queue 工具被调用",
                          "goal / queue 调用均成功",
                          "tool_search 能搜到 goal / queue 工具"],
                     "模型未按提示调用 goal / queue 工具(真模型自由发挥)")
        return
    cx.checks.add("goal 工具被调用", "goal" in tool_names, f"工具序列={tool_names}")
    cx.checks.add("queue 工具被调用", "queue" in tool_names, f"工具序列={tool_names}")

    failed = [r for r in turn.tool_results() if r["is_error"]]
    cx.checks.add("goal / queue 调用均成功", not failed,
                  f"失败={[(r['call_id'], r['text'][:120]) for r in failed]}")

    # 插件工具以 Builtin 源注册:不进 always_on 广播集(模型经 tool_search
    # 发现),因此这里断言「tool_search 能搜到它们」而不是「广播集里有」。
    search_hit = [r for r in turn.tool_results()
                  if "queue" in r["text"] and "goal" in r["text"]]
    cx.checks.add("tool_search 能搜到 goal / queue 工具", bool(search_hit),
                  f"搜索命中={len(search_hit)}")


def _mock_goal_plugin(params: dict) -> list[dict]:
    return [
        _tool("tool_search", {"query": "goal"}),
        _tool("goal", {"action": "enter", "goal": "整理工作区"}),
        _tool("queue", {"action": "add", "prompt": "再整理一次"}),
    ]


GOAL_PLUGIN = lib.Scenario(
    "goal_plugin",
    "goal-queue 插件:goal / queue 工具注册且可调用",
    _run_goal_plugin,
    effort="low",
    defaults={},
    mock_script=_mock_goal_plugin,
)
