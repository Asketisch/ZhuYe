"""能力覆盖层 · CLI 能力面与深度参数面(全 17 子命令)。

覆盖:
- C1 `serve` 的三个恢复入口(参数被接受 / 非法 id 报错明确);
- C2 `exec` 的 JSONL 输出契约(每行合法 JSON + 终态事件);
- C3 `session` 子命令(ls 能列出会话;show 对不存在的 id 报错);
- C4 `doctor` / `version` 的输出契约;
- C5 `session search` 跨会话全文搜索(命中 / 未命中 / `--json` 字段);
- C6 `exec` 的退出码映射(成功 0 / turn 失败 1 / plan mode 未产出计划 122)。
- C10–C18 全子命令深度参数面(原 tests/rust/cli_deep.sh 194 断言迁入):
  login / config / mcp / session 双 ID / traces / task+team / plugin
  marketplace / workspace+update+doctor+lsp+security / pipeline+discussion。

运行:tests/run_all.sh functional,或
  ZHUYE_BIN=... python3 -m pytest tests/functional/test_cli.py -v
"""

from __future__ import annotations

import json
import shlex
import time

import pytest

from wire import require_binary, rollout_files, run_cli

pytestmark = pytest.mark.usefixtures("sandbox")


def test_C1_serve_恢复入口(sandbox) -> None:
    """`serve` 接受 --resume / -c / -r;非法 thread id 报错信息明确。"""
    require_binary()
    help_out = run_cli(sandbox, "serve", "--help", timeout=30)
    assert help_out.returncode == 0, f"serve --help 退出码={help_out.returncode}"
    for flag in ("--resume", "--continue-last", "--resume-by"):
        assert flag in help_out.stdout, f"serve --help 缺 {flag}:{help_out.stdout[:400]}"

    # 非法 thread id:provider 检查在前,故给 mock provider 才能走到 id 解析。
    bad = run_cli(
        sandbox,
        "serve",
        "--resume",
        "not-a-uuid",
        env={"ZHUYE_MODEL": "mock"},
        timeout=30,
    )
    assert bad.returncode != 0, "非法 thread id 应报错"
    assert "invalid thread id" in (bad.stdout + bad.stderr), (
        f"报错文案异常:{bad.stdout + bad.stderr!r}"
    )


def test_C2_exec_输出JSONL(sandbox) -> None:
    """`exec` 的 stdout 每行是合法 JSON,且含 turn 事件流。"""
    require_binary()
    proc = run_cli(
        sandbox,
        "exec",
        "hello",
        env={"ZHUYE_MODEL": "mock"},
        timeout=60,
    )
    assert proc.returncode == 0, f"exec 退出码={proc.returncode}:{proc.stderr[:500]}"
    lines = [ln for ln in proc.stdout.splitlines() if ln.strip()]
    assert lines, f"exec 无输出:{proc.stdout!r}"
    events = []
    for ln in lines:
        try:
            events.append(json.loads(ln))
        except json.JSONDecodeError as exc:
            pytest.fail(f"exec 输出非 JSON 行:{ln!r}({exc})")
    types = [e.get("msg", {}).get("type") for e in events]
    assert "turn_started" in types, f"缺 turn_started:{types}"
    assert "turn_complete" in types, f"缺 turn_complete:{types}"


