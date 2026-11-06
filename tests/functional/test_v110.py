"""能力覆盖层 · v1.10 五项功能(字段级断言)。

覆盖:
- 功能1:一键导入其他主流本地 agent 配置(zhuye-import config)
- 功能2:goal + 队列模式(goal-queue 插件门控)
- 功能3:模型上下文最大长度配置([model].context_window / [context_windows])
- 功能4:思考强度配置(model.reasoning_effort / set_effort max|ultra)
- 功能5:空 API key(anthropic 兼容端点免 key 注册)

依据:AGENTS.md 开发流程检查表 —— 能力断言到字段级,不接受"退出码 0"。
"""

import json
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

IMPORT_BIN = Path(ZHUYE_BIN).parent / "zhuye-import"
REPO_ROOT = Path(ZHUYE_BIN).resolve().parents[2]


def _require_import_bin() -> None:
    if not IMPORT_BIN.is_file():
        pytest.skip(f"zhuye-import 二进制不存在:{IMPORT_BIN}")


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

    # 非法档位:非零退出 + 提示合法值清单。
    bad = run_cli(sandbox, "config", "set", "model.reasoning_effort", "extreme", timeout=30)
    assert bad.returncode != 0, f"非法档位应拒绝:{bad.stdout}"
    assert "low|medium|high|max|ultra" in bad.stdout + bad.stderr

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
    """`[anthropic] api_key="" base_url=...`(免 key 本地网关)→ doctor 识别
    active provider 为 anthropic;去掉 base_url 则视为未配置。"""
    require_binary()
    base = "# 测试用最小配置:关闭全部内置 hook\n[hooks]\nenabled = []\n"
    # 对照组先行:无 base_url 的空 key → 未配置。
    _config_toml(sandbox).write_text(base + '\n[anthropic]\napi_key = ""\n')
    bare = run_cli(sandbox, "doctor", "--check-network", timeout=60)
    assert "no [active].provider set" in bare.stdout, f"空 key 无 base_url 应未配置:{bare.stdout[:300]}"

    # 主断言:显式 base_url + 空 key → anthropic 注册并被推出。
    _config_toml(sandbox).write_text(
        base + '\n[anthropic]\napi_key = ""\nbase_url = "http://127.0.0.1:9/v1"\n'
    )
    out = run_cli(sandbox, "doctor", "--check-network", timeout=60)
    assert "checking provider 'anthropic'" in out.stdout, (
        f"免 key anthropic 兼容端点应注册并成为 active provider:{out.stdout[:400]}"
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


def _make_agent_fixture(base: Path) -> Path:
    """构造带三家 agent 配置的伪 HOME(形状与各来源单测同源)。"""
    home = base / "agent-home"
    (home / ".claude").mkdir(parents=True)
    (home / ".claude" / "settings.json").write_text(
        json.dumps(
            {
                "env": {
                    "ANTHROPIC_API_KEY": "sk-ant-fx",
                    "ANTHROPIC_BASE_URL": "https://gw.example.com/v1",
                },
                "model": "claude-sonnet-4-latest",
            }
        )
    )
    (home / ".claude.json").write_text(
        json.dumps({"mcpServers": {"fetch": {"command": "uvx", "args": ["mcp-server-fetch"], "env": {"K": "V"}}}})
    )
    oc = home / ".config" / "opencode"
    oc.mkdir(parents=True)
    (oc / "opencode.json").write_text(
        json.dumps(
            {
                "model": "deepseek/deepseek-chat",
                "provider": {
                    "deepseek": {"baseURL": "https://api.deepseek.com/v1", "apiKey": "sk-ds-fx"},
                    "local": {"baseURL": "http://127.0.0.1:8080/v1", "apiKey": ""},
                },
                "mcp": {"http-one": {"url": "https://mcp.example/mcp"}},
            }
        )
    )
    codex = home / ".codex"
    codex.mkdir(parents=True)
    (codex / "config.toml").write_text(
        'model = "gpt-5.1"\nmodel_provider = "oss"\n\n'
        '[model_providers.oss]\nbase_url = "https://oss.example.com/v1"\n'
        f'env_key = "ZZ_V110_CODEX_KEY"\n\n[mcp_servers.linter]\ncommand = "npx"\nargs = ["-y", "mcp-linter"]\n'
    )
    return home


def test_V9_一键导入_字段级合并与幂等(sandbox, tmp_path) -> None:
    """`zhuye-import config --home <fixture> --output <cfg>`:三家来源的
    provider / MCP 增量合并进 config.toml(字段级),重复导入幂等。"""
    require_binary()
    _require_import_bin()
    home = _make_agent_fixture(tmp_path)
    # 目标放在 cwd(沙箱 ~/.zhuye/config.toml 由 fixture 预置,不能作为
    # 「dry-run 不写文件」的判据)。
    target = sandbox.cwd / "imported-config.toml"
    env = sandbox.env({"ZZ_V110_CODEX_KEY": "sk-codex-fx"})

    # dry-run:不写文件,报告里有全部待导入条目。
    dry = subprocess.run(
        [str(IMPORT_BIN), "config", "--home", str(home), "--output", str(target), "--dry-run", "--json"],
        capture_output=True, text=True, env=env, timeout=60,
    )
    assert dry.returncode == 0, dry.stderr[:400]
    report = json.loads(dry.stdout.strip().splitlines()[-1])
    assert report["dry_run"] is True
    assert not target.exists(), "dry-run 不应写文件"
    assert set(report["providers_added"]) == {
        "imported:claude-code:anthropic",
        "imported:opencode:deepseek",
        "imported:opencode:local",
        "imported:codex:oss",
    }, f"provider 集合:{report['providers_added']}"
    assert set(report["mcp_added"]) == {"fetch", "http-one", "linter"}

    # 真实导入:落盘并逐字段断言。
    run = subprocess.run(
        [str(IMPORT_BIN), "config", "--home", str(home), "--output", str(target), "--json"],
        capture_output=True, text=True, env=env, timeout=60,
    )
    assert run.returncode == 0, run.stderr[:400]
    import tomllib

    cfg = tomllib.loads(target.read_text())
    anthropic_creds = cfg["anthropic"]["credentials"]
    assert [c["label"] for c in anthropic_creds] == ["imported:claude-code:anthropic"]
    ac = anthropic_creds[0]
    assert ac["api_key"] == "sk-ant-fx"
    assert ac["base_url"] == "https://gw.example.com/v1"
    assert ac["model"] == "claude-sonnet-4-latest"

    openai_creds = {c["label"]: c for c in cfg["openai"]["credentials"]}
    assert set(openai_creds) == {
        "imported:opencode:deepseek",
        "imported:opencode:local",
        "imported:codex:oss",
    }
    assert openai_creds["imported:opencode:deepseek"]["api_key"] == "sk-ds-fx"
    assert openai_creds["imported:opencode:deepseek"]["base_url"] == "https://api.deepseek.com/v1"
    assert openai_creds["imported:opencode:deepseek"]["model"] == "deepseek-chat"
    # 空 key 的本地端点照常导入(功能5 联动)。
    assert openai_creds["imported:opencode:local"]["api_key"] == ""
    assert openai_creds["imported:opencode:local"]["base_url"] == "http://127.0.0.1:8080/v1"
    # codex env_key 引用的密钥从进程 env 解析。
    assert openai_creds["imported:codex:oss"]["api_key"] == "sk-codex-fx"
    assert openai_creds["imported:codex:oss"]["model"] == "gpt-5.1"

    mcp = cfg["mcp_servers"]
    assert mcp["fetch"]["command"] == "uvx"
    assert mcp["fetch"]["args"] == ["mcp-server-fetch"]
    assert mcp["fetch"]["env"] == {"K": "V"}
    assert mcp["http-one"]["url"] == "https://mcp.example/mcp"
    assert mcp["linter"]["command"] == "npx"

    # 重复导入:全部跳过(幂等),文件不变。
    again = subprocess.run(
        [str(IMPORT_BIN), "config", "--home", str(home), "--output", str(target), "--json"],
        capture_output=True, text=True, env=env, timeout=60,
    )
    assert again.returncode == 0
    report2 = json.loads(again.stdout.strip().splitlines()[-1])
    assert report2["providers_added"] == [], f"重复导入应无新增:{report2}"
    assert set(report2["providers_skipped"]) == set(report["providers_added"])
    assert report2["mcp_added"] == []
    assert set(report2["mcp_skipped"]) == set(report["mcp_added"])


def test_V10_配置导入后serve可用导入的mcp与凭证配置(sandbox, tmp_path) -> None:
    """导入产物是合法 config.toml:serve 以 mock 模型正常起会话
    (不校验网络,只验证配置可解析、不影响启动)。"""
    require_binary()
    _require_import_bin()
    home = _make_agent_fixture(tmp_path)
    target = _config_toml(sandbox)
    run = subprocess.run(
        [str(IMPORT_BIN), "config", "--home", str(home), "--output", str(target), "--json"],
        capture_output=True, text=True, env=sandbox.env(), timeout=60,
    )
    assert run.returncode == 0, run.stderr[:300]
    # 导入的 MCP 不实际拉起(命令不存在会失败仅 warn);核心断言是
    # serve 配置解析不炸、turn 正常。
    with ServeSession(sandbox, env={"ZHUYE_MODEL": "mock"}) as s:
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "hi"}]},
            timeout=DRAIN_TIMEOUT,
        )
    assert "turn_complete" in msg_types(evs), f"导入配置不应破坏启动:{msg_types(evs)}"


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
