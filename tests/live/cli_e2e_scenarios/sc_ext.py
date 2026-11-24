"""ext 组场景:扩展能力(子代理 / 后台任务 / 远程工具 / 管理面 Op)。

引擎事实(与 zhuye-subagent / zhuye-tools / zhuye-exec 对齐):

- 子代理:LLM 侧工具名是 `call_<role>`(如 `call_explorer`);子代理跑
  在独立会话里(自己的模型调用 —— 会消耗 mock 脚本行!),进度经
  `subagent_progress` {child_id, role, kind: message|tool_begin|tool_end,
  text, call_id?} 汇报;`Op::QuerySubagents` 回 `subagent_status`
  {children:[{child_id, role, state: running|completed|failed|cancelled,
  iteration, total_tokens, ...}]};
- 后台任务:bash 工具带 `run_in_background = true` → 返回
  {"background":true, task_id} 与文本「[background task {id} started]」;
  完成后**没有**专门的 wire 事件 —— 结果被拼成文本项
  「[background task {id} ({kind}) completed]\\n{result}」注入**下一轮**
  UserInput;查询用 `background_status` 工具;
- 远程工具:`Op::RegisterTools` 在 serve 的 stdin 循环本地处理(不进
  core),模型调用时 core 发 `tool_execution_request`
  {call_id, tool, args},客户端用 `Op::ToolExecutionResponse`
  {call_id, output} 回执;名字与既有工具撞车 → `tool_name_conflict`
  错误事件;
- 管理面 Op(不开回合,各自回一条事件):
  `Op::QuerySubagents` → subagent_status;`Op::QueryGuardStats` →
  guard_stats;`Op::PromptCommand{action:"list"}` → prompt_list;
  `Op::RescanExtensions` → extensions_rescanned;
  `Op::SetPermissionMode{mode}` / `Op::CyclePermissionMode` →
  permission_mode_changed。
"""

from __future__ import annotations

import json
import time

import cli_e2e_lib as lib


def _text(value: str) -> dict:
    return {"type": "text", "text": value}


def _tool(name: str, args: dict) -> dict:
    return {"type": "tool_call", "name": name, "args": args}


# ── subagent:子代理派生 ────────────────────────────────────────────────────


def _run_subagent(cx: lib.Ctx) -> None:
    env = cx.env()
    s = cx.session(env)
    turn = s.turn("请用 call_explorer 工具让 explorer 帮我找出关键文件。",
                  timeout=cx.timeout())

    if "call_explorer" not in turn.tool_names:
        lib.skip_all(cx, ["call_explorer 工具被调用", "收到 subagent_progress 事件",
                          "进度事件带 child_id / role", "子代理调用未被判错",
                          "subagent_status 回(child 列表)"],
                     "模型未按提示调用 call_explorer(真模型自由发挥)")
        return
    cx.checks.add("call_explorer 工具被调用", "call_explorer" in turn.tool_names,
                  f"工具序列={turn.tool_names}")
    progress = [lib._msg(e) for e in turn.events if lib.etype(e) == "subagent_progress"]
    cx.checks.add("收到 subagent_progress 事件", bool(progress),
                  f"事件数={len(progress)},首个={json.dumps(progress[0], ensure_ascii=False)[:160] if progress else None}")
    if progress:
        cx.checks.add("进度事件带 child_id / role",
                      bool(progress[0].get("child_id")) and bool(progress[0].get("role")),
                      f"child_id={progress[0].get('child_id')},role={progress[0].get('role')}")
    failed = [r for r in turn.tool_results() if r["is_error"]]
    cx.checks.add("子代理调用未被判错", not failed,
                  f"失败={[(r['call_id'], r['text'][:160]) for r in failed]}")

    # 状态查询:Op::QuerySubagents → subagent_status
    s.op({"type": "query_subagents"})
    evt = s.wait(lambda e: lib.etype(e) == "subagent_status", 30, what="subagent_status")
    m = lib._msg(evt)
    children = m.get("children") or []
    cx.checks.add("subagent_status 回(child 列表)",
                  isinstance(children, list), f"children={json.dumps(children, ensure_ascii=False)[:200]}")


def _mock_subagent(params: dict) -> list[dict]:
    return [
        _tool("call_explorer", {"prompt": "找出关键文件"}),
        _text("子代理:找到 src/main.rs。"),   # 子代理自己的模型回复
        _text("子代理已完成。"),
    ]


SUBAGENT = lib.Scenario(
    "subagent",
    "子代理:派生 call_explorer,回报 subagent_progress 与状态快照",
    _run_subagent,
    effort="low",
    defaults={},
    mock_script=_mock_subagent,
)


# ── background:后台 bash 任务 ──────────────────────────────────────────────


