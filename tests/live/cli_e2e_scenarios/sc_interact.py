"""interact 组场景:人机交互(中断 / 转向 / 提问 / 审批)。

引擎事实(与 zhuye-core / zhuye-tools 对齐,改动需同步改本文件):

- `Op::Interrupt`:取消在飞 turn 的取消令牌 → turn 任务发**一条**
  `turn_aborted`(reason = user_interrupt),不发 turn_complete;空闲时
  收到 interrupt 也会回一条 turn_aborted 作确认;bash 工具在等待期
  `select!` 取消令牌,子进程被杀掉(长 sleep 也能立刻中断);
- `Op::Steer {priority, items}`:不打断当前 turn,消息进 steering 队列,
  在下一个安全点(pre_loop 回环入口)注入为用户消息;`priority = "now"`
  是「用户中途说话」,`attachment` 以 `<system-reminder>` 包裹当参考资料;
- `ask_user_question` 工具 → `EventMsg::AskUserQuestion`
  {request_id, questions:[{header,question,options:[{label,description}],
  multi_select}]};回执 `Op::AskUserQuestionResponse {id,
  answers:{answers:[{selected:[i],custom?}]}}`;
  `ask_user` / `request_human_input` → `EventMsg::AskUserInput`
  {request_id, prompt, secret?, placeholder?};回执
  `Op::AskUserInputResponse {id, text}`;
- 审批:env `ZHUYE_APPROVALS=1` 打开 ApprovalGate。默认权限档
  (auto)下 write/edit 类自动放行,**非只读 bash** 会弹审批:
  `approval_request` {request_id, kind:{type:"tool",tool_name,args},
  risk?};回执 `Op::ToolApproval {id, decision}`,decision ∈
  approve / approve_for_session / {deny:{reason}} / {reject_for_session:{reason}}。
"""

from __future__ import annotations

import json

import cli_e2e_lib as lib


def _text(value: str) -> dict:
    return {"type": "text", "text": value}


def _tool(name: str, args: dict) -> dict:
    return {"type": "tool_call", "name": name, "args": args}


def _echo_text(turn: lib.Turn) -> str:
    """取回本回合里 request_echo 类型的回复文本。"""
    buf = []
    for evt in turn.events:
        m = lib._msg(evt)
        if m.get("type") == "agent_message":
            buf.append(m.get("text") or "")
        elif m.get("type") == "agent_message_delta":
            buf.append(m.get("delta") or "")
    return "".join(buf)


# ── interrupt:回合中途中断 ─────────────────────────────────────────────────


def _run_interrupt(cx: lib.Ctx) -> None:
    env = cx.env()
    s = cx.session(env)

    s.user_input("请用 bash 工具执行命令 `sleep 30`(必须走 bash 工具)。")
    started = s.wait(lambda e: lib.etype(e) == "turn_started", cx.timeout(),
                     what="turn_started")
    turn_id = lib._msg(started).get("turn_id")
    s.wait(lambda e: lib.etype(e) == "tool_call_begin", cx.timeout(),
           what="工具开始执行")

    s.op({"type": "interrupt"})
    aborted = s.wait(
        lambda e: lib.etype(e) == "turn_aborted" and lib._msg(e).get("turn_id") == turn_id,
        60, what="turn_aborted",
    )
    reason = lib._msg(aborted).get("reason") or {}
    reason_type = reason.get("type") if isinstance(reason, dict) else reason
    cx.checks.add("turn 以 turn_aborted 收口", aborted is not None, f"reason={reason}")
    cx.checks.add("中断原因标记为 user_interrupt",
                  reason_type == "user_interrupt", f"reason={json.dumps(reason, ensure_ascii=False)}")
    completes = [e for e in s.events
                 if lib.etype(e) == "turn_complete" and lib._msg(e).get("turn_id") == turn_id]
    cx.checks.add("中断后不再发 turn_complete", not completes,
                  f"turn_complete 数={len(completes)}")

    # 会话未被中断波及:下一条输入仍能正常跑完。
    nxt = s.turn("还在吗?", timeout=cx.timeout())
    cx.checks.add("中断后会话可继续", nxt.completed,
                  f"终态={nxt.status or nxt.aborted_reason}")


def _mock_interrupt(params: dict) -> list[dict]:
    return [
        _tool("bash", {"command": f"sleep {int(params.get('sleep_secs', 30))}"}),
        _text("命令已结束。"),
    ]


INTERRUPT = lib.Scenario(
    "interrupt",
    "回合中途中断:长 sleep 被取消,turn 以 user_interrupt 收口",
    _run_interrupt,
    effort="low",
    defaults={"sleep_secs": 30},
    mock_script=_mock_interrupt,
)


# ── steer:回合中途转向 ─────────────────────────────────────────────────────


