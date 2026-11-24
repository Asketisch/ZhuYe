#!/usr/bin/env python3
"""cli_e2e_lib.py —— CLI 端到端演练套件的共享库。

被 `cli_e2e_run.py`(单场景执行器)与 `cli_e2e_scenarios/*`(各场景)使用:

- 配置:`load_config` 读 `cli_e2e.toml` 并应用 `ZHUYE_E2E_*` 环境变量覆盖;
- 隔离环境:`make_env` 为单个场景生成独立的假 HOME(ZHUYE_HOME)、引擎
  配置与工作区,rollout / span 日志 / session notes 全落临时目录;
- 会话驱动:`Session` 包装一个 `zhuye serve` 子进程(stdin JSONL 投递
  Submission、stdout JSONL 收 Event),提供「发送 → 等待 → 自动应答」
  的同步原语;
- 检查报告:`Checks` 收集结构化检查项(pass / fail / skip),由套件入口
  逐项折成 tcase;
- mock 自检:`ZHUYE_E2E_MOCK=1` 时用内置 mock provider(`ZHUYE_MODEL=mock`
  + `ZHUYE_MOCK_SCRIPT`)脚本化模型回复,不依赖任何端点即可验证场景的
  协议机制。mock 的 `request_echo` 回复会把引擎实际发给模型的
  tools / system / messages 回显成 JSON,自检据此做字段级断言。

协议要点(与 zhuye-protocol 对齐):
- Submission: `{"id": <客户端生成>, "op": {"type": <snake_case>, ...}}`
- Event:     `{"id": <关联 submission id>, "msg": {"type": ..., ...}}`
- 生命周期事件的 id 为空串;turn 的事件带触发它的 UserInput 的 id
  (plan 审批后自动开始的开发回合沿用被审批那条输入的 id,所以终态
  判定按 turn_id 配对,不按 submission id)。
"""

from __future__ import annotations

import glob
import json
import os
import queue
import subprocess
import threading
import time
import tomllib

# 结构性错误码:出现即视为该回合「被引擎收口」,等待循环提前结束。
FATAL_CODES = frozenset(
    {
        "compact_rapid_refill",
        "compact_rapid_refill_window_full",
        "compact_unavailable_window_full",
        "invalid_submission",
        "prompt_rejected",
    }
)

TURN_TERMINAL = ("turn_complete", "turn_aborted")

# 标量配置:toml 字段 → 环境变量覆盖名(前缀 ZHUYE_E2E_)。
_SCALAR_KEYS = (
    "BASE_URL",
    "API_KEY",
    "MODEL",
    "ENDPOINT",
    "CONTEXT_WINDOW",
    "EFFORT",
    "PHASE_TIMEOUT",
    "EXPECT_ARTIFACT",
    "EXPECT_COMMIT",
)


# ── 配置 ────────────────────────────────────────────────────────────────────


def scalars(cfg: dict) -> dict:
    """把 toml 的端点 / 运行参数 / 任务期望摊平成字符串字典。"""
    ep = cfg.get("endpoint") or {}
    run = cfg.get("run") or {}
    task = cfg.get("task") or {}
    return {
        "BASE_URL": str(ep.get("base_url") or ""),
        "API_KEY": str(ep.get("api_key") or ""),
        "MODEL": str(ep.get("model") or ""),
        "ENDPOINT": str(ep.get("endpoint") or "openai"),
        "CONTEXT_WINDOW": str(ep.get("context_window") or ""),
        "EFFORT": str(run.get("effort") or "xhigh"),
        "PHASE_TIMEOUT": str(run.get("phase_timeout") or 1200),
        "EXPECT_ARTIFACT": str(task.get("expect_artifact") or ""),
        "EXPECT_COMMIT": str(task.get("expect_commit_subject") or ""),
    }


def load_config(toml_path: str, env: dict | None = None) -> dict:
    """读取测试配置并应用字段级环境变量覆盖(环境变量优先于 toml)。"""
    with open(toml_path, "rb") as f:
        cfg = tomllib.load(f)
    env = os.environ if env is None else env
    flat = scalars(cfg)
    for key in _SCALAR_KEYS:
        override = env.get(f"ZHUYE_E2E_{key}")
        if override:
            flat[key] = override
    cfg["_scalars"] = flat
    return cfg


# ── 隔离环境 ────────────────────────────────────────────────────────────────