def test_C3_session_子命令(sandbox, serve_session, mock_script) -> None:
    """`session ls` 列出刚写入的会话;`session show` 对不存在的 id 报错非 0。

    索引里的 session_id 取自 SessionMeta 记录(与文件名的 thread id 不同)。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit({"type": "user_input", "items": [{"type": "text", "text": "一轮"}]})
    time.sleep(0.8)
    files = rollout_files(sandbox.home)
    assert files, "无会话文件"
    import json as _json

    session_id = None
    for ln in files[-1].read_text().splitlines():
        if ln.strip():
            rec = _json.loads(ln)
            if rec.get("type") == "session_meta":
                session_id = rec.get("session_id")
    assert session_id, "无 session_meta / session_id"

    listed = run_cli(sandbox, "session", "ls", env={"ZHUYE_MODEL": "mock"}, timeout=30)
    assert listed.returncode == 0, f"session ls 退出码={listed.returncode}:{listed.stderr}"
    assert session_id in listed.stdout, f"session ls 未列出:{listed.stdout!r}"

    missing = run_cli(
        sandbox, "session", "show", "00000000-0000-0000-0000-000000000000", timeout=30
    )
    assert missing.returncode != 0, "session show 不存在的 id 应报错"


def test_C4_doctor与version(sandbox) -> None:
    """`version` 的输出契约(doctor 的退出码 / 输出见 test_C6b)。"""
    require_binary()
    ver = run_cli(sandbox, "version", timeout=30)
    assert ver.returncode == 0, f"version 退出码={ver.returncode}:{ver.stderr}"
    assert "zhuye" in ver.stdout.lower(), f"version 输出异常:{ver.stdout!r}"


def test_C5_session_search_跨会话全文搜索(sandbox, serve_session, mock_script) -> None:
    """`session search <词>` 跨会话命中消息内容;`--json` 每行一个 JSON 对象。

    依据:crates/resources/zhuye-rollout/src/search.rs(大小写不敏感子串
    匹配、按 mtime 最近活跃优先、命中片段过 redact_string);
    crates/runtime/zhuye-cli/src/session.rs 的 `search`。
    """
    require_binary()
    needle = "needle-in-a-haystack"
    script = mock_script([f'{{"type":"text","text":"{needle}"}}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit({"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]})
    time.sleep(0.8)

    hit = run_cli(sandbox, "session", "search", needle, timeout=30)
    assert hit.returncode == 0, f"session search 退出码={hit.returncode}:{hit.stderr}"
    assert needle in hit.stdout, f"session search 未命中:{hit.stdout!r}"
    assert "1 hit(s)" in hit.stdout, f"命中计数不对:{hit.stdout!r}"

    # 未命中:退出 0 + 明确的 no matches 文案(不是静默空白)。
    miss = run_cli(sandbox, "session", "search", "zzz-不存在的词-zzz", timeout=30)
    assert miss.returncode == 0, f"未命中也应退出 0:{miss.stderr}"
    assert "no matches" in miss.stdout, f"未命中文案不对:{miss.stdout!r}"

    # --json:每行独立 JSON,字段齐。
    js = run_cli(sandbox, "session", "search", needle, "--json", timeout=30)
    assert js.returncode == 0, f"json 模式退出码={js.returncode}:{js.stderr}"
    lines = [ln for ln in js.stdout.splitlines() if ln.strip()]
    assert lines, f"json 输出为空:{js.stdout!r}"
    for ln in lines:
        try:
            obj = json.loads(ln)
        except json.JSONDecodeError as exc:
            pytest.fail(f"json 输出非 JSON 行:{ln!r}({exc})")
        for f in ("session_id", "file", "line_no", "role", "snippet"):
            assert f in obj, f"json 命中行缺字段 {f}:{obj}"
        assert needle in obj["snippet"], f"snippet 未含关键词:{obj}"


# C6 场景矩阵:exec 退出码映射,每场景独立报告(一个断言失败不遮蔽其余)。
# - 成功:exit_code_for(Success) → 0;
# - turn 失败:脚本首次调用即发工具调用,`ZHUYE_MAX_ITERATIONS=1` 下引擎
#   回到 model_call 时已超上限 → TurnComplete.status = MaxIterations → 1;
# - plan mode:mock 不会调 ExitPlanMode → 未见 PlanReady → 122
#   (`finish_plan_mode`)。
_C6_EXIT_SCENARIOS = [
    pytest.param(("exec", "hi"), {}, None, 0, id="成功退出0"),
    pytest.param(
        ("exec", "hi"),
        {"ZHUYE_MAX_ITERATIONS": "1"},
        ['{"type":"tool_call","name":"echo","args":{"text":"x"}}'],
        1,
        id="turn失败退出1",
    ),
    pytest.param(("exec", "--plan-mode", "hi"), {}, None, 122, id="plan未产出退出122"),
]


@pytest.mark.parametrize(
    ("args", "extra_env", "mock_lines", "expected_code"), _C6_EXIT_SCENARIOS
)
def test_C6_exec_退出码映射(
    sandbox, mock_script, args, extra_env, mock_lines, expected_code
) -> None:
    """`exec` 的退出码映射矩阵。

    依据:crates/runtime/zhuye-exec/src/lib.rs 的 `exit_code_for`
    (Error / 非 Success 的 TurnComplete / TurnAborted → 1)与
    `finish_plan_mode`(plan mode 未见 PlanReady → 122)。
    """
    require_binary()
    env = {"ZHUYE_MODEL": "mock", **extra_env}
    if mock_lines is not None:
        env["ZHUYE_MOCK_SCRIPT"] = str(mock_script(mock_lines))
    proc = run_cli(sandbox, *args, env=env, timeout=60)
    assert proc.returncode == expected_code, (
        f"{args} 应退出 {expected_code},实得 {proc.returncode}:"
        f"{proc.stderr[:400] or proc.stdout[-400:]}"
    )


def test_C6b_doctor_退出0且有输出(sandbox) -> None:
    """`doctor` 冒烟:退出 0 且 stdout 非空。

    单拆于此:C4 只覆盖 `version` 的输出契约;`doctor --check-network`
    变体在 test_v110.py。默认 `doctor` 的退出码与输出断言原本挤在
    C6 尾部,现按"一测一行为"独立。
    """
    require_binary()
    doc = run_cli(sandbox, "doctor", env={"ZHUYE_MODEL": "mock"}, timeout=60)
    assert doc.returncode == 0, f"doctor 退出码={doc.returncode}:{doc.stderr[:500]}"
    assert doc.stdout.strip(), "doctor 无输出"


def test_C7_hooks启停持久化(sandbox) -> None:
    """`zhuye hooks disable/enable` 写 `[hooks].disabled` 并生效:
    disable 后该 hook 不再 veto Stop(会话可正常收尾),enable 后恢复。

    依据:crates/abilities/zhuye-hooks/src/config.rs `build_engine` 的
    disabled apply + crates/runtime/zhuye-cli/src/hooks.rs。
    """
    require_binary()
    # 沙箱默认 `[hooks] enabled = []`(关全部重 hook);本用例显式启用
    # verification 一个,验名单增删与状态反映。
    (sandbox.home / ".zhuye/config.toml").write_text('[hooks]\nenabled = ["verification"]\n')

    # 1) 基线:ls 能列出注册的 hook(默认全启用)。
    proc = run_cli(sandbox, "hooks", "ls", timeout=30)
    assert proc.returncode == 0, f"hooks ls 退出码={proc.returncode}:{proc.stderr}"
    assert "verification" in proc.stdout, f"未列出内置 hook:\n{proc.stdout!r}"
    assert "disabled" not in proc.stdout, f"默认不应有禁用项:\n{proc.stdout!r}"

    # 2) disable verification → 写进 config.toml#hooks.disabled。
    proc = run_cli(sandbox, "hooks", "disable", "verification", timeout=30)
    assert proc.returncode == 0, f"hooks disable 退出码={proc.returncode}:{proc.stderr}"
    cfg = (sandbox.home / ".zhuye/config.toml").read_text()
    assert "disabled" in cfg and "verification" in cfg, f"config 未写禁用名单:\n{cfg!r}"

    # 3) ls 反映禁用状态。
    proc = run_cli(sandbox, "hooks", "ls", timeout=30)
    assert "disabled" in proc.stdout, f"ls 未反映禁用状态:\n{proc.stdout!r}"
    verification_line = [ln for ln in proc.stdout.splitlines() if "verification" in ln]
    assert verification_line and "disabled" in verification_line[0], (
        f"verification 应显示 disabled:{verification_line!r}"
    )

    # 4) enable → 从名单移除(幂等)。
    proc = run_cli(sandbox, "hooks", "enable", "verification", timeout=30)
    assert proc.returncode == 0, f"hooks enable 退出码={proc.returncode}:{proc.stderr}"
    cfg = (sandbox.home / ".zhuye/config.toml").read_text()
    assert "verification" not in cfg.split("disabled")[-1] if "disabled" in cfg else True, (
        f"enable 后 config 仍含 verification:\n{cfg!r}"
    )
    proc = run_cli(sandbox, "hooks", "ls", timeout=30)
    verification_line = [ln for ln in proc.stdout.splitlines() if "verification" in ln]
    assert verification_line and "disabled" not in verification_line[0], (
        f"enable 后 verification 应恢复:{verification_line!r}"
    )


def test_C8_插件hook禁用持久化(sandbox, serve_session, mock_script) -> None:
    """插件延迟注册时仍遵循禁用配置,重新启用后恢复执行。"""
    require_binary()
    plugin_dir = sandbox.cwd / "hookprobe"
    plugin_dir.mkdir()
    (plugin_dir / "plugin.toml").write_text(
        'name = "hookprobe"\nversion = "0.1.0"\nhooks = "./hooks.json"\n'
    )
    marker = sandbox.cwd / "hook-ran"
    (plugin_dir / "hooks.json").write_text(json.dumps({
        "hooks": {"PostToolUse": [{"matcher": "*", "hooks": [{
            "type": "command", "command": f"printf ran >> {shlex.quote(str(marker))}"
        }]}]}
    }))
    installed = run_cli(sandbox, "plugin", "install", str(plugin_dir), "--scope", "user")
    assert installed.returncode == 0, installed.stderr + installed.stdout
    name = "plugin:hookprobe@inline:PostToolUse#0"
    script = mock_script([
        '{"type":"tool_call","name":"echo","args":{"text":"hook-check"}}',
        '{"type":"text","text":"done"}',
    ])

    def run_turn() -> None:
        with serve_session(args=["--plugin", "hookprobe@inline"], env=sandbox.mock_env(script)) as session:
            events = session.submit({"type": "user_input", "items": [{"type": "text", "text": "执行"}]})
        ends = [event["msg"] for event in events if event["msg"]["type"] == "tool_call_end"]
        assert ends and ends[0]["is_error"] is False, events
        assert any(event["msg"]["type"] == "turn_complete" for event in events), events

    run_turn()
    assert marker.read_text() == "ran"
    marker.unlink()
    disabled = run_cli(sandbox, "hooks", "disable", name)
    assert disabled.returncode == 0, disabled.stderr + disabled.stdout
    run_turn()
    assert not marker.exists(), "已禁用的插件 hook 仍然执行"
    enabled = run_cli(sandbox, "hooks", "enable", name)
    assert enabled.returncode == 0, enabled.stderr + enabled.stdout
    run_turn()
    assert marker.read_text() == "ran"


def test_C7b_hooks配置损坏保护与并发更新(sandbox) -> None:
    """损坏配置保持原样；并发启停保留其它字段和注释。"""
    from concurrent.futures import ThreadPoolExecutor
    import tomllib

    require_binary()
    path = sandbox.home / ".zhuye/config.toml"
    malformed = '# 原始配置\n[hooks\nenabled = ["verification"]\n'
    path.write_text(malformed)
    for args in [("disable", "verification"), ("enable", "verification"), ("ls",)]:
        result = run_cli(sandbox, "hooks", *args, env={"ZHUYE_HOME": ""}, timeout=30)
        assert result.returncode != 0, result.stdout
        assert path.read_text() == malformed

    original = '# 保留的顶层注释\n[hooks]\nenabled = ["verification"] # 原始名单\n\n[future_extension]\nvalue = "keep" # 保留未知字段\n'
    path.write_text(original)
    names = [f"plugin:future:Stop#{index}" for index in range(12)]
    def disable(name: str):
        return run_cli(sandbox, "hooks", "disable", name, env={"ZHUYE_HOME": ""}, timeout=30)
    with ThreadPoolExecutor(max_workers=8) as pool:
        results = list(pool.map(disable, names))
    assert all(result.returncode == 0 for result in results), [result.stderr for result in results]
    text = path.read_text()
    config = tomllib.loads(text)
    assert set(config["hooks"]["disabled"]) == set(names)
    assert config["hooks"]["enabled"] == ["verification"]
    assert config["future_extension"]["value"] == "keep"
    assert '# 保留的顶层注释' in text and '# 原始名单' in text and '# 保留未知字段' in text

    result = run_cli(sandbox, "hooks", "enable", names[0], env={"ZHUYE_HOME": ""}, timeout=30)
    assert result.returncode == 0, result.stderr
    updated = tomllib.loads(path.read_text())
    assert set(updated["hooks"]["disabled"]) == set(names[1:])
    assert updated["future_extension"]["value"] == "keep"

    path.write_text('hooks = { enabled = ["verification"], custom = 7 }\n')
    result = run_cli(sandbox, "hooks", "disable", "verification", env={"ZHUYE_HOME": ""}, timeout=30)
    assert result.returncode == 0, result.stderr
    inline = tomllib.loads(path.read_text())["hooks"]
    assert inline == {"enabled": ["verification"], "custom": 7, "disabled": ["verification"]}


def test_C9_插件随数据目录隔离(sandbox, serve_session, mock_script) -> None:
    """插件安装记录、缓存和运行时加载均遵循 ZHUYE_HOME。"""
    require_binary()
    home_a = sandbox.cwd / "data-a"
    home_b = sandbox.cwd / "data-b"
    for home in (home_a, home_b):
        home.mkdir()
        (home / "config.toml").write_text('[hooks]\nenabled = []\n')
    plugin = sandbox.cwd / "isolated-plugin"
    plugin.mkdir()
    (plugin / "plugin.toml").write_text('name = "isolated"\nversion = "0.1.0"\nhooks = "./hooks.json"\n')
    marker = sandbox.cwd / "isolated-hook-ran"
    (plugin / "hooks.json").write_text(json.dumps({"hooks": {"PostToolUse": [{"matcher": "*", "hooks": [
        {"type": "command", "command": f"printf ran >> {shlex.quote(str(marker))}"}
    ]}]}}))
    env_a = {"ZHUYE_HOME": str(home_a)}
    env_b = {"ZHUYE_HOME": str(home_b)}
    installed = run_cli(sandbox, "plugin", "install", str(plugin), "--scope", "user", env=env_a)
    assert installed.returncode == 0, installed.stderr + installed.stdout
    state = json.loads((home_a / "plugins/installed_plugins.json").read_text())
    assert "isolated@inline" in json.dumps(state), state
    assert not (home_b / "plugins/installed_plugins.json").exists()
    assert not (sandbox.home / ".zhuye/plugins/installed_plugins.json").exists()
    listed_a = run_cli(sandbox, "plugin", "list", env=env_a)
    listed_b = run_cli(sandbox, "plugin", "list", env=env_b)
    assert listed_a.returncode == listed_b.returncode == 0
    assert "isolated@inline" in listed_a.stdout and "isolated@inline" not in listed_b.stdout

    script = mock_script(['{"type":"tool_call","name":"echo","args":{"text":"probe"}}',
                          '{"type":"text","text":"done"}'])
    for home, should_run in [(home_a, True), (home_b, False)]:
        with serve_session(args=["--plugin", "isolated@inline"],
                           env=sandbox.mock_env(script, ZHUYE_HOME=str(home))) as session:
            events = session.submit({"type": "user_input", "items": [{"type": "text", "text": "执行"}]})
        ends = [event["msg"] for event in events if event["msg"]["type"] == "tool_call_end"]
        assert ends and not ends[0]["is_error"], events
        assert marker.exists() is should_run
        if marker.exists():
            assert marker.read_text() == "ran"
            marker.unlink()


# ══ CLI 参数面迁移(原 tests/rust/cli_deep.sh 194 断言)══


def _write_fake_session(base, thread_id: str, internal_id: str) -> None:
    """在 rollout 目录树写一个最小 2 行 session fixture(可控 UUID)。"""
    d = base / "2026" / "08" / "01"
    d.mkdir(parents=True, exist_ok=True)
    (d / f"{thread_id}.jsonl").write_text(
        json.dumps({"type": "session_meta", "session_id": internal_id,
                    "model": "mock/mock-1", "started_at": "2026-08-01T00:00:00Z",
                    "message_count": 1}) + "\n"
        + json.dumps({"type": "message", "role": "user", "content": "fake session body"}) + "\n"
    )


def _real_session_ids(sandbox):
    """mock exec 一次后,返回 (thread 文件名 id, 内部 session_id)。"""
    files = sorted((sandbox.home / ".zhuye" / "sessions").rglob("*.jsonl"))
    assert files, "exec 未产生 session 文件"
    thread = files[0].stem
    internal = json.loads(files[0].read_text().splitlines()[0])["session_id"]
    return thread, internal


def test_C10_login四provider与脱敏(sandbox) -> None:
    """login 四 provider / 缺 key 报错 / 短 key 告警与 --force。"""
    require_binary()
    p = run_cli(sandbox, "login", "--provider", "anthropic", "--api-key", "sk-ant-test-12345678")
    assert p.returncode == 0, p.stderr
    assert "Config saved to" in p.stdout
    assert "[anthropic]" in run_cli(sandbox, "config", "show").stdout

    p = run_cli(sandbox, "login", "--provider", "openai", "--api-key", "sk-oai-test-12345678")
    assert p.returncode == 0, p.stderr
    # api_key 脱敏(短 key 覆写之前检查)。
    show = run_cli(sandbox, "config", "show")
    assert "sk-oai-test-12345678" not in show.stdout, show.stdout[:300]
    revealed = run_cli(sandbox, "config", "show", "--reveal")
    assert "sk-oai-test-12345678" in revealed.stdout

    for provider, key in (("ollama", "ollama-local-key-123"), ("local", "ollama-local-key-456")):
        p = run_cli(sandbox, "login", "--provider", provider, "--api-key", key)
        assert p.returncode == 0, f"{provider}: {p.stderr}"

    p = run_cli(sandbox, "login", "--provider", "openai", input_text="")
    assert p.returncode != 0, p.stdout
    assert "api_key" in p.stdout + p.stderr

    p = run_cli(sandbox, "login", "--provider", "openai", "--api-key", "sk-x")
    assert p.returncode == 0, p.stderr
    assert "very short" in p.stdout + p.stderr
    p = run_cli(sandbox, "login", "--provider", "openai", "--api-key", "sk-x", "--force")
    assert p.returncode == 0, p.stderr
    assert "very short" not in p.stdout + p.stderr


def test_C11_config_set_unset_ls(sandbox) -> None:
    """config set / ls / 非法 key / unset / show 输出契约。"""
    require_binary()
    p = run_cli(sandbox, "config", "set", "anthropic.model", "claude-x-test")
    assert p.returncode == 0, p.stderr
    assert "set anthropic.model = claude-x-test" in p.stdout

    ls = run_cli(sandbox, "config", "ls")
    assert "claude-x-test" in ls.stdout
    assert "<unset>" in ls.stdout

    p = run_cli(sandbox, "config", "set", "not.a.real.key", "1")
    assert p.returncode != 0
    assert "allowed keys" in p.stdout + p.stderr
    assert "anthropic.model" in p.stdout + p.stderr

    p = run_cli(sandbox, "config", "unset", "anthropic.model")
    assert p.returncode == 0, p.stderr
    ls = run_cli(sandbox, "config", "ls")
    model_line = next(line for line in ls.stdout.splitlines() if "anthropic.model" in line)
    assert "<unset>" in model_line, model_line

    show = run_cli(sandbox, "config", "show")
    assert "ReflectConfig @" in show.stdout


def test_C12_mcp双传输与registry(sandbox) -> None:
    """mcp add(stdio+http)/ 互斥 / ls / show / test / remove / registry。"""
    from wire import MOCK_MCP

    require_binary()
    if not MOCK_MCP.is_file():
        pytest.skip("mock_mcp_server 未构建(cargo build -p zhuye-mcp)")

    p = run_cli(sandbox, "mcp", "add", "fsx", "--command", str(MOCK_MCP))
    assert p.returncode == 0, p.stderr
    assert "added server 'fsx'" in p.stdout
    p = run_cli(sandbox, "mcp", "add", "http1", "--url", "https://example.com/mcp",
                "--header", "Authorization: Bearer x")
    assert p.returncode == 0, p.stderr
    p = run_cli(sandbox, "mcp", "add", "both1", "--command", "x", "--url", "https://e.com")
    assert p.returncode == 2, p.returncode
    assert "cannot be used with" in p.stdout + p.stderr

    ls = run_cli(sandbox, "mcp", "ls").stdout
    assert "fsx" in ls and "http1" in ls and "stdio" in ls

    show = run_cli(sandbox, "mcp", "show", "fsx").stdout
    assert 'type = "stdio"' in show and "command" in show
    show = run_cli(sandbox, "mcp", "show", "http1").stdout
    assert "example.com/mcp" in show
    p = run_cli(sandbox, "mcp", "show", "missing")
    assert p.returncode != 0 and "not in config" in p.stdout + p.stderr

    p = run_cli(sandbox, "mcp", "test", "fsx")
    assert p.returncode == 0, p.stderr
    assert "mcp__fsx__echo" in p.stdout and "mcp__fsx__slow" in p.stdout
    run_cli(sandbox, "mcp", "add", "bad", "--command", "/nonexistent/zhuye-mcp-bad")
    p = run_cli(sandbox, "mcp", "test", "bad")
    assert p.returncode != 0
    assert "failed to start" in p.stdout + p.stderr

    p = run_cli(sandbox, "mcp", "remove", "fsx")
    assert p.returncode == 0 and "removed server 'fsx'" in p.stdout
    assert run_cli(sandbox, "mcp", "show", "fsx").returncode != 0

    ls = run_cli(sandbox, "mcp", "registry", "ls")
    assert ls.returncode == 0, ls.stderr
    for rid in ("filesystem", "github", "postgres"):
        assert rid in ls.stdout, ls.stdout[:300]
    assert run_cli(sandbox, "mcp", "registry", "show", "fetch").returncode == 0
    p = run_cli(sandbox, "mcp", "registry", "show", "google-search")
    assert p.returncode != 0 and "unknown registry id" in p.stdout + p.stderr


def test_C13_session双ID前缀与生命周期(sandbox, serve_session, mock_script) -> None:
    """双 ID 前缀解析回归 + ls 过滤 / rename / export / fork / rm。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"hello"}'])
    p = run_cli(sandbox, "exec", "hello", env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)})
    assert p.returncode == 0, p.stderr
    thread, internal = _real_session_ids(sandbox)
    base = sandbox.home / ".zhuye" / "sessions"

    # 双 ID 前缀解析(产品修复回归)。
    p = run_cli(sandbox, "session", "show", thread[:8])
    assert p.returncode == 0, p.stderr
    assert "Path:" in p.stdout and f"{thread}.jsonl" in p.stdout
    p = run_cli(sandbox, "session", "show", internal[:8])
    assert p.returncode == 0 and "Path:" in p.stdout, p.stderr
    assert run_cli(sandbox, "session", "show", internal[:12]).returncode == 0
    assert run_cli(sandbox, "session", "show", internal).returncode == 0
    assert run_cli(sandbox, "session", "show", thread).returncode == 0
    p = run_cli(sandbox, "session", "show", "zz")
    assert "no session matches prefix" in p.stdout + p.stderr

    # 受控 fixture:8 位前缀歧义。
    _write_fake_session(base, "11111111-0000-4000-8000-000000000001", "11111111-0000-4000-8000-000000000001")
    _write_fake_session(base, "11111111-0000-4000-8000-000000000002", "11111111-0000-4000-8000-000000000002")
    p = run_cli(sandbox, "session", "show", "1111")
    assert p.returncode != 0
    assert "matches 2 sessions" in p.stdout + p.stderr
    assert run_cli(sandbox, "session", "show", "11111111-0000-4000-8000-000000000001").returncode == 0

    # 受控 fixture:文件名 id ≠ 内部 id(双 ID 兜底)。
    _write_fake_session(base, "22222222-0000-4000-8000-000000000001", "44444444-0000-4000-8000-000000000001")
    assert run_cli(sandbox, "session", "show", "44444444").returncode == 0
    p = run_cli(sandbox, "session", "show", "22222222")
    assert p.returncode == 0, p.stderr
    assert "22222222-0000-4000-8000-000000000001.jsonl" in p.stdout

    # 生命周期。
    ls = run_cli(sandbox, "session", "ls").stdout
    assert internal in ls and "11111111-0000-4000-8000-000000000001" in ls
    ls = run_cli(sandbox, "session", "ls", "--model", "mock").stdout
    assert internal in ls
    ls = run_cli(sandbox, "session", "ls", "--model", "no-such-model").stdout
    assert "(no sessions found" in ls
    ls = run_cli(sandbox, "session", "ls", "--limit", "2").stdout
    # grep -c 语义:按「匹配行数」计,不按出现次数。
    import re as _re
    matched_lines = [line for line in ls.splitlines() if _re.search(r"  \d", line)]
    assert len(matched_lines) <= 3, ls

    p = run_cli(sandbox, "session", "rename", internal[:8], "mytest-name")
    assert p.returncode == 0 and "Renamed session" in p.stdout, p.stderr
    p = run_cli(sandbox, "session", "show", internal[:8])
    assert "Title:   mytest-name" in p.stdout

    p = run_cli(sandbox, "session", "export", internal)
    assert p.returncode == 0 and "# ZhuYe session" in p.stdout
    out_md = sandbox.cwd / "c13-export.md"
    run_cli(sandbox, "session", "export", internal, "--out", str(out_md))
    assert out_md.stat().st_size > 0

    p = run_cli(sandbox, "session", "fork", internal, "--branch", "b1")
    assert p.returncode == 0 and "Forked session" in p.stdout, p.stderr

    victim = base / "2026" / "08" / "01" / "11111111-0000-4000-8000-000000000001.jsonl"
    vid = "11111111-0000-4000-8000-000000000001"
    p = run_cli(sandbox, "session", "rm", vid, input_text="n")
    assert victim.exists() and "aborted" in p.stdout + p.stderr
    p = run_cli(sandbox, "session", "rm", vid, "--yes")
    assert p.returncode == 0, p.stderr
    assert not victim.exists()
    assert run_cli(sandbox, "session", "show", vid).returncode != 0


