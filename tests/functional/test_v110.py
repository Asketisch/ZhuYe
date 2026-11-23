"""能力覆盖层 · v1.10 五项功能(字段级断言)。

覆盖:
- 功能2:goal + 队列模式(goal-queue 插件门控)
- 功能3:模型上下文最大长度配置([model].context_window / [context_windows])
- 功能4:思考强度配置(model.reasoning_effort / set_effort max|ultra)
- 功能5:空 API key(anthropic 兼容端点免 key 注册)

依据:AGENTS.md 开发流程检查表 —— 能力断言到字段级,不接受"退出码 0"。
"""

import json
import shutil
import subprocess
from pathlib import Path

import pytest

from wire import (
    DRAIN_TIMEOUT,
    ServeSession,
    Sandbox,
    ZHUYE_BIN,
    first_event,
    msg_types,
    require_binary,
    rollout_thread_id,
    run_cli,
)

pytestmark = pytest.mark.usefixtures("sandbox")

REPO_ROOT = Path(ZHUYE_BIN).resolve().parents[2]


def _config_toml(sandbox: Sandbox) -> Path:
    return sandbox.home / ".zhuye" / "config.toml"


def _read_config(sandbox: Sandbox) -> dict:
    import tomllib

    return tomllib.loads(_config_toml(sandbox).read_text())


# ── 功能4:思考强度配置 ─────────────────────────────────────────────────


def test_V1_config_set_model_reasoning_effort_字段级(sandbox) -> None:
    """`config set model.reasoning_effort ultra` 落盘;非法值拒绝;unset 清空。"""
    require_binary()
    ok = run_cli(sandbox, "config", "set", "model.reasoning_effort", "ultra", timeout=30)
    assert ok.returncode == 0, f"set 失败:{ok.stdout + ok.stderr}"
    cfg = _read_config(sandbox)
    assert cfg["model"]["reasoning_effort"] == "ultra", f"落盘字段:{cfg.get('model')}"

    # 大小写归一:Max → max。
    run_cli(sandbox, "config", "set", "model.reasoning_effort", "Max", timeout=30)
    assert _read_config(sandbox)["model"]["reasoning_effort"] == "max"

    # 新档位可写入并保持规范化值。
    xhigh = run_cli(sandbox, "config", "set", "model.reasoning_effort", "XHigh", timeout=30)
    assert xhigh.returncode == 0
    assert _read_config(sandbox)["model"]["reasoning_effort"] == "xhigh"

    # 非法档位:非零退出 + 提示合法值清单。
    bad = run_cli(sandbox, "config", "set", "model.reasoning_effort", "extreme", timeout=30)
    assert bad.returncode != 0, f"非法档位应拒绝:{bad.stdout}"
    assert "low|medium|high|xhigh|max|ultra" in bad.stdout + bad.stderr

    # ls 可见;unset 清空。
    ls = run_cli(sandbox, "config", "ls", timeout=30)
    assert "model.reasoning_effort" in ls.stdout
    un = run_cli(sandbox, "config", "unset", "model.reasoning_effort", timeout=30)
    assert un.returncode == 0
    assert "reasoning_effort" not in _read_config(sandbox).get("model", {})


def test_V2_set_effort_max_ultra_op_不崩且turn收尾(serve_session) -> None:
    """serve 提交 set_effort max / ultra(新档位)→ 无 error 事件,后续 turn 正常。"""
    require_binary()
    with serve_session(env={"ZHUYE_MODEL": "mock"}) as s:
        for effort in ("max", "ultra"):
            s.send({"type": "set_effort", "effort": effort})
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "hi"}]},
            timeout=DRAIN_TIMEOUT,
        )
    types = msg_types(evs)
    assert "turn_complete" in types, f"turn 未收尾:{types}"
    errors = [e for e in evs if e.get("msg", {}).get("type") == "error"]
    assert not errors, f"新档位不应产生 error 事件:{errors}"


# ── 功能3:模型上下文最大长度 ───────────────────────────────────────────