def engine_config_toml(
    scalars_: dict,
    *,
    window: int | None = None,
    plugins: list[str] | None = None,
    extra_toml: str = "",
    model_windows: dict | None = None,
    max_iterations: int | None = None,
) -> str:
    """生成隔离 HOME 里的引擎配置。

    最小独立面(不继承真实 ~/.zhuye 的插件 / MCP / hooks),演练结果只
    取决于本配置 + 引擎默认。`extra_toml` 追加在末尾(场景专属片段,
    如 `[compact]` / `[token_budget]` / `[cache_warming]`)。
    `model_windows` 在 `[context_windows.entries]` 里追加指定模型
    (spec → token 数)的窗口覆盖 —— 压缩场景给 `mock/mock-1` 设小窗口
    用真实模型路径之外的确定性触发。
    """
    import json as _json

    q = _json.dumps
    model = scalars_.get("MODEL", "")
    window = window if window is not None else (
        int(scalars_["CONTEXT_WINDOW"]) if scalars_.get("CONTEXT_WINDOW", "").isdigit() else None
    )
    plugins = plugins if plugins is not None else []
    lines = [
        "# 由 cli_e2e 套件生成 —— CLI 端到端演练的独立引擎配置。",
        "[active]",
        'group = "e2e"',
        *([f"max_iterations = {int(max_iterations)}"] if max_iterations else []),
        "",
        "[[model_group]]",
        'id = "e2e"',
        f"endpoint = {q(scalars_.get('ENDPOINT') or 'openai')}",
        f"api_key = {q(scalars_.get('API_KEY', ''))}",
        f"base_url = {q(scalars_.get('BASE_URL', ''))}",
        f"model = {q(model)}",
        "responses_api = false",
        "credentials = []",
        "",
    ]
    entries = {}
    if window is not None and model:
        entries[model] = int(window)
    for spec, w in (model_windows or {}).items():
        entries[spec] = int(w)
    if entries:
        lines += ["[context_windows.entries]"]
        lines += [f"{q(spec)} = {w}" for spec, w in sorted(entries.items())]
        lines += [""]
    lines += [
        "[sandbox]",
        "os_level = true",
        "writable_dirs = []",
        "# 任务夹具虽纯本地,但不人为限制 agent 的合法依赖。",
        "allow_network = true",
        "",
        "[plugins]",
        "enabled_plugins = [" + ", ".join(q(p) for p in plugins) + "]",
        "disabled_plugins = []",
        "",
    ]
    if extra_toml:
        lines += [extra_toml.rstrip("\n"), ""]
    return "\n".join(lines)