def test_C14_traces_show与ls(sandbox, mock_script) -> None:
    """traces 落盘 / show 前缀(Calls/model/usage/request)/ ls / 错误路径。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"trace-me"}'])
    p = run_cli(sandbox, "exec", "trace-me", env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)})
    assert p.returncode == 0, p.stderr

    traces_dir = sandbox.home / ".zhuye" / "traces" / "model-io"
    files = sorted(traces_dir.glob("*.jsonl")) if traces_dir.exists() else []
    assert files, "traces 未落盘 model-io"
    name = files[0].stem
    assert name.startswith("model-io-sess_")
    tprefix = name[len("model-io-sess_"):][:8]
    p = run_cli(sandbox, "traces", "show", tprefix)
    assert p.returncode == 0, p.stderr
    for needle in ("Calls:", "model:", "usage:", "request:"):
        assert needle in p.stdout, needle

    ls = run_cli(sandbox, "traces", "ls")
    assert ls.returncode == 0 and "mock" in ls.stdout
    assert run_cli(sandbox, "traces", "ls", "-n", "5").returncode == 0
    p = run_cli(sandbox, "traces", "show", "deadbeef")
    assert p.returncode != 0
    assert "no trace session matches" in p.stdout + p.stderr


def test_C15_task与team全链路(sandbox) -> None:
    """task create/ls/show/update/stop/purge + team create/sync/delete。"""
    require_binary()
    p = run_cli(sandbox, "task", "create", "--subject", "S1", "--list", "L1",
                "--description", "d1", "--active-form", "Doing")
    assert p.returncode == 0, p.stderr
    assert "Task #1 created in list 'L1'" in p.stdout
    tid = next(ch for ch in p.stdout.split() if ch.startswith("#")).lstrip("#")

    ls = run_cli(sandbox, "task", "ls").stdout
    assert "S1" in ls and "pending" in ls

    p = run_cli(sandbox, "task", "show", tid, "--list", "L1")
    assert p.returncode == 0 and "S1" in p.stdout and "pending" in p.stdout
    p = run_cli(sandbox, "task", "show", tid)
    assert p.returncode == 2, p.returncode

    p = run_cli(sandbox, "task", "update", tid, "--list", "L1", "--status", "completed")
    assert p.returncode == 0, p.stderr
    assert "completed" in run_cli(sandbox, "task", "ls").stdout
    p = run_cli(sandbox, "task", "update", tid, "--list", "L1", "--subject", "S1-renamed")
    assert p.returncode == 0, p.stderr
    assert "S1-renamed" in run_cli(sandbox, "task", "ls").stdout

    # 非数字 id:clap u32 解析失败(rc=2)。
    assert run_cli(sandbox, "task", "update", "abc", "--list", "L1", "--status", "done").returncode == 2
    assert run_cli(sandbox, "task", "stop", "abc", "--list", "L1").returncode == 2

    assert run_cli(sandbox, "task", "team", "create", "myteam", "--description", "d").returncode == 0
    assert "myteam" in run_cli(sandbox, "task", "team", "ls").stdout
    assert run_cli(sandbox, "task", "team", "show", "myteam").returncode == 0
    assert run_cli(sandbox, "task", "team", "sync", "--list-specs").returncode == 0
    p = run_cli(sandbox, "task", "team", "delete", "myteam", input_text="n")
    assert "myteam" in run_cli(sandbox, "task", "team", "ls").stdout
    assert "aborted" in p.stdout + p.stderr
    p = run_cli(sandbox, "task", "team", "delete", "myteam", "--yes")
    assert p.returncode == 0, p.stderr
    assert "myteam" not in run_cli(sandbox, "task", "team", "ls").stdout

    p = run_cli(sandbox, "task", "stop", tid, "--list", "L1")
    assert p.returncode == 0, p.stderr
    assert "S1-renamed" not in run_cli(sandbox, "task", "ls").stdout
    assert run_cli(sandbox, "task", "purge", "--list", "L1", "--yes").returncode == 0


def test_C16_plugin_marketplace全链路(sandbox) -> None:
    """marketplace add/ls/坏目录/install/info/show/enable/disable/uninstall/inline/remove。"""
    require_binary()
    mkt = sandbox.cwd / "mkt"
    (mkt / ".claude-plugin").mkdir(parents=True)
    (mkt / "plugins" / "hello").mkdir(parents=True)
    (mkt / "plugins" / "hello" / "plugin.toml").write_text(
        'name = "hello"\nversion = "0.1.0"\ndescription = "deep test plugin"\n'
    )
    (mkt / ".claude-plugin" / "marketplace.json").write_text(json.dumps({
        "name": "deep-mk",
        "owner": {"name": "Test"},
        "plugins": [{"name": "hello", "version": "0.1.0",
                     "source": {"source": "directory", "path": "./plugins/hello"}}],
    }))

    p = run_cli(sandbox, "plugin", "marketplace", "add", "deep-mk",
                "--from", "directory", "--path", str(mkt))
    assert p.returncode == 0 and "added marketplace 'deep-mk'" in p.stdout, p.stderr
    assert "deep-mk" in run_cli(sandbox, "plugin", "marketplace", "ls").stdout

    badmkt = sandbox.cwd / "badmkt"
    badmkt.mkdir()
    p = run_cli(sandbox, "plugin", "marketplace", "add", "bad-mk",
                "--from", "directory", "--path", str(badmkt))
    assert p.returncode != 0 and "manifest" in p.stdout + p.stderr

    p = run_cli(sandbox, "plugin", "install", "hello@deep-mk")
    assert p.returncode == 0 and "hello@deep-mk" in p.stdout, p.stderr
    assert "hello@deep-mk" in run_cli(sandbox, "plugin", "list").stdout

    p = run_cli(sandbox, "plugin", "info", "hello@deep-mk")
    assert p.returncode == 0, p.stderr
    assert "user" in p.stdout and "0.1.0" in p.stdout
    assert 'name = "hello"' in run_cli(sandbox, "plugin", "show", "hello@deep-mk").stdout

    p = run_cli(sandbox, "plugin", "enable", "hello@deep-mk")
    assert p.returncode == 0, p.stderr
    cfg = (sandbox.home / ".zhuye" / "config.toml").read_text()
    assert 'enabled_plugins = ["hello@deep-mk"]' in cfg
    p = run_cli(sandbox, "plugin", "disable", "hello@deep-mk")
    assert p.returncode == 0, p.stderr
    assert "hello@deep-mk" not in (sandbox.home / ".zhuye" / "config.toml").read_text()

    p = run_cli(sandbox, "plugin", "uninstall", "hello@deep-mk", "--yes")
    assert p.returncode == 0 and "uninstalled" in p.stdout, p.stderr
    p = run_cli(sandbox, "plugin", "info", "hello@deep-mk")
    assert p.returncode != 0 and "not installed" in p.stdout + p.stderr

    p = run_cli(sandbox, "plugin", "install", str(mkt / "plugins" / "hello"))
    assert p.returncode == 0 and "hello@inline" in p.stdout, p.stderr
    assert run_cli(sandbox, "plugin", "marketplace", "refresh").returncode == 0
    p = run_cli(sandbox, "plugin", "marketplace", "remove", "deep-mk")
    assert p.returncode == 0 and "removed marketplace 'deep-mk'" in p.stdout
    assert "deep-mk" not in run_cli(sandbox, "plugin", "marketplace", "ls").stdout


def test_C17_workspace_update_doctor_lsp_security(sandbox) -> None:
    """workspace clone(本地 / file:// shallow / 坏 URL)、update、doctor、lsp、security。"""
    import subprocess

    require_binary()
    assert "workspace" in run_cli(sandbox, "workspace", "ls").stdout

    src = sandbox.cwd / "clone_src"
    src.mkdir()
    def _git(*args: str, cwd=None) -> bool:
        return subprocess.run(["git", *args], cwd=str(cwd or src),
                              capture_output=True).returncode == 0
    assert _git("init", "-q")
    assert _git("config", "user.email", "t@t")
    assert _git("config", "user.name", "t")
    (src / "f.txt").write_text("hello")
    assert _git("add", "f.txt")
    assert _git("commit", "-qm", "init")

    p = run_cli(sandbox, "workspace", "clone", str(src), "--dest", "cloned")
    assert p.returncode == 0, p.stderr
    assert (sandbox.cwd / "cloned" / "f.txt").exists()

    p = run_cli(sandbox, "workspace", "clone", f"file://{src}",
                "--dest", "cloned_shallow", "--depth", "1")
    assert p.returncode == 0, p.stderr
    shallow = sandbox.cwd / "cloned_shallow"
    assert (shallow / "f.txt").exists() and (shallow / ".git" / "shallow").exists()
    rev = subprocess.run(["git", "-C", str(shallow), "rev-parse", "--is-shallow-repository"],
                         capture_output=True, text=True)
    assert "true" in rev.stdout

    p = run_cli(sandbox, "workspace", "clone", "https://invalid.invalid/nope", "--dest", "wbad")
    assert p.returncode != 0 and "git clone" in p.stdout + p.stderr

    p = run_cli(sandbox, "update")
    assert "zhuye v" in p.stdout and "cargo install" in p.stdout
    p = run_cli(sandbox, "update", "--check-only")
    assert p.returncode == 0 and "check-only" in p.stdout, p.stderr

    p = run_cli(sandbox, "doctor")
    assert p.returncode == 0, p.stderr
    assert "config" in p.stdout and "rollout" in p.stdout
    assert "network probe skipped" in p.stdout
    p = run_cli(sandbox, "doctor", "--check-network", env={"ZHUYE_MODEL": "mock"})
    assert p.returncode == 0, p.stderr
    assert "[network]" in p.stdout and "checking provider" in p.stdout

    p = run_cli(sandbox, "lsp", "list")
    assert "no [lsp_servers" in p.stdout
    p = run_cli(sandbox, "lsp", "status", "rust")
    assert p.returncode != 0 and "not configured" in p.stdout + p.stderr
    cfg = sandbox.home / ".zhuye" / "config.toml"
    cfg.write_text(cfg.read_text() + (
        '\n[lsp_servers.rust]\ncommand = "rust-analyzer"\n'
        'file_patterns = [{ glob = "**/*.rs", language_id = "rust" }]\n'
    ))
    assert "rust-analyzer" in run_cli(sandbox, "lsp", "list").stdout
    p = run_cli(sandbox, "lsp", "status", "rust")
    assert p.returncode == 0, p.stderr
    assert "rust-analyzer" in p.stdout and "**/*.rs" in p.stdout

    p = run_cli(sandbox, "security", "audit")
    assert p.returncode == 0 and "ZhuYe security audit" in p.stdout, p.stderr


def test_C18_pipeline与discussion错误路径(sandbox, mock_script) -> None:
    """pipeline team/runner 错误路径;discussion mock 驱动与 --output 文件。"""
    require_binary()
    p = run_cli(sandbox, "pipeline", "team", "ghost", "--topic", "t")
    assert p.returncode != 0 and "team not found: ghost" in p.stdout + p.stderr

    run_cli(sandbox, "task", "team", "create", "only")
    (sandbox.cwd / "pipe.toml").write_text(
        '[pipeline]\nname = "deep"\nfailure_policy = "abort"\n\n'
        '[nodes.only]\nteam = "only"\ntemplate = "Do: {{topic}}"\ndepends_on = []\n'
    )
    p = run_cli(sandbox, "pipeline", "run", "-c", str(sandbox.cwd / "pipe.toml"), "--topic", "t1")
    assert p.returncode != 0
    assert "no runner for node 'only'" in p.stdout + p.stderr

    script = mock_script([
        '{"type":"tool_call","name":"send_message","args":{"content":"advocate says async","kind":"utterance"}}',
        '{"type":"tool_call","name":"send_message","args":{"content":"skeptic says sync","kind":"utterance"}}',
        '{"type":"text","text":"a"}',
        '{"type":"text","text":"b"}',
    ])
    (sandbox.cwd / "disc.toml").write_text(
        '[discussion]\nmode = "sequential"\ntopic = "deep-topic"\nmax_rounds = 1\n\n'
        '[[agents]]\nrole = "advocate"\nsystem_prompt = "advocate"\n'
        'allowed_tools = ["send_message", "read_messages", "finish_discussion"]\n\n'
        '[[agents]]\nrole = "skeptic"\nsystem_prompt = "skeptic"\n'
        'allowed_tools = ["send_message", "read_messages", "finish_discussion"]\n'
    )
    disc_env = {"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}
    p = run_cli(sandbox, "discussion", "run", "-c", str(sandbox.cwd / "disc.toml"), env=disc_env)
    assert p.returncode == 0, p.stderr
    for needle in ("=== Discussion Transcript ===", "=== Result ===", "rounds_completed",
                   '"transcript_len": 2', "advocate says async", "skeptic says sync",
                   '"collab_message"'):
        assert needle in p.stdout, needle

    out_file = sandbox.cwd / "c18-report.json"
    p = run_cli(sandbox, "discussion", "run", "-c", str(sandbox.cwd / "disc.toml"),
                "-o", str(out_file), env=disc_env)
    assert out_file.stat().st_size > 0
    content = out_file.read_text()
    assert '"rounds_completed"' in content and "Discussion Transcript" in content

    p = run_cli(sandbox, "discussion", "ls")
    assert p.returncode == 0 and "(no sessions found" in p.stdout
    p = run_cli(sandbox, "discussion", "ls", "--discussion-id",
                "00000000-0000-4000-8000-000000000000")
    assert p.returncode == 0, p.stderr


def test_C19_secrets加密存储全链路(sandbox) -> None:
    """`secrets` 子命令:set(stdin)/ get / list / delete 确认与 --yes /
    损坏 store fail-closed / machine key 权限 fail-closed(unix)。"""
    import sys

    require_binary()
    store_dir = sandbox.home / ".zhuye" / "secrets"

    # set(stdin 读明文)→ get 往返一致。
    p = run_cli(sandbox, "secrets", "set", "openai", input_text="sk-live-abc123\n")
    assert p.returncode == 0 and "stored" in p.stdout, p.stderr
    assert (store_dir / "secrets.json").exists()
    raw = (store_dir / "secrets.json").read_text()
    assert "sk-live-abc123" not in raw, "明文不得落盘"
    p = run_cli(sandbox, "secrets", "get", "openai")
    assert p.returncode == 0 and p.stdout == "sk-live-abc123\n", (p.returncode, p.stdout)

    # list 只出条目名;set 空输入报错。
    p = run_cli(sandbox, "secrets", "set", "empty", input_text="\n")
    assert p.returncode != 0, p.stdout
    assert "stdin is empty" in p.stdout + p.stderr
    run_cli(sandbox, "secrets", "set", "anthropic", input_text="sk-ant-def")
    ls = run_cli(sandbox, "secrets", "list")
    assert ls.returncode == 0
    assert "openai" in ls.stdout and "anthropic" in ls.stdout
    assert "sk-" not in ls.stdout, "list 不得泄露值"

    # delete:拒绝保留 + aborted;--yes 删除;缺失报错。
    p = run_cli(sandbox, "secrets", "delete", "anthropic", input_text="n")
    assert p.returncode == 0 and "aborted" in p.stdout + p.stderr
    assert "anthropic" in run_cli(sandbox, "secrets", "list").stdout
    p = run_cli(sandbox, "secrets", "delete", "anthropic", "--yes")
    assert p.returncode == 0 and "deleted" in p.stdout, p.stderr
    assert "anthropic" not in run_cli(sandbox, "secrets", "list").stdout
    p = run_cli(sandbox, "secrets", "get", "anthropic")
    assert p.returncode != 0 and "secret not found" in p.stdout + p.stderr

    # 损坏 store:fail-closed,绝不返回部分明文。
    sj = store_dir / "secrets.json"
    sj.write_text('{"version":1,"entries":{"openai":{"salt":"zz","nonce":"zz","ct":"zz","created_at":"x"}}}')
    p = run_cli(sandbox, "secrets", "get", "openai")
    assert p.returncode != 0
    assert "corrupt" in p.stdout + p.stderr or "authentication failed" in p.stdout + p.stderr
    sj.unlink()

    # machine key 权限过宽:打开即失败(fail-closed;unix 专属语义)。
    if sys.platform != "win32":
        run_cli(sandbox, "secrets", "set", "again", input_text="v2")
        key = store_dir / ".machine.key"
        assert key.exists()
        key.chmod(0o644)
        p = run_cli(sandbox, "secrets", "get", "again")
        assert p.returncode != 0
        assert "insecure permissions" in p.stdout + p.stderr
        key.chmod(0o600)
        assert run_cli(sandbox, "secrets", "get", "again").stdout == "v2\n"