def _run_steer(cx: lib.Ctx) -> None:
    env = cx.env()
    s = cx.session(env)

    s.user_input("请先用 bash 工具执行命令 `echo step1`,再汇报结果。")
    s.wait(lambda e: lib.etype(e) == "turn_started", cx.timeout(), what="turn_started")
    s.wait(lambda e: lib.etype(e) == "tool_call_begin", cx.timeout(),
           what="工具开始执行")

    steer_text = cx.p("steer_text") or "改用方案 B:不要写文件,只汇报。"
    s.op({"type": "steer", "priority": cx.p("priority") or "now",
          "items": [{"type": "text", "text": steer_text}]})

    # 转向消息在下一个安全点注入 → 后续那次模型请求里应能看到它。
    s.wait(lambda e: lib.etype(e) == "turn_complete", cx.timeout(),
           what="回合结束")
    echo = ""
    for evt in s.events:
        m = lib._msg(evt)
        if m.get("type") in ("agent_message", "agent_message_delta"):
            echo += (m.get("text") or m.get("delta") or "")
    seen = " ".join(lib.echo_user_texts(echo))
    # 注入的转向消息会作为用户输入项落进 rollout —— 真模型下这是唯一
    # 不依赖「模型回显请求」的观测口(mock 自检另有 request_echo 佐证)。
    rollout = env.rollout_text(s.session_id or "")
    cx.checks.add(
        "转向消息注入到后续模型上下文(rollout 可见)",
        steer_text in rollout,
        f"rollout 含转向文本={steer_text in rollout}",
    )
    if seen:
        cx.checks.add(
            "转向消息出现在模型请求的 user 文本里",
            steer_text in seen,
            f"模型看到的 user 文本={seen[:240]}",
        )
        cx.checks.add(
            "转向消息带 <system-reminder> 包裹(attachment 档)",
            ("<system-reminder>" in seen) == (str(cx.p("priority") or "now") == "attachment"),
            f"seen={seen[:160]}",
        )
    else:
        cx.checks.skip("转向消息出现在模型请求的 user 文本里",
                       "本轮没有 request_echo 回显(真模型不回显请求)")
        cx.checks.skip("转向消息带 <system-reminder> 包裹(attachment 档)",
                       "本轮没有 request_echo 回显(真模型不回显请求)")


def _mock_steer(params: dict) -> list[dict]:
    return [
        _tool("bash", {"command": "echo step1"}),
        {"type": "request_echo"},
        _text("已按转向要求汇报。"),
    ]


STEER = lib.Scenario(
    "steer",
    "回合中途转向:steer 消息在下一个安全点注入模型上下文",
    _run_steer,
    effort="low",
    defaults={"priority": "now", "steer_text": "改用方案 B:不要写文件,只汇报。"},
    mock_script=_mock_steer,
)


# ── ask_user:向用户提问并接收回答 ──────────────────────────────────────────


def _run_ask_user(cx: lib.Ctx) -> None:
    # ask_user(自由文本提问)走审批门 → 需要打开 ApprovalGate。
    env = cx.env(env_extra={"ZHUYE_APPROVALS": "1"})
    s = cx.session(env)

    questions: list[dict] = []
    inputs: list[dict] = []

    def answer_question(session, evt):
        if lib.etype(evt) != "ask_user_question":
            return False
        m = lib._msg(evt)
        questions.append(m)
        session.send(session._new_id("answer"), {
            "type": "ask_user_question_response",
            "id": m.get("request_id"),
            "answers": {"answers": [{"selected": [0]}]},
        })
        return True

    def answer_input(session, evt):
        if lib.etype(evt) != "ask_user_input":
            return False
        m = lib._msg(evt)
        inputs.append(m)
        session.send(session._new_id("input"), {
            "type": "ask_user_input_response",
            "id": m.get("request_id"),
            "text": cx.p("answer_text") or "用方案 A",
        })
        return True

    turn = s.turn("请调用 ask_user_question 工具问我要选哪个方案"
                  "(不要直接给答案),并等待我的回答。", timeout=cx.timeout(),
                  respond=[answer_question, answer_input])

    cx.checks.add("收到 ask_user_question 事件", bool(questions),
                  f"问题数={len(questions)},首个={questions[0] if questions else None}")
    if questions:
        q = questions[0].get("questions") or []
        cx.checks.add("问题结构含 header/question/options",
                      bool(q) and "header" in q[0] and "question" in q[0] and "options" in q[0],
                      f"questions={json.dumps(q, ensure_ascii=False)[:200]}")
    cx.checks.add("收到 ask_user_input 事件(自由文本提问)", bool(inputs),
                  f"输入请求数={len(inputs)}")
    failed = [r for r in turn.tool_results() if r["is_error"]]
    cx.checks.add("提问工具调用未被判错", not failed,
                  f"失败={[(r['call_id'], r['text'][:120]) for r in failed]}")
    cx.checks.add("回合正常收尾", turn.completed,
                  f"终态={turn.status or turn.aborted_reason}")