class Env:
    """单个场景的隔离环境:假 HOME + 引擎配置 + 工作区 + 进程环境变量。"""

    def __init__(self, work: str, home: str, ws: str, proc_env: dict, serve_args: list[str]):
        self.work = work
        self.home = home
        self.ws = ws
        self.zhuye_home = os.path.join(home, ".zhuye")
        self.proc_env = proc_env
        self.serve_args = serve_args

    # 观测面:rollout / span 日志 / model-io trace(都在假 HOME 下)。
    def rollout_files(self, session_id: str) -> list[str]:
        pat = os.path.join(self.zhuye_home, "sessions", "**", f"{session_id}*.jsonl")
        return sorted(glob.glob(pat, recursive=True))

    def rollout_records(self, session_id: str) -> list[dict]:
        out: list[dict] = []
        for path in self.rollout_files(session_id):
            with open(path, encoding="utf-8") as f:
                for line in f:
                    line = line.strip()
                    if line:
                        try:
                            out.append(json.loads(line))
                        except json.JSONDecodeError:
                            pass
        return out

    def rollout_text(self, session_id: str) -> str:
        """rollout 全文(粗粒度检索用:续作注入 / 转向注入等留痕)。"""
        return "\n".join(
            json.dumps(r, ensure_ascii=False) for r in self.rollout_records(session_id)
        )

    def span_events(self, event: str) -> list[dict]:
        """span 日志里指定事件名的全部记录(遥测 goal.turn.verified 等)。"""
        out: list[dict] = []
        for path in sorted(
            glob.glob(os.path.join(self.zhuye_home, "traces", "log", "*.jsonl"))
        ):
            with open(path, encoding="utf-8") as f:
                for line in f:
                    line = line.strip()
                    if not line:
                        continue
                    try:
                        rec = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    if rec.get("event") == event:
                        out.append(rec)
        return out

    def model_io_requests(self, session_id: str) -> list[dict]:
        """model-io trace 的 request 侧记录(断言 wire 上的 effort / cache 标记)。"""
        path = os.path.join(
            self.zhuye_home, "traces", "model-io", f"model-io-sess_{session_id}.jsonl"
        )
        if not os.path.exists(path):
            return []
        out = []
        with open(path, encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if line:
                    try:
                        out.append(json.loads(line))
                    except json.JSONDecodeError:
                        pass
        return out


def make_env(
    work: str,
    cfg: dict,
    *,
    window: int | None = None,
    plugins: list[str] | None = None,
    extra_toml: str = "",
    env_extra: dict | None = None,
    serve_args: list[str] | None = None,
    mock_script: list[dict] | None = None,
    model_windows: dict | None = None,
    max_iterations: int | None = None,
) -> Env:
    """在 `work` 下生成隔离环境并返回。"""
    home = os.path.join(work, "home")
    ws = os.path.join(work, "ws")
    zhuye_home = os.path.join(home, ".zhuye")
    os.makedirs(zhuye_home, exist_ok=True)
    os.makedirs(ws, exist_ok=True)
    scalars_ = cfg["_scalars"]
    with open(os.path.join(zhuye_home, "config.toml"), "w", encoding="utf-8") as f:
        f.write(engine_config_toml(
            scalars_, window=window, plugins=plugins, extra_toml=extra_toml,
            model_windows=model_windows, max_iterations=max_iterations,
        ))
    with open(os.path.join(home, ".gitconfig"), "w", encoding="utf-8") as f:
        f.write(
            "[user]\n"
            "    name = zhuye-e2e\n"
            "    email = zhuye-e2e@example.com\n"
            "[commit]\n"
            "    gpgsign = false\n"
        )
    proc_env = dict(os.environ)
    proc_env["HOME"] = home
    proc_env["ZHUYE_HOME"] = zhuye_home
    # 不让宿主的引擎环境变量渗进演练(与 tests/lib.sh make_isolated_env 同口径)。
    for name in list(proc_env):
        if name.startswith("ZHUYE_") and name not in ("ZHUYE_HOME",):
            del proc_env[name]
    for key in ("OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY", "OLLAMA_API_KEY"):
        proc_env.pop(key, None)
    if env_extra:
        proc_env.update(env_extra)
    if mock_script is not None:
        script_path = os.path.join(work, "mock-script.jsonl")
        with open(script_path, "w", encoding="utf-8") as f:
            for reply in mock_script:
                f.write(json.dumps(reply, ensure_ascii=False) + "\n")
        proc_env["ZHUYE_MODEL"] = "mock"
        proc_env["ZHUYE_MOCK_SCRIPT"] = script_path
    return Env(work, home, ws, proc_env, list(serve_args or []))


# ── 检查报告 ────────────────────────────────────────────────────────────────


class Checks:
    """结构化检查项。fail 任一 → 场景 FAIL;全部 skip → 场景 SKIP。"""

    def __init__(self) -> None:
        self.items: list[dict] = []

    def add(self, name: str, ok: bool, detail: str = "") -> bool:
        self.items.append({"name": name, "status": "pass" if ok else "fail", "detail": detail})
        return ok

    def skip(self, name: str, reason: str) -> None:
        self.items.append({"name": name, "status": "skip", "detail": reason})

    @property
    def status(self) -> str:
        if any(i["status"] == "fail" for i in self.items):
            return "FAIL"
        if self.items and all(i["status"] == "skip" for i in self.items):
            return "SKIP"
        return "PASS"

    def summary(self) -> str:
        counts = {"pass": 0, "fail": 0, "skip": 0}
        for i in self.items:
            counts[i["status"]] += 1
        return f"{counts['pass']} passed, {counts['fail']} failed, {counts['skip']} skipped"


# ── 会话驱动 ────────────────────────────────────────────────────────────────


class WaitTimeout(Exception):
    """等待条件超时。"""


class SessionDied(Exception):
    """serve 进程退出(stdout 关闭)。"""


def _msg(evt: dict) -> dict:
    return evt.get("msg") or {}


def etype(evt: dict) -> str:
    return _msg(evt).get("type") or ""


def tool_output_text(evt: dict) -> str:
    """tool_call_end 事件的输出全文(各内容块拼接)。"""
    out = _msg(evt).get("output") or {}
    parts = []
    for block in out.get("content") or []:
        if isinstance(block, dict) and block.get("type") == "text":
            parts.append(block.get("text") or "")
    return "".join(parts)


class Turn:
    """一个回合的事件切片与派生指标。"""

    def __init__(self, events: list[dict], sub_id: str, terminal: dict | None):
        self.events = events
        self.sub_id = sub_id
        self.terminal = terminal

    @property
    def completed(self) -> bool:
        return self.terminal is not None and etype(self.terminal) == "turn_complete"

    @property
    def status(self) -> str | None:
        if self.completed:
            return _msg(self.terminal).get("status")
        return None

    @property
    def aborted_reason(self) -> dict | None:
        if self.terminal is None or self.completed:
            return None
        return _msg(self.terminal).get("reason")

    @property
    def error_codes(self) -> list[str]:
        return [m.get("code") or "" for m in self._msgs("error")]

    @property
    def text(self) -> str:
        """回合内全部助手文本(含工具调用之间的过程说明)。"""
        return self._text()

    @property
    def final_text(self) -> str:
        """最后一次工具调用之后的助手文本(= 最终答复)。"""
        return self._text(final_only=True)

    def _text(self, final_only: bool = False) -> str:
        buf: list[str] = []
        for evt in self.events:
            m = _msg(evt)
            t = m.get("type")
            if t == "tool_call_end" and final_only:
                buf = []
            elif t == "agent_message_delta":
                buf.append(m.get("delta") or "")
            elif t == "agent_message":
                if "text" in m:
                    buf.append(m.get("text") or "")
                else:
                    for block in m.get("content") or []:
                        if isinstance(block, dict) and block.get("type") == "text":
                            buf.append(block.get("text") or "")
        return "".join(buf)

    @property
    def thinking_chars(self) -> int:
        return sum(
            len(m.get("delta") or "")
            for m in self._msgs("thinking_delta")
        )

    @property
    def tool_names(self) -> list[str]:
        return [m.get("tool_name") or "" for m in self._msgs("tool_call_begin")]

    def tool_results(self) -> list[dict]:
        out = []
        for evt in self.events:
            if etype(evt) == "tool_call_end":
                m = _msg(evt)
                res = _msg(evt).get("output") or {}
                out.append(
                    {
                        "call_id": m.get("call_id"),
                        "is_error": bool(res.get("is_error")),
                        "text": tool_output_text(evt),
                    }
                )
        return out

    @property
    def compactions(self) -> list[dict]:
        """真实发生的压缩事件(策略非 noop)。"""
        return [m for m in self._msgs("context_compacted") if m.get("strategy") != "noop"]

    @property
    def token_counts(self) -> list[dict]:
        """逐次模型调用的用量(排除回合收尾的累计事件 turn_total=true)。"""
        return [
            {k: m.get(k) for k in ("input_tokens", "output_tokens", "cached_tokens", "cache_write_tokens", "total_tokens")}
            for m in self._msgs("token_count")
            if not m.get("turn_total")
        ]

    @property
    def inputs(self) -> list[int]:
        return [tc["input_tokens"] or 0 for tc in self.token_counts]

    def _msgs(self, *types: str) -> list[dict]:
        return [_msg(e) for e in self.events if etype(e) in types]


class Session:
    """一个 `zhuye serve` 子进程的同步驱动。"""

    def __init__(
        self,
        bin: str,
        env: Env,
        *,
        events_path: str | None = None,
        stderr_path: str | None = None,
        resume: str | None = None,
        label: str = "main",
    ):
        self.bin = bin
        self.env = env
        self.events_path = events_path
        self.stderr_path = stderr_path
        self.resume = resume
        self.label = label
        self.proc: subprocess.Popen | None = None
        self.events: list[dict] = []
        self.session_id: str | None = None
        self.model: str | None = None
        self.configured: dict | None = None
        self._q: queue.Queue = queue.Queue()
        self._seq = 0
        self._events_file = None
        self._stderr_file = None

    def __enter__(self) -> "Session":
        return self.start()

    def __exit__(self, *exc) -> None:
        self.close()

    def start(self) -> "Session":
        args = [self.bin, "serve", *self.env.serve_args]
        if self.resume:
            args += ["--resume", self.resume]
        if self.events_path:
            self._events_file = open(self.events_path, "a", encoding="utf-8")
        if self.stderr_path:
            self._stderr_file = open(self.stderr_path, "a", encoding="utf-8")
        self.proc = subprocess.Popen(
            args,
            cwd=self.env.ws,
            env=self.env.proc_env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self._stderr_file or subprocess.DEVNULL,
            text=True,
            bufsize=1,
        )
        threading.Thread(target=self._pump, daemon=True).start()
        evt = self.wait(lambda e: etype(e) == "session_configured", 90)
        self.configured = _msg(evt)
        self.session_id = self.configured.get("session_id")
        self.model = self.configured.get("model")
        return self

    def _pump(self) -> None:
        assert self.proc is not None
        try:
            for line in self.proc.stdout:
                line = line.strip()
                if not line:
                    continue
                try:
                    evt = json.loads(line)
                except json.JSONDecodeError:
                    continue
                self.events.append(evt)
                if self._events_file:
                    self._events_file.write(json.dumps(evt, ensure_ascii=False) + "\n")
                    self._events_file.flush()
                self._q.put(evt)
        finally:
            self._q.put(None)  # EOF

    # ── 发送 ──────────────────────────────────────────────────────────────

    def send(self, sub_id: str, op: dict) -> None:
        assert self.proc and self.proc.stdin
        self.proc.stdin.write(json.dumps({"id": sub_id, "op": op}, ensure_ascii=False) + "\n")
        self.proc.stdin.flush()

    def _new_id(self, prefix: str) -> str:
        self._seq += 1
        return f"e2e-{prefix}-{self._seq}"

    def user_input(self, text: str, sub_id: str | None = None) -> str:
        sub_id = sub_id or self._new_id("input")
        self.send(sub_id, {"type": "user_input", "items": [{"type": "text", "text": text}]})
        return sub_id

    def set_effort(self, effort: str) -> None:
        self.send(self._new_id("effort"), {"type": "set_effort", "effort": effort})

    def op(self, op: dict) -> str:
        """投递任意 Op(带新生成的 submission id)并返回该 id。"""
        sub_id = self._new_id(op.get("type") or "op")
        self.send(sub_id, op)
        return sub_id

    # ── 等待 ──────────────────────────────────────────────────────────────

    def next_event(self, timeout: float) -> dict:
        try:
            evt = self._q.get(timeout=max(timeout, 0.01))
        except queue.Empty as e:
            raise WaitTimeout(f"{timeout:.0f}s 内没有新事件") from e
        if evt is None:
            raise SessionDied("serve 进程退出(stdout 关闭)")
        return evt

    def wait(
        self,
        pred,
        timeout: float,
        respond=None,
        what: str = "条件",
    ) -> dict:
        """等到 `pred(evt)` 为真并返回该事件;`respond` 里的处理器先于
        pred 消化需要即时应答的事件(审批 / 提问 / 远程工具请求)。
        处理器签名 `(session, evt) -> bool`(True = 已消费)。

        未被处理器消费的 `approval_request` 会被**兜底批准**:真模型
        可能顺手调用任何需审批的工具(如 memory 查询),不兜底就把回合
        挂到超时。要观察审批流的场景自带处理器(先于本兜底消费)。
        """
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise WaitTimeout(f"等待「{what}」超时({timeout:.0f}s)")
            try:
                evt = self._q.get(timeout=min(remaining, 1.0))
            except queue.Empty:
                if self.proc is not None and self.proc.poll() is not None and self._q.empty():
                    raise SessionDied("serve 进程退出(stdout 关闭)")
                continue
            if evt is None:
                raise SessionDied("serve 进程退出(stdout 关闭)")
            consumed = False
            if respond:
                for handler in respond:
                    if handler(self, evt):
                        consumed = True
                        break
            if not consumed and etype(evt) == "approval_request":
                self._auto_approve(evt)
            if pred(evt):
                return evt

    def _auto_approve(self, evt: dict) -> None:
        """兜底批准:真模型调了任何需审批的工具都不至于挂住回合。"""
        m = _msg(evt)
        request_id = m.get("request_id")
        if not request_id:
            return
        kind = m.get("kind") or {}
        op_type = "hook_approval" if kind.get("type") == "hook" else "tool_approval"
        self.send(self._new_id("auto-approval"), {
            "type": op_type, "id": request_id, "decision": "approve",
        })

    def wait_quiet(self, quiet: float = 3.0, max_wait: float = 600.0) -> list[dict]:
        """等到事件流静默 `quiet` 秒(用于没有明确终态的异步过程:
        插件驱动的自动续作 / 保温任务等),返回期间收到的事件。"""
        collected: list[dict] = []
        deadline = time.monotonic() + max_wait
        while time.monotonic() < deadline:
            try:
                evt = self._q.get(timeout=quiet)
            except queue.Empty:
                return collected
            if evt is None:
                raise SessionDied("serve 进程退出(stdout 关闭)")
            collected.append(evt)
        raise WaitTimeout(f"{max_wait:.0f}s 内事件流未静默")

    def turn(
        self,
        text: str | None = None,
        *,
        items: list[dict] | None = None,
        timeout: float | None = None,
        respond=None,
        sub_id: str | None = None,
        expect_settled: bool = True,
    ) -> Turn:
        """投递一条 UserInput 并等回合终态(turn_complete / turn_aborted)。

        按顺序等待:turn_started(取 turn_id)→ 同 turn_id 的终态事件,
        不按 submission id 配对(见模块文档)。`expect_settled` 时再等
        submission_closed —— 回合后处理(goal 自校验等)在 turn_complete
        之后才跑,它才是「可以安全发下一条输入」的信号。
        """
        timeout = self._timeout(timeout)
        sub_id = sub_id or self._new_id("turn")
        start = len(self.events)
        payload_items = items if items is not None else [{"type": "text", "text": text or ""}]
        self.send(sub_id, {"type": "user_input", "items": payload_items})
        started = self.wait(
            lambda e: etype(e) == "turn_started", timeout, respond, what="turn_started"
        )
        turn_id = _msg(started).get("turn_id")
        terminal = self.wait(
            lambda e: etype(e) in TURN_TERMINAL and _msg(e).get("turn_id") == turn_id,
            timeout,
            respond,
            what="回合终态",
        )
        if expect_settled:
            self.wait(
                lambda e: etype(e) == "submission_closed" and e.get("id") == sub_id,
                timeout,
                respond,
                what="submission_closed",
            )
        end = self.events.index(terminal) + 1
        return Turn(self.events[start:end], sub_id, terminal)

    def wait_turn_end(self, timeout: float | None = None, respond=None, since: int | None = None) -> Turn:
        """等待一个不是由本客户端 UserInput 触发的回合结束(插件自动
        续作等),返回从 `since`(默认当前)起的回合切片。"""
        timeout = self._timeout(timeout)
        start = len(self.events) if since is None else since
        terminal = self.wait(
            lambda e: etype(e) in TURN_TERMINAL, timeout, respond, what="回合终态"
        )
        end = self.events.index(terminal) + 1
        return Turn(self.events[start:end], terminal.get("id") or "", terminal)

    def _timeout(self, timeout: float | None) -> float:
        if timeout is not None:
            return float(timeout)
        return float(self.env.proc_env.get("ZHUYE_E2E_PHASE_TIMEOUT", "1200"))

    # ── 收尾 ──────────────────────────────────────────────────────────────

    def shutdown(self, timeout: float = 20.0) -> dict | None:
        """优雅关闭:Op::Shutdown → shutdown_complete。"""
        if not self.proc:
            return None
        try:
            self.send(self._new_id("shutdown"), {"type": "shutdown"})
            return self.wait(lambda e: etype(e) == "shutdown_complete", timeout, what="shutdown_complete")
        except (WaitTimeout, SessionDied, BrokenPipeError, OSError):
            return None
        finally:
            self.close()

    def close(self) -> None:
        proc = self.proc
        if not proc:
            return
        try:
            if proc.stdin:
                proc.stdin.close()
        except Exception:
            pass
        try:
            proc.wait(timeout=5)
        except Exception:
            proc.kill()
            try:
                proc.wait(timeout=5)
            except Exception:
                pass
        for handle in (self._events_file, self._stderr_file):
            if handle:
                handle.close()
        self._events_file = self._stderr_file = None
        self.proc = None


# ── 场景与执行上下文 ────────────────────────────────────────────────────────


class Scenario:
    """一个端到端场景的注册信息。

    - `run(cx)`:场景主体,经 `cx.session()` 起会话、发 Op、收事件、
      `cx.checks.add/skip` 登记检查项;
    - `needs_model=True`:本质依赖真实模型(缓存分段上报 / 可中断的
      长输出等),mock 自检模式下整场景 SKIP;
    - `mock_only=True`:镜像约束 —— 依赖 mock 专属的确定性构造(给
      `mock/mock-1` 配小窗口等),真实模型模式下整场景 SKIP;
    - `effort`:默认思考强度(toml 的 `[scenarios.<name>] effort` 可覆盖);
    - `defaults`:可被 toml `[scenarios.<name>]` 覆盖的参数默认值;
    - `mock_script(params)`:mock 自检用的脚本化模型回复(每次模型调用
      一行);`needs_model=False` 的场景必须提供,否则自检跳过该场景。
    """

    def __init__(
        self,
        name: str,
        title: str,
        run,
        *,
        needs_model: bool = False,
        mock_only: bool = False,
        effort: str | None = None,
        defaults: dict | None = None,
        mock_script=None,
    ):
        self.name = name
        self.title = title
        self.run = run
        self.needs_model = needs_model
        self.mock_only = mock_only
        self.effort = effort
        self.defaults = defaults or {}
        self.mock_script = mock_script


class Ctx:
    """单个场景的执行上下文:参数、检查表、会话生命周期、观测面。"""

    def __init__(self, scenario: Scenario, cfg: dict, params: dict, work: str, bin: str, mock: bool):
        self.scenario = scenario
        self.cfg = cfg
        self.params = params
        self.work = work
        self.bin = bin
        self.mock = mock
        self.checks = Checks()
        self.sessions: list[Session] = []
        self.metrics: dict = {}
        os.makedirs(work, exist_ok=True)

    def p(self, key: str, default=None):
        return self.params.get(key, default)

    def timeout(self, default: float = 600.0) -> float:
        """阶段超时:场景参数 `phase_timeout` > 全局 run.phase_timeout。"""
        value = self.params.get("phase_timeout")
        if value:
            return float(value)
        return float(self.cfg["_scalars"].get("PHASE_TIMEOUT") or default)

    def effort(self) -> str:
        return str(self.params.get("effort") or self.scenario.effort or self.cfg["_scalars"]["EFFORT"])

    def env(self, *, extra_toml: str = "", env_extra: dict | None = None,
            serve_args: list[str] | None = None, window: int | None = None,
            plugins: list[str] | None = None, setup=None,
            model_windows: dict | None = None,
            max_iterations: int | None = None) -> Env:
        """生成本场景的隔离环境(`setup(ws)` 可布置工作区夹具)。"""
        script = None
        if self.mock:
            script = self.scenario.mock_script(self.params) if self.scenario.mock_script else []
        env = make_env(
            self.work,
            self.cfg,
            window=window,
            plugins=plugins,
            extra_toml=extra_toml,
            env_extra=env_extra,
            serve_args=serve_args,
            mock_script=script,
            model_windows=model_windows,
            max_iterations=max_iterations,
        )
        if setup:
            setup(env.ws)
        return env

    def session(self, env: Env, *, resume: str | None = None, label: str = "main") -> Session:
        path = os.path.join(self.work, f"events-{label}.jsonl")
        err = os.path.join(self.work, f"serve-{label}.stderr.log")
        s = Session(self.bin, env, events_path=path, stderr_path=err, resume=resume, label=label)
        self.sessions.append(s)
        s.start()
        s.set_effort(self.effort())
        return s

    def close_all(self) -> None:
        for s in self.sessions:
            try:
                s.close()
            except Exception:
                pass
        self.sessions = []

    def wait_for_span(self, env: Env, event: str, at_least: int, timeout: float = 120.0) -> list[dict]:
        """等 span 日志里 `event` 的记录数达到 `at_least`。

        turn 之后的异步处理(goal 自校验)没有 wire 事件,它的遥测记录
        才是「处理完成、可以安全发下一条输入」的信号 —— 续作文本入队
        发生在该记录之前。
        """
        deadline = time.monotonic() + timeout
        while True:
            records = env.span_events(event)
            if len(records) >= at_least:
                return records
            if time.monotonic() > deadline:
                raise WaitTimeout(f"等待 span 日志 {event} × {at_least} 超时({timeout:.0f}s)")
            time.sleep(0.2)

    def write_file(self, ws: str, rel: str, content: str) -> str:
        path = os.path.join(ws, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as f:
            f.write(content)
        return path

    def read_file(self, ws: str, rel: str) -> str | None:
        path = os.path.join(ws, rel)
        if not os.path.exists(path):
            return None
        with open(path, encoding="utf-8", errors="replace") as f:
            return f.read()

    def exists(self, ws: str, rel: str) -> bool:
        return os.path.exists(os.path.join(ws, rel))


# ── 常用自动应答器 ──────────────────────────────────────────────────────────

# 处理器签名统一为 `(session, evt) -> bool`(True = 已消费,交给等待
# 条件之前先消化);`log` 列表记录被应答的事件原文,供场景做断言。


def _text_output(text: str, is_error: bool = False) -> dict:
    return {
        "content": [{"type": "text", "text": text}],
        "is_error": is_error,
        "metadata": {},
        "elapsed_ms": 0,
    }


def on_approval(decision: str = "approve", reason: str = "e2e 拒绝", log: list | None = None):
    """approval_request → ToolApproval。

    `ReviewDecision` 是 externally tagged 的 snake_case 枚举:无载荷变体
    直接是字符串("approve" / "approve_for_session"),带载荷变体是对象
    ({"deny":{"reason":...}} / {"reject_for_session":{"reason":...}})。
    """
    seen: set[str] = set()

    def handler(s: Session, evt: dict) -> bool:
        if etype(evt) != "approval_request":
            return False
        m = _msg(evt)
        request_id = m.get("request_id")
        if not request_id or request_id in seen:
            return True
        seen.add(request_id)
        if decision in ("approve", "approve_for_session"):
            op_decision: object = decision
        elif decision == "reject_for_session":
            op_decision = {"reject_for_session": {"reason": reason}}
        else:
            op_decision = {"deny": {"reason": reason}}
        kind = m.get("kind") or {}
        op_type = "hook_approval" if kind.get("type") == "hook" else "tool_approval"
        s.send(s._new_id("approval"), {"type": op_type, "id": request_id, "decision": op_decision})
        if log is not None:
            log.append(m)
        return True

    return handler


def on_plan_ready(choice: str = "auto_mode", log: list | None = None):
    """plan_ready → PlanApproval。choice = auto_mode / manual_approve / revise。"""

    def handler(s: Session, evt: dict) -> bool:
        if etype(evt) != "plan_ready":
            return False
        m = _msg(evt)
        s.send(s._new_id("plan"), {"type": "plan_approval", "id": m.get("plan_id"), "choice": choice})
        if log is not None:
            log.append(m)
        return True

    return handler


def on_question(answer, log: list | None = None):
    """ask_user_question → AskUserQuestionResponse。

    `answer(event) -> list[dict]`:每个问题一个 Answer 槽,如
    `{"selected": [0]}` 或 `{"custom": "自由文本"}`。
    """

    def handler(s: Session, evt: dict) -> bool:
        if etype(evt) != "ask_user_question":
            return False
        m = _msg(evt)
        s.send(
            s._new_id("answer"),
            {"type": "ask_user_question_response", "id": m.get("request_id"), "answers": {"answers": answer(m)}},
        )
        if log is not None:
            log.append(m)
        return True

    return handler


def on_input(text: str, log: list | None = None):
    """ask_user_input(自由文本提问)→ AskUserInputResponse。"""

    def handler(s: Session, evt: dict) -> bool:
        if etype(evt) != "ask_user_input":
            return False
        m = _msg(evt)
        s.send(s._new_id("input"), {"type": "ask_user_input_response", "id": m.get("request_id"), "text": text})
        if log is not None:
            log.append(m)
        return True

    return handler


def on_remote_tool(reply, log: list | None = None):
    """tool_execution_request → ToolExecutionResponse。

    `reply(event) -> dict`:返回 ToolOutput(content / is_error / metadata / elapsed_ms);
    也可直接传字符串(按纯文本成功输出)。
    """

    def handler(s: Session, evt: dict) -> bool:
        if etype(evt) != "tool_execution_request":
            return False
        m = _msg(evt)
        output = _text_output(reply) if isinstance(reply, str) else reply(m)
        s.send(s._new_id("remote"), {"type": "tool_execution_response", "call_id": m.get("call_id"), "output": output})
        if log is not None:
            log.append(m)
        return True

    return handler


def skip_all(cx, names: list[str], reason: str) -> None:
    """把一组检查项标为 skip。

    真模型自由发挥:未必按提示词调用指定工具(如该用
    `bash(run_in_background)` 却去查 `background_status`)。这种
    「模型没按预期走」判**跳过**而非失败 —— 协议面本身没问题,
    缺的是模型配合(确定性自检由 mock 模式覆盖)。
    """
    for name in names:
        cx.checks.skip(name, reason)


def last_json(text: str) -> dict | None:
    """取文本里最后一个完整 JSON 对象(mock request_echo 回复解析用)。"""
    end = text.rfind("}")
    if end < 0:
        return None
    depth = 0
    for i in range(end, -1, -1):
        if text[i] == "}":
            depth += 1
        elif text[i] == "{":
            depth -= 1
            if depth == 0:
                try:
                    return json.loads(text[i : end + 1])
                except json.JSONDecodeError:
                    return None
    return None


def echo_user_texts(text: str) -> list[str]:
    """从 request_echo 回复里取出模型看到的全部 user 消息文本。"""
    data = last_json(text)
    if not data:
        return []
    out = []
    for m in data.get("messages") or []:
        if m.get("role") == "user":
            out.append(m.get("text") or "")
    return out


def echo_tools(text: str) -> list[str]:
    data = last_json(text)
    return list((data or {}).get("tools") or [])


def echo_text(turn) -> str:
    """回合内全部助手文本(含 request_echo 回显、跨多次模型调用拼接)。

    与 `Turn.text` 同源,单独抽成函数是因为 `Turn` 只在 `turn()` 返回值
    里有,场景常需要「把某个 turn 的文本拿去解析回显」。
    """
    return turn.text


def echo_messages(text: str) -> list[dict]:
    """request_echo 回复里的完整 messages(role/text 归一化)。"""
    data = last_json(text)
    out = []
    for m in (data or {}).get("messages") or []:
        content = m.get("content")
        if isinstance(content, list):
            text_parts = "".join(
                b.get("text") or "" for b in content if isinstance(b, dict) and b.get("type") == "text"
            )
        else:
            text_parts = str(content or "")
        out.append({
            "role": m.get("role") or "",
            "text": m.get("text") if isinstance(m.get("text"), str) else text_parts,
        })
    return out


# —— 场景夹具 ——————————————————————————————————————————————

PASSCODE = "REMOTE-4420"