def _run_background(cx: lib.Ctx) -> None:
    env = cx.env()
    s = cx.session(env)
    first = s.turn("在后台跑一条命令,然后收尾。", timeout=cx.timeout())

    results = [r for r in first.tool_results() if "background task" in r["text"]]
    if not results and "bash" not in first.tool_names:
        lib.skip_all(cx, ["bash 以 run_in_background 启动(回执含后台任务 id)",
                          "后台任务结果注入到下一轮上下文"],
                     "模型未按提示调用带 run_in_background 的 bash(真模型自由发挥)")
        return
    cx.checks.add("bash 以 run_in_background 启动(回执含后台任务 id)",
                  bool(results), f"回执={[r['text'][:120] for r in results]}")

    # 后台结果在**下一轮** UserInput 的回合边界注入(drain_completed),
    # 等后台任务真正跑完再发下一条输入。
    time.sleep(2.0)
    second = s.turn("刚才后台任务的结果是什么?", timeout=cx.timeout())
    seen = " ".join(lib.echo_user_texts(lib.echo_text(second)))
    cx.checks.add("后台任务结果注入到下一轮上下文",
                  "background task" in seen, f"下一轮 user 文本={seen[:240]}")


def _mock_background(params: dict) -> list[dict]:
    return [
        _tool("bash", {"command": "echo background-done > bg.txt",
                       "run_in_background": True}),
        _text("后台任务已启动。"),
        {"type": "request_echo"},
        _text("后台结果是 background-done。"),
    ]


BACKGROUND = lib.Scenario(
    "background",
    "后台任务:bash run_in_background,结果注入下一轮上下文",
    _run_background,
    effort="low",
    defaults={},
    mock_script=_mock_background,
)


# ── remote_tool:客户端注册的远程工具 ───────────────────────────────────────


def _run_remote_tool(cx: lib.Ctx) -> None:
    env = cx.env()
    s = cx.session(env)

    s.op({"type": "register_tools", "tools": [{
        "name": "echo_tool",
        "description": "回显传入的文本(由客户端实现)",
        "parameters": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    }]})

    requests: list[dict] = []
    responder = lib.on_remote_tool("远程工具回显:OK", log=requests)
    turn = s.turn("请调用 echo_tool 工具,把 hello 传过去。", timeout=cx.timeout(),
                  respond=[responder])

    if not requests and "echo_tool" not in turn.tool_names:
        lib.skip_all(cx, ["收到 tool_execution_request", "请求带 call_id / tool / args",
                          "远程工具回执后调用成功"],
                     "模型未按提示调用远程工具 echo_tool(真模型自由发挥)")
        return
    cx.checks.add("收到 tool_execution_request", bool(requests),
                  f"请求数={len(requests)}")
    if requests:
        cx.checks.add("请求带 call_id / tool / args",
                      bool(requests[0].get("call_id")) and bool(requests[0].get("tool")),
                      f"请求={json.dumps(requests[0], ensure_ascii=False)[:200]}")
    failed = [r for r in turn.tool_results() if r["is_error"]]
    cx.checks.add("远程工具回执后调用成功", not failed,
                  f"失败={[(r['call_id'], r['text'][:120]) for r in failed]}")
    cx.checks.add("回合正常收尾", turn.completed,
                  f"终态={turn.status or turn.aborted_reason}")


def _mock_remote_tool(params: dict) -> list[dict]:
    return [
        _tool("echo_tool", {"text": "hello"}),
        _text("远程工具已回显。"),
    ]


REMOTE_TOOL = lib.Scenario(
    "remote_tool",
    "远程工具:RegisterTools → tool_execution_request → 客户端回执",
    _run_remote_tool,
    effort="low",
    defaults={},
    mock_script=_mock_remote_tool,
)


# ── admin_ops:管理面 Op(不开回合)──────────────────────────────────────────


def _run_admin_ops(cx: lib.Ctx) -> None:
    env = cx.env()
    s = cx.session(env)

    expectations = [
        ({"type": "query_subagents"}, "subagent_status", "子代理状态快照"),
        ({"type": "query_guard_stats"}, "guard_stats", "prompt guard 统计"),
        # PromptAction 是 internally tagged(snake_case):{"type":"list"}。
        ({"type": "prompt_command", "action": {"type": "list"}}, "prompt_list", "提示词列表"),
        ({"type": "rescan_extensions", "skills": True, "commands": False},
         "extensions_rescanned", "扩展重扫回执"),
        ({"type": "set_permission_mode", "mode": "prompt"},
         "permission_mode_changed", "权限模式切换"),
        ({"type": "cycle_permission_mode"}, "permission_mode_changed", "权限模式循环"),
    ]
    for op, event_name, label in expectations:
        s.op(op)
        try:
            evt = s.wait(lambda e, n=event_name: lib.etype(e) == n, 20,
                         what=f"{label}({event_name})")
            cx.checks.add(f"{label} 有回执({event_name})", evt is not None,
                          json.dumps(lib._msg(evt), ensure_ascii=False)[:160])
        except lib.WaitTimeout as e:
            cx.checks.add(f"{label} 有回执({event_name})", False, str(e))

    # 权限模式确实切到了 prompt。
    modes = [m for m in s.events if lib.etype(m) == "permission_mode_changed"]
    cx.checks.add("权限模式变更事件非空", bool(modes),
                  f"次数={len(modes)}")
    cx.checks.add("会话仍可继续对话", s.turn("还在。", timeout=cx.timeout()).completed, "")


def _mock_admin_ops(params: dict) -> list[dict]:
    return [_text("好的。")]


ADMIN_OPS = lib.Scenario(
    "admin_ops",
    "管理面 Op:子代理/guard/提示词/扩展重扫/权限模式各自回执",
    _run_admin_ops,
    effort="low",
    defaults={},
    mock_script=_mock_admin_ops,
)
