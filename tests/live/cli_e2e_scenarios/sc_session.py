"""session 组场景:会话生命周期(续作 / 回退 / 限额)。

引擎事实(与 zhuye-core / zhuye-exec 对齐,改动需同步改本文件):

- 续作:`zhuye serve --resume <session_id>` 从 rollout 回放历史并预载
  (`preload_messages`),`session_configured` 事件带**同一个**
  session_id(没有单独的 session_resumed 事件);每轮 UserInput 前
  core 还会 `recorder.replay(session_id)` 重建历史(跨轮不失忆);
- 回退:`Op::Rewind {to_turn_id}`(None = 回退到最近一条 user turn)
  截断 rollout 记录并发 `turn_rewound` {to_turn_id, truncated_after};
  下一轮的历史随之变短(回放口径);
- 迭代上限:`[active].max_iterations`(或 env `ZHUYE_MAX_ITERATIONS`)
  触顶 → turn 以 `status = max_iterations` 收尾;若模型没给出任何
  文本,额外发一条 `iteration_budget_exhausted` 错误事件;
- token 预算:`[token_budget].session_total_tokens`(或 env
  `ZHUYE_TOKEN_BUDGET`)耗尽 → turn 以 `status = token_budget_exceeded`
  收尾。
"""

from __future__ import annotations

import json

import cli_e2e_lib as lib


def _text(value: str) -> dict:
    return {"type": "text", "text": value}


def _tool(name: str, args: dict) -> dict:
    return {"type": "tool_call", "name": name, "args": args}


def _echo_text(turn: lib.Turn) -> str:
    buf = []
    for evt in turn.events:
        m = lib._msg(evt)
        if m.get("type") == "agent_message":
            buf.append(m.get("text") or "")
        elif m.get("type") == "agent_message_delta":
            buf.append(m.get("delta") or "")
    return "".join(buf)


# ── resume:续作(回放历史)──────────────────────────────────────────────────


def _run_resume(cx: lib.Ctx) -> None:
    env = cx.env()
    s = cx.session(env)
    first = s.turn("第一轮:请记住暗号是「青鸟」。", timeout=cx.timeout())
    cx.checks.add("首个会话回合正常收尾", first.completed,
                  f"终态={first.status or first.aborted_reason}")
    session_id = s.session_id
    cx.checks.add("拿到 session_id", bool(session_id), f"session_id={session_id}")

    # 同工作区起第二个 serve,带 --resume 回放历史。
    s2 = cx.session(env, resume=session_id, label="resumed")
    cx.checks.add("续作后 session_id 一致", s2.session_id == session_id,
                  f"原={session_id},续作={s2.session_id}")

    turn = s2.turn("第二轮:暗号是什么?", timeout=cx.timeout())
    echo = _echo_text(turn)
    seen = " ".join(lib.echo_user_texts(echo))
    cx.checks.add("续作会话恢复了上一轮历史(模型看得到旧输入)",
                  "青鸟" in seen, f"模型看到的 user 文本={seen[:240]}")


def _mock_resume(params: dict) -> list[dict]:
    # 每个 serve 进程各自从头读脚本(mock 客户端按进程加载),所以第 1 行
    # 放 request_echo:首会话与续作会话的第一句模型调用都能回显
    # 「模型实际看到的 messages」—— 续作会话那份含历史。
    return [
        {"type": "request_echo"},
        _text("暗号是青鸟。"),
    ]


RESUME = lib.Scenario(
    "resume",
    "会话续作:--resume 回放历史,模型看得到上一轮输入",
    _run_resume,
    effort="low",
    defaults={},
    mock_script=_mock_resume,
)


# ── rewind:回退到上一条 user turn ─────────────────────────────────────────