def _mock_ask_user(params: dict) -> list[dict]:
    return [
        _tool("ask_user_question", {
            "questions": [{
                "header": "方案",
                "question": "用哪个方案?",
                "options": [{"label": "方案 A", "description": "先用 A"},
                            {"label": "方案 B", "description": "改用 B"}],
                "multi_select": False,
            }]
        }),
        _tool("ask_user", {"prompt": "补充一句说明"}),
        _text("已收到回答,继续。"),
    ]


ASK_USER = lib.Scenario(
    "ask_user",
    "向用户提问:ask_user_question / ask_user 的事件与回执闭环",
    _run_ask_user,
    effort="low",
    defaults={"answer_text": "用方案 A"},
    mock_script=_mock_ask_user,
)


# ── approval:工具审批(approve / deny)──────────────────────────────────────


def _run_approval(cx: lib.Ctx) -> None:
    env = cx.env(env_extra={"ZHUYE_APPROVALS": "1"})
    s = cx.session(env)

    requests: list[dict] = []
    approve = lib.on_approval("approve", log=requests)
    turn = s.turn("请用 bash 工具执行命令 `echo approved > approved.txt`"
                  "(必须走 bash 工具,不要用 write 工具)。",
                  timeout=cx.timeout(), respond=[approve])

    if not requests and "bash" not in turn.tool_names:
        lib.skip_all(cx, ["产生 approval_request 事件", "审批类型为 tool(kind.type=tool)",
                          "审批请求带 request_id", "批准后命令真的执行了(产物落盘)"],
                     "模型未按提示调用 bash(真模型自由发挥),无审批请求可观察")
        return
    cx.checks.add("产生 approval_request 事件", bool(requests),
                  f"审批请求数={len(requests)}")
    if requests:
        kind = requests[0].get("kind") or {}
        cx.checks.add("审批类型为 tool(kind.type=tool)",
                      kind.get("type") == "tool",
                      f"kind={json.dumps(kind, ensure_ascii=False)[:200]}")
        cx.checks.add("审批请求带 request_id",
                      bool(requests[0].get("request_id")),
                      f"request_id={requests[0].get('request_id')}")
    cx.checks.add("批准后命令真的执行了(产物落盘)",
                  cx.exists(env.ws, "approved.txt"), "路径 approved.txt")
    cx.checks.add("回合正常收尾", turn.completed,
                  f"终态={turn.status or turn.aborted_reason}")


def _mock_approval(params: dict) -> list[dict]:
    return [
        _tool("bash", {"command": "echo approved > approved.txt"}),
        _text("命令已执行。"),
    ]


APPROVAL = lib.Scenario(
    "approval",
    "工具审批:非只读 bash 触发 approval_request,批准后放行",
    _run_approval,
    effort="low",
    defaults={},
    mock_script=_mock_approval,
)


# ── approval_deny:拒绝后工具不执行 ────────────────────────────────────────


def _run_approval_deny(cx: lib.Ctx) -> None:
    env = cx.env(env_extra={"ZHUYE_APPROVALS": "1"})
    s = cx.session(env)

    requests: list[dict] = []
    deny = lib.on_approval("deny", reason="e2e 拒绝执行", log=requests)
    turn = s.turn("请用 bash 工具执行命令 `echo denied > denied.txt`"
                  "(必须走 bash 工具)。",
                  timeout=cx.timeout(), respond=[deny])

    if not requests and "bash" not in turn.tool_names:
        lib.skip_all(cx, ["产生 approval_request 事件", "被拒后命令未执行(无落盘)",
                          "被拒的工具调用回执为错误"],
                     "模型未按提示调用 bash(真模型自由发挥),无审批请求可观察")
        return
    cx.checks.add("产生 approval_request 事件", bool(requests),
                  f"审批请求数={len(requests)}")
    cx.checks.add("被拒后命令未执行(无落盘)",
                  not cx.exists(env.ws, "denied.txt"), "路径 denied.txt")
    failed = [r for r in turn.tool_results() if r["is_error"]]
    cx.checks.add("被拒的工具调用回执为错误", bool(failed),
                  f"失败回执数={len(failed)}")
    cx.checks.add("会话未崩溃(回合仍有终态)",
                  turn.completed or bool(turn.aborted_reason),
                  f"终态={turn.status or turn.aborted_reason}")


def _mock_approval_deny(params: dict) -> list[dict]:
    return [
        _tool("bash", {"command": "echo denied > denied.txt"}),
        _text("命令未执行。"),
    ]


APPROVAL_DENY = lib.Scenario(
    "approval_deny",
    "工具审批:拒绝后命令不执行、工具回执为错误",
    _run_approval_deny,
    effort="low",
    defaults={},
    mock_script=_mock_approval_deny,
)