def test_V3_model_context_window_进session_configured(sandbox) -> None:
    """`config set model.context_window 307200` → exec 报告 context_window_size=307200。"""
    require_binary()
    ok = run_cli(sandbox, "config", "set", "model.context_window", "307200", timeout=30)
    assert ok.returncode == 0, ok.stderr
    assert _read_config(sandbox)["model"]["context_window"] == 307200

    proc = run_cli(sandbox, "exec", "hi", env={"ZHUYE_MODEL": "mock"}, timeout=60)
    assert proc.returncode == 0, proc.stderr[:400]
    events = [json.loads(ln) for ln in proc.stdout.splitlines() if ln.strip()]
    sc = first_event(events, "session_configured")
    assert sc is not None, f"缺 session_configured:{msg_types(events)}"
    assert sc["msg"]["context_window_size"] == 307200, (
        f"配置的窗口应进事件:{sc['msg'].get('context_window_size')}"
    )


def test_V4_context_windows表_归一化匹配且优先于model段(sandbox) -> None:
    """`[context_windows] "Mock" = 204800`(大小写形态)对模型 mock 生效,
    且 per-model 条目优先于 `[model].context_window`。"""
    require_binary()
    sandbox.write_config(
        "\n[context_windows]\nMock = 204800\n\n[model]\ncontext_window = 999000\n"
    )
    proc = run_cli(sandbox, "exec", "hi", env={"ZHUYE_MODEL": "mock"}, timeout=60)
    assert proc.returncode == 0, proc.stderr[:400]
    events = [json.loads(ln) for ln in proc.stdout.splitlines() if ln.strip()]
    sc = first_event(events, "session_configured")
    assert sc is not None, f"缺 session_configured:{msg_types(events)}"
    assert sc["msg"]["context_window_size"] == 204800, (
        f"per-model 条目(归一化匹配)应赢过 [model] 段:{sc['msg']}"
    )


# ── 功能5:空 API key ──────────────────────────────────────────────────


def test_V5_anthropic空key加base_url_可推出active_provider(sandbox) -> None:
    """`[[model_group]]` 空 key + base_url(免 key 本地网关)→ doctor 识别
    active group 为 anthropic;去掉 base_url 则视为未配置(配置模型 v2
    口径:组级凭据 = key / base_url / credentials 任一非空)。"""
    require_binary()
    base = "# 测试用最小配置:关闭全部内置 hook\n[hooks]\nenabled = []\n"
    # env 隔离:本用例验证 TOML 配置行为,进程 env 的 ZHUYE_MODEL(runner
    # 门禁恒设 mock,mock 优先级高于 TOML)必须显式置空,否则 doctor 的
    # provider 解析恒为 mock,下面两个断言必假失败。
    no_model_env = {"ZHUYE_MODEL": ""}
    # 对照组先行:无 base_url 的空 key → 组无可用凭据 → 未配置。
    _config_toml(sandbox).write_text(
        base + '\n[[model_group]]\nid = "anthropic"\nendpoint = "anthropic"\napi_key = ""\n'
    )
    bare = run_cli(sandbox, "doctor", "--check-network", env=no_model_env, timeout=60)
    assert "no [active].group set" in bare.stdout, f"空 key 无 base_url 应未配置:{bare.stdout[:300]}"

    # 主断言:显式 base_url + 空 key → anthropic 注册并被推出。
    _config_toml(sandbox).write_text(
        base + '\n[[model_group]]\nid = "anthropic"\nendpoint = "anthropic"\napi_key = ""\n'
               'base_url = "http://127.0.0.1:9/v1"\n'
    )
    out = run_cli(sandbox, "doctor", "--check-network", env=no_model_env, timeout=60)
    assert "checking model group 'anthropic'" in out.stdout, (
        f"免 key anthropic 兼容端点应注册并成为 active group:{out.stdout[:400]}"
    )


# ── 功能2:goal + 队列模式(goal-queue 插件) ───────────────────────────


def _install_goal_queue_plugin(sandbox) -> None:
    """安装并启用 goal-queue 插件(从仓库路径;本地路径安装的 id 后缀为 @inline)。"""
    src = REPO_ROOT / "plugins" / "goal-queue"
    assert src.is_dir(), f"插件目录缺失:{src}"
    install = run_cli(sandbox, "plugin", "install", str(src), timeout=60)
    assert install.returncode == 0, f"plugin install 失败:{install.stdout + install.stderr}"
    enable = run_cli(sandbox, "plugin", "enable", "goal-queue@inline", timeout=30)
    assert enable.returncode == 0, f"plugin enable 失败:{enable.stdout + enable.stderr}"
    cfg = _read_config(sandbox)
    assert "goal-queue@inline" in cfg["plugins"]["enabled_plugins"], f"启用态未落盘:{cfg.get('plugins')}"


