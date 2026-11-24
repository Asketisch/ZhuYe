#!/usr/bin/env python3
"""CLI 真模型端到端驱动器(zhuye serve + stdin JSONL Submission 协议)。

驱动一轮完整 agent 会话:plan 模式 → 出 plan → 自动批准 → 正常开发 →
git init + commit。只断言协议形状与产物存在性,不断言模型文本 —— 与
L6 层「测链路,不测措辞」同一哲学。

输入输出经 `--result` JSON 文件交付,由 cli_e2e.sh 的 tcase 消费;
事件全量留档 `--events`(失败定位第一手材料)。

协议要点(与 zhuye-protocol 对齐):
- Submission: `{"id": <客户端生成>, "op": {"type": <snake_case>, ...}}`
- Event:     `{"id": <关联 submission id>, "msg": {"type": ..., ...}}`
- 关键事件:  session_configured / plan_ready / plan_approved /
             permission_mode_changed / token_count / turn_complete / error

退出码:0 = PASS;2 = 环境性跳过(端点不可达);1 = FAIL。
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time

# 单阶段(出 plan / 开发 / 收尾)的超时秒数,可用环境变量覆盖。
DEFAULT_PHASE_TIMEOUT = 1200


class PhaseTimeout(Exception):
    """单阶段超时。"""


class SessionDied(Exception):
    """serve 进程提前退出(stdout 关闭)。"""


def send(proc, sub_id: str, op: dict) -> None:
    """投递一条 Submission(stdin JSONL)。"""
    line = json.dumps({"id": sub_id, "op": op}, ensure_ascii=False)
    proc.stdin.write(line + "\n")
    proc.stdin.flush()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--bin", required=True, help="zhuye CLI 二进制路径")
    ap.add_argument("--workspace", required=True, help="隔离的临时工作区")
    ap.add_argument("--home", required=True, help="隔离的 HOME(ZHUYE_HOME 同根)")
    ap.add_argument("--task-file", required=True, help="任务提示词文件")
    ap.add_argument("--continue-file", required=True, help="继续/收尾指令文件")
    ap.add_argument("--effort", default="xhigh", help="思考强度(默认 xhigh)")
    ap.add_argument("--events", required=True, help="事件全量留档路径")
    ap.add_argument("--result", required=True, help="结果 JSON 输出路径")
    ap.add_argument(
        "--phase-timeout",
        type=int,
        default=int(os.environ.get("ZHUYE_E2E_PHASE_TIMEOUT", DEFAULT_PHASE_TIMEOUT)),
        help="单阶段超时秒数",
    )
    args = ap.parse_args()

    with open(args.task_file, encoding="utf-8") as f:
        task_text = f.read().strip()
    with open(args.continue_file, encoding="utf-8") as f:
        continue_text = f.read().strip()

    result: dict = {
        "status": "FAIL",
        "reason": "",
        "session_id": None,
        "model": None,
        "plan_ready": False,
        "plan_markdown_chars": 0,
        "plan_approved": False,
        "permission_flow": [],
        "turns_completed": 0,
        "errors": [],
        "error_codes": [],
        "max_input_tokens": 0,
        "model_calls": 0,
        "context_compacted": 0,
        "ask_user": False,
    }
    events_file = open(args.events, "w", encoding="utf-8")

    def finish() -> int:
        kill()
        events_file.close()
        with open(args.result, "w", encoding="utf-8") as f:
            json.dump(result, f, ensure_ascii=False, indent=2)
        print(json.dumps(result, ensure_ascii=False))
        if result["status"] == "PASS":
            return 0
        if result["status"] == "SKIP":
            return 2
        return 1

    def kill():
        try:
            proc.stdin.close()
        except Exception:
            pass
        try:
            proc.wait(timeout=5)
        except Exception:
            proc.kill()

    env = dict(os.environ)
    env["HOME"] = args.home
    env["ZHUYE_HOME"] = os.path.join(args.home, ".zhuye")
    proc = subprocess.Popen(
        [args.bin, "serve", "--plan-mode"],
        cwd=args.workspace,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        bufsize=1,
    )

    try:
        # ── 阶段 0:等待会话配置,确认模型解析成功 ────────────────────
        deadline = time.monotonic() + 60
        configured = None
        while configured is None:
            if time.monotonic() > deadline:
                raise PhaseTimeout("等待 session_configured 超时(60s)")
            line = proc.stdout.readline()
            if not line:
                raise SessionDied("serve 提前退出(启动阶段)")
            events_file.write(line)
            msg = json.loads(line).get("msg", {})
            if msg.get("type") == "session_configured":
                configured = msg
        result["session_id"] = configured.get("session_id")
        result["model"] = configured.get("model")
        if not configured.get("model"):
            result["reason"] = "session_configured 缺 model —— 模型解析失败"
            return finish()

        # ── 阶段 1:思考强度 + 任务 → plan_ready ─────────────────────
        send(proc, "e2e-effort", {"type": "set_effort", "effort": args.effort})
        plan_sub = "e2e-task"
        send(
            proc,
            plan_sub,
            {"type": "user_input", "items": [{"type": "text", "text": task_text}]},
        )
        plan_id, plan_done = None, False
        deadline = time.monotonic() + args.phase_timeout
        while plan_id is None:
            if time.monotonic() > deadline:
                raise PhaseTimeout(f"等待 plan_ready 超时({args.phase_timeout}s)")
            line = proc.stdout.readline()
            if not line:
                raise SessionDied("serve 提前退出(plan 阶段)")
            events_file.write(line)
            evt = json.loads(line)
            absorb(evt, result)
            msg = evt.get("msg", {})
            if msg.get("type") == "plan_ready":
                plan_id = msg.get("plan_id")
                result["plan_markdown_chars"] = len(msg.get("markdown") or "")
            elif msg.get("type") == "turn_complete" and evt.get("id") == plan_sub:
                plan_done = True
                break  # 回合已收尾,不必再等 plan_ready(超时前及时回报)
            elif msg.get("type") == "error" and is_fatal(msg):
                result["reason"] = f"plan 阶段结构性错误: {msg.get('code')}"
                return finish()
        if plan_done:
            # 回合结束仍无 plan —— 模型以文本收尾未调 ExitPlanMode。
            result["reason"] = "plan 回合结束但未产出 plan_ready(模型未调 ExitPlanMode)"
            return finish()
        if result["plan_markdown_chars"] <= 0:
            result["reason"] = "plan_ready 的 markdown 为空"
            return finish()
        result["plan_ready"] = True

        # ── 阶段 2:确认 plan(自动批准,进入开发模式)────────────────
        # 相当于人工「确认 plan 没问题」:审阅 markdown 非空即批准。
        approve_sub = "e2e-approve"
        send(
            proc,
            approve_sub,
            {"type": "plan_approval", "id": plan_id, "choice": "auto_mode"},
        )
        approved, dev_done = False, False
        deadline = time.monotonic() + args.phase_timeout
        while not dev_done:
            if time.monotonic() > deadline:
                raise PhaseTimeout(f"开发回合超时({args.phase_timeout}s)")
            line = proc.stdout.readline()
            if not line:
                raise SessionDied("serve 提前退出(审批/开发阶段)")
            events_file.write(line)
            evt = json.loads(line)
            absorb(evt, result)
            msg = evt.get("msg", {})
            if msg.get("type") == "plan_approved":
                approved = True
            elif msg.get("type") == "turn_complete" and evt.get("id") == approve_sub:
                dev_done = True
            elif msg.get("type") == "error" and is_fatal(msg):
                result["reason"] = f"开发阶段结构性错误: {msg.get('code')}"
                return finish()
        result["plan_approved"] = approved
        if not approved:
            result["reason"] = "plan_approved 事件缺失 —— 审批链路未生效"
            return finish()

        # ── 阶段 3:收尾指令(git init + commit 等)→ 终态 ────────────
        final_sub = "e2e-continue"
        send(
            proc,
            final_sub,
            {"type": "user_input", "items": [{"type": "text", "text": continue_text}]},
        )
        deadline = time.monotonic() + args.phase_timeout
        while True:
            if time.monotonic() > deadline:
                raise PhaseTimeout(f"收尾回合超时({args.phase_timeout}s)")
            line = proc.stdout.readline()
            if not line:
                raise SessionDied("serve 提前退出(收尾阶段)")
            events_file.write(line)
            evt = json.loads(line)
            absorb(evt, result)
            msg = evt.get("msg", {})
            if msg.get("type") == "turn_complete" and evt.get("id") == final_sub:
                result["turns_completed"] += 1
                if msg.get("status") == "success":
                    result["status"] = "PASS"
                else:
                    result["reason"] = f"收尾回合终态 {msg.get('status')}"
                break
            if msg.get("type") == "error" and is_fatal(msg):
                result["reason"] = f"收尾阶段结构性错误: {msg.get('code')}"
                return finish()

        return finish()
    except SessionDied as e:
        result["reason"] = str(e)
        # 0 次模型调用 = 请求根本没发出去(端点不可达),环境问题判 SKIP。
        if result["model_calls"] == 0:
            result["status"] = "SKIP"
            result["reason"] = f"模型端点不可达: {e}"
        return finish()
    except PhaseTimeout as e:
        result["reason"] = str(e)
        return finish()


def absorb(evt: dict, result: dict) -> None:
    """把关心的事件归进结果摘要(全量另行留档)。"""
    evt_id = evt.get("id", "")
    msg = evt.get("msg", {})
    t = msg.get("type")
    if t == "permission_mode_changed":
        result["permission_flow"].append(f"{msg.get('from')}→{msg.get('to')}")
    elif t == "token_count":
        result["model_calls"] += 1
        result["max_input_tokens"] = max(
            result["max_input_tokens"], msg.get("input_tokens") or 0
        )
    elif t == "context_compacted":
        result["context_compacted"] += 1
    elif t == "error":
        result["errors"].append(json.dumps(msg, ensure_ascii=False)[:300])
        code = msg.get("code") or ""
        if code:
            result["error_codes"].append(code)
    elif t == "ask_user_question":
        result["ask_user"] = True
    elif t == "turn_complete" and evt_id.startswith("e2e-task"):
        result["turns_completed"] += 1


def is_fatal(msg: dict) -> bool:
    """结构性错误(fail-fast);模型侧可恢复错误交给回合自然收尾。"""
    code = msg.get("code") or ""
    return code in ("compact_rapid_refill", "invalid_submission", "prompt_rejected")


if __name__ == "__main__":
    sys.exit(main())