def _run_rewind(cx: lib.Ctx) -> None:
    env = cx.env()
    s = cx.session(env)
    first = s.turn("第一轮:请记住暗号是「青鸟」。", timeout=cx.timeout())
    second = s.turn("第二轮:再确认一次暗号是「青鸟」。", timeout=cx.timeout())
    cx.checks.add("两轮均正常收尾", first.completed and second.completed,
                  f"终态={first.status},{second.status}")

    s.op({"type": "rewind", "to_turn_id": None})
    evt = s.wait(lambda e: lib.etype(e) == "turn_rewound", 30, what="turn_rewound")
    m = lib._msg(evt)
    cx.checks.add("收到 turn_rewound 事件", evt is not None,
                  f"to_turn_id={m.get('to_turn_id')},truncated_after={m.get('truncated_after')}")

    # 回退后:新一轮的上下文里不应再有被回退那一轮的输入。
    turn = s.turn("回退后重新提问:暗号是什么?", timeout=cx.timeout())
    seen = " ".join(lib.echo_user_texts(_echo_text(turn)))
    cx.checks.add("被回退那一轮的输入已从上下文移除",
                  "再确认一次" not in seen, f"模型看到的 user 文本={seen[:240]}")
    cx.checks.add("更早的输入仍在上下文里(只回退最近一轮)",
                  "第一轮" in seen, f"模型看到的 user 文本={seen[:240]}")
    cx.checks.add("回退后回合正常收尾", turn.completed,
                  f"终态={turn.status or turn.aborted_reason}")


def _mock_rewind(params: dict) -> list[dict]:
    return [
        _text("记住了:青鸟。"),
        _text("确认:青鸟。"),
        # 回退后的新一轮:回显「模型实际看到的 messages」。
        {"type": "request_echo"},
        _text("暗号是青鸟。"),
    ]


REWIND = lib.Scenario(
    "rewind",
    "会话回退:Op::Rewind 截断历史,下一轮看不到被回退的输入",
    _run_rewind,
    effort="low",
    defaults={},
    mock_script=_mock_rewind,
)


# ── max_iterations:迭代上限 ────────────────────────────────────────────────


def _run_max_iterations(cx: lib.Ctx) -> None:
    limit = int(cx.p("max_iterations", 3))
    env = cx.env(max_iterations=limit)
    s = cx.session(env)
    turn = s.turn("请用 bash 工具反复执行命令 `echo tick`(至少 8 次),最后再收尾。",
                  timeout=cx.timeout())
    cx.checks.add(
        "触达迭代上限时以 max_iterations 收尾",
        turn.status == "max_iterations",
        f"status={turn.status or turn.aborted_reason}",
    )
    codes = sorted(set(turn.error_codes))
    cx.checks.add(
        "无文本产出时补发 iteration_budget_exhausted",
        ("iteration_budget_exhausted" in codes) or bool(turn.text.strip()),
        f"错误码={codes},文本长度={len(turn.text)}",
    )


def _mock_max_iterations(params: dict) -> list[dict]:
    limit = int(params.get("max_iterations", 3))
    return [_tool("bash", {"command": "echo tick"}) for _ in range(limit + 2)]


MAX_ITERATIONS = lib.Scenario(
    "max_iterations",
    "迭代上限:max_iterations 触顶后回合以 max_iterations 收尾",
    _run_max_iterations,
    effort="low",
    defaults={"max_iterations": 3},
    mock_script=_mock_max_iterations,
)


# ── token_budget:会话级 token 预算 ─────────────────────────────────────────


def _run_token_budget(cx: lib.Ctx) -> None:
    budget = int(cx.p("session_total_tokens", 1))
    env = cx.env(extra_toml=f"[token_budget]\nsession_total_tokens = {budget}\n")
    s = cx.session(env)
    turn = s.turn("请用 bash 工具反复执行命令 `echo tick`(至少 8 次),最后再收尾。",
                  timeout=cx.timeout())
    cx.checks.add(
        "token 预算耗尽时以 token_budget_exceeded 收尾",
        turn.status == "token_budget_exceeded",
        f"status={turn.status or turn.aborted_reason}",
    )
    codes = sorted(set(turn.error_codes))
    cx.checks.add("预算收口有明确错误码或状态", bool(codes) or turn.status is not None,
                  f"错误码={codes},status={turn.status}")


def _mock_token_budget(params: dict) -> list[dict]:
    return [_tool("bash", {"command": "echo tick"}) for _ in range(3)] + [_text("收尾。")]


TOKEN_BUDGET = lib.Scenario(
    "token_budget",
    "会话级 token 预算:耗尽后回合以 token_budget_exceeded 收尾",
    _run_token_budget,
    effort="low",
    defaults={"session_total_tokens": 1},
    mock_script=_mock_token_budget,
)