def test_V6_未启用插件时queue_goal工具不存在(serve_session, mock_script) -> None:
    """对照组:未启用 goal-queue 插件 → 调 queue / goal 工具报 tool not found。"""
    require_binary()
    script = mock_script([
        '{"type":"tool_call","name":"queue","args":{"action":"status"}}',
        '{"type":"text","text":"done"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "查队列"}]},
            timeout=DRAIN_TIMEOUT,
        )
    ends = [e["msg"] for e in evs if e["msg"]["type"] == "tool_call_end"]
    assert ends and ends[0]["is_error"] is True, f"未启用插件应 tool not found:{ends}"
    assert "tool not found" in json.dumps(ends[0], ensure_ascii=False)


def test_V7_启用插件后queue工具_入队落盘并串行消费(serve_session, mock_script, sandbox) -> None:
    """启用 goal-queue → mock 调 queue add + start → 任务落
    `~/.zhuye/queue/<thread_id>.json`;driver 出队注入新 turn,turn 完成后
    任务状态 done(事件驱动串行语义,字段级)。"""
    require_binary()
    _install_goal_queue_plugin(sandbox)

    script = mock_script([
        '{"type":"tool_call","name":"queue","args":{"action":"add","prompt":"第二批任务","name":"批2"}}',
        '{"type":"tool_call","name":"queue","args":{"action":"start"}}',
        '{"type":"text","text":"queued"}',
        '{"type":"text","text":"第二任务执行完毕"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "排队并开跑"}]},
            timeout=DRAIN_TIMEOUT,
        )
        # add / start 两次工具调用都成功,输出含任务 id 与开关状态。
        ends = [e["msg"] for e in evs if e["msg"]["type"] == "tool_call_end" and not e["msg"].get("is_error")]
        assert len(ends) >= 2, f"add/start 应成功:{[e['msg']['type'] for e in evs]}"

        def _text(msg: dict) -> str:
            # wire 形态:tool_call_end.output.content(嵌套 ToolOutput)。
            return "".join(
                b.get("text", "") for b in msg.get("output", {}).get("content", []) if isinstance(b, dict)
            )

        add_text = _text(ends[0])
        assert "任务已入队" in add_text, f"add 输出:{add_text}"
        import re

        m = re.search(r"任务已入队:(\S+)", add_text)
        assert m, f"add 输出应含任务 id:{add_text}"
        task_id = m.group(1)
        start_text = _text(ends[1])
        assert "队列模式已开启" in start_text, f"start 输出:{start_text}"
        assert "turn_complete" in msg_types(evs), f"turn 未收尾:{msg_types(evs)}"

        thread_id = rollout_thread_id(sandbox.home)
        store = sandbox.home / ".zhuye" / "queue" / f"{thread_id}.json"
        assert store.exists(), f"queue store 未落盘:{store}"

        # driver 出队(事件驱动 + 2s tick 兜底)→ 注入新 turn → 完成后置 done。
        import time

        def _status() -> str:
            tasks = json.loads(store.read_text())["tasks"]
            return tasks[0]["status"] if tasks else ""

        deadline = time.monotonic() + 20.0
        while time.monotonic() < deadline and _status() != "done":
            time.sleep(0.3)
        assert _status() == "done", f"任务应串行消费到 done:{store.read_text()}"
        tasks = json.loads(store.read_text())["tasks"]
        assert tasks[0]["id"] == task_id, f"落盘任务 id 应与工具输出一致:{tasks[0]['id']} vs {task_id}"
        assert tasks[0]["prompt"] == "第二批任务"
        assert tasks[0]["name"] == "批2"
        assert tasks[0]["started_at"] is not None, f"started_at 应记录:{tasks[0]}"
        assert tasks[0]["finished_at"] is not None, f"finished_at 应记录:{tasks[0]}"


def test_V8_goal工具_状态与提交Op(serve_session, mock_script, sandbox) -> None:
    """启用 goal-queue → `goal status` 报未激活;`goal enter` 提交
    Op(输出 enter_submitted,引擎进入 goal 模式)。"""
    require_binary()
    _install_goal_queue_plugin(sandbox)

    script = mock_script([
        '{"type":"tool_call","name":"goal","args":{"action":"status"}}',
        '{"type":"tool_call","name":"goal","args":{"action":"enter","goal":"让测试全绿","token_budget":12345}}',
        '{"type":"text","text":"goal set"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "设定目标"}]},
            timeout=DRAIN_TIMEOUT,
        )
        ends = [e["msg"] for e in evs if e["msg"]["type"] == "tool_call_end" and not e["msg"].get("is_error")]
        assert len(ends) >= 2, f"status/enter 应成功:{[e['msg']['type'] for e in evs]}"

        def _text(msg: dict) -> str:
            # wire 形态:tool_call_end.output.content(嵌套 ToolOutput)。
            return "".join(
                b.get("text", "") for b in msg.get("output", {}).get("content", []) if isinstance(b, dict)
            )

        assert "目标模式未激活" in _text(ends[0]), f"status 输出:{_text(ends[0])}"
        enter_text = _text(ends[1])
        assert "目标模式已提交" in enter_text, f"enter 输出:{enter_text}"
        assert "让测试全绿" in enter_text, f"enter 应回显目标:{enter_text}"
        # enter 提交的 Op 走 submission 通道:mock client 在位 → 引擎构造
        # controller,无 GOAL_NO_CLIENT error。
        errors = [e for e in evs if e["msg"]["type"] == "error"]
        assert not errors, f"不应有 error 事件:{errors}"


# ── 功能1:一键导入其他本地 agent 配置 ─────────────────────────────────


def test_V11_rescan_extensions_skills重扫回执字段级(serve_session, mock_script) -> None:
    """`Op::RescanExtensions` → `extensions_rescanned`(字段级)。

    依据:op.rs RescanExtensions 变体(additive);submission_loop 的
    RescanExtensions 分支 —— skills=true 时按 M4Deps.skill_dirs(与
    bootstrap 同源)重扫,回执 skills 总数;commands 位当前索引重扫
    未接线,恒回 `false`。事件恰一条、不开 turn。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        # 1) skills 重扫:目录可为空,回执必为非负 int;commands 恒 false。
        evs = s.submit({"type": "rescan_extensions", "skills": True, "commands": False})
        m = first_event(evs, "extensions_rescanned")
        assert m is not None, f"未见 extensions_rescanned:{msg_types(evs)}"
        # 事件经 per-submission + session 扇出双路送达(prompt_list 同款),
        # 客户端按 sub_id 去重 —— 这里断言字段级而非唯一性。
        assert all(
            isinstance(e["msg"].get("skills"), int) for e in evs
            if e["msg"].get("type") == "extensions_rescanned"
        ), f"skills 应为 int:{msg_types(evs)}"
        assert m["msg"]["skills"] >= 0
        assert m["msg"]["commands"] is False, f"commands 当前恒回 false:{m['msg']}"

        # 2) 未请求任何重扫 → 照常应答(skills=0)。
        evs = s.submit({"type": "rescan_extensions", "skills": False, "commands": False})
        m = first_event(evs, "extensions_rescanned")
        assert m is not None, f"未见 extensions_rescanned:{msg_types(evs)}"
        assert m["msg"]["skills"] == 0, f"未请求 skills 重扫应回 0:{m['msg']}"


def test_V12_wasm插件_工具与hook字段级(sandbox, mock_script) -> None:
    """WASM 插件(多语言生态):Rust 组件的 tool 注册与 hook 注册。

    依据:zhuye-wasm(WasmTool/WasmHook)+ manifest [[wasm_tools]] +
    hooks.json wasm 条目 + loader wasm 分支。样例产物随插件入库
    (重建用 scripts/build_plugin_demo_wasm.sh)。断言字段级:demo_echo
    的 tool_call_end 回显输入文本(is_error=false,覆盖注册 → 模型可见
    → invoke → 输出全链路);stderr 出现 `wasm hook registered` /
    `wasm tool registered`(hooks.json 的 `wasm` 字段曾在解析层被丢弃,
    wasm hook 从未注册过 —— 本行断言即防回归)。
    """
    require_binary()
    wasm_dir = REPO_ROOT / "plugins" / "plugin-demo" / "bin"
    if not (wasm_dir / "zhuye-demo-tool.wasm").exists() or not (
        wasm_dir / "zhuye-demo-hook.wasm"
    ).exists():
        pytest.skip("WASM 样例产物缺失(先跑 scripts/build_plugin_demo_wasm.sh)")

    # demo 插件装进 sandbox 的 ZHUYE_HOME(直装;state 文件必须是 V2
    # schema —— 此前 fixture 写的是 `{"installed":[...]}` 旧形态,
    # PluginManager::load 反序列化失败 → plugin runtime 静默 skip,
    # 「回合无 error」的弱断言从未暴露这一点)。
    home = sandbox.home / ".zhuye" / "plugins" / "cache" / "inline" / "demo" / "0.1.0"
    shutil.copytree(REPO_ROOT / "plugins" / "plugin-demo", home, dirs_exist_ok=True)
    state = sandbox.home / ".zhuye" / "plugins" / "installed_plugins.json"
    state.parent.mkdir(parents=True, exist_ok=True)
    now = "2026-01-01T00:00:00Z"
    state.write_text(json.dumps({
        "version": 2,
        "plugins": {
            "demo@inline": [{
                "scope": "user",
                "install_path": str(home),
                "version": "0.1.0",
                "installed_at": now,
                "last_updated": now,
            }]
        },
    }))
    config = sandbox.home / ".zhuye" / "config.toml"
    config.parent.mkdir(parents=True, exist_ok=True)
    config.write_text('[plugins]\nenabled_plugins = ["demo@inline"]\n')

    # 1) 工具链字段级:mock 调 demo_echo(WASM 组件注册的工具)→
    #    tool_call_end 回显输入文本、is_error=false。工具未注册会得到
    #    tool not found,故本断言覆盖「注册 → 模型可见 → invoke → 输出」
    #    全链路;PostToolUse wasm hook(matcher "*")随工具结束派发,
    #    不阻断回合。
    script = mock_script([
        '{"type":"tool_call","name":"demo_echo","args":{"text":"V12-wasm-echo"}}',
        '{"type":"text","text":"V12 done"}',
    ])
    with ServeSession(sandbox, env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script),
                                    "RUST_LOG": "info"}) as s:
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "run wasm"}]},
            timeout=DRAIN_TIMEOUT,
        )
        ends = [e["msg"] for e in evs if e["msg"]["type"] == "tool_call_end"]
        # tool_call_end 只带 call_id,工具名在 tool_call_begin —— 按
        # call_id 关联出 demo_echo 的结束事件。
        name_by_call: dict[str, str] = {
            m["call_id"]: m["tool_name"]
            for m in (e["msg"] for e in evs if e["msg"]["type"] == "tool_call_begin")
        }
        echo_end = next(
            (m for m in ends if name_by_call.get(m.get("call_id")) == "demo_echo"), None
        )
        assert echo_end is not None, (
            f"未见 demo_echo 工具结束事件:{msg_types(evs)} tools={sorted(name_by_call.values())}"
        )
        assert not echo_end.get("is_error"), f"demo_echo 不应失败:{echo_end}"
        echo_text = "".join(
            b.get("text", "") for b in echo_end.get("output", {}).get("content", []) if isinstance(b, dict)
        )
        assert "V12-wasm-echo" in echo_text, f"echo 回显失败:{echo_text}"
        assert first_event(evs, "turn_complete") is not None, msg_types(evs)
        assert not [t for t in msg_types(evs) if t == "error"], msg_types(evs)

    # 2) 注册链路字段级:loader 的 wasm 分支必须真的注册 hooks.json 里的
    #    wasm hook(此前 hooks.json 的 `wasm` 字段在解析层被静默丢弃,
    #    整条 wasm hook 链路从未生效)。stderr(tracing 日志)应出现注册行。
    stderr_text = ""
    stderr_file = s.stderr_path
    if stderr_file.exists():
        stderr_text = stderr_file.read_text(errors="replace")
    assert "wasm hook registered" in stderr_text, (
        f"loader 应注册 wasm hook(查 stderr):{stderr_text[-2000:]}"
    )
    # 同名工具占用防护是注册期校验:demo_echo 未与内置工具同名,正常注册
    # (不触发);此处断言注册日志出现 wasm tool registered 佐证工具挂载。
    assert "wasm tool registered" in stderr_text, stderr_text[-2000:]
