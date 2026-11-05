"""能力覆盖层 · CLI 能力面:子命令的**行为**断言(参数矩阵仍在 tests/rust/*.sh)。

覆盖:
- C1 `serve` 的三个恢复入口(参数被接受 / 非法 id 报错明确);
- C2 `exec` 的 JSONL 输出契约(每行合法 JSON + 终态事件);
- C3 `session` 子命令(ls 能列出会话;show 对不存在的 id 报错);
- C4 `doctor` / `version` 的输出契约;
- C5 `session search` 跨会话全文搜索(命中 / 未命中 / `--json` 字段);
- C6 `exec` 的退出码映射(成功 0 / turn 失败 1 / plan mode 未产出计划 122)。

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
    """doctor / version 的输出契约。"""
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


def test_C6_exec_退出码映射(sandbox, mock_script) -> None:
    """`exec` 的退出码:成功 0;turn 失败 1;`--plan-mode` 未产出计划 122。

    依据:crates/runtime/zhuye-exec/src/lib.rs 的 `exit_code_for`
    (Error / 非 Success 的 TurnComplete / TurnAborted → 1)与
    `finish_plan_mode`(plan mode 未见 PlanReady → 122)。
    """
    require_binary()
    # 成功 → 0
    ok = run_cli(sandbox, "exec", "hi", env={"ZHUYE_MODEL": "mock"}, timeout=60)
    assert ok.returncode == 0, f"成功应退出 0,实得 {ok.returncode}:{ok.stderr[:400]}"

    # turn 失败(撞迭代上限 → TurnComplete.status != success)→ 1。
    # 脚本第 1 次调用发工具调用,引擎回到 model_call 时已超上限 →
    # status = MaxIterations。
    script = mock_script(['{"type":"tool_call","name":"echo","args":{"text":"x"}}'])
    failed = run_cli(
        sandbox,
        "exec",
        "hi",
        env={
            "ZHUYE_MODEL": "mock",
            "ZHUYE_MOCK_SCRIPT": str(script),
            "ZHUYE_MAX_ITERATIONS": "1",
        },
        timeout=60,
    )
    assert failed.returncode == 1, (
        f"turn 失败应退出 1,实得 {failed.returncode}:{failed.stdout[-400:]}"
    )

    # plan mode:mock 不会调 ExitPlanMode → 未见 PlanReady → 122。
    plan = run_cli(
        sandbox, "exec", "--plan-mode", "hi", env={"ZHUYE_MODEL": "mock"}, timeout=60
    )
    assert plan.returncode == 122, (
        f"plan mode 未产出计划应退出 122,实得 {plan.returncode}:{plan.stdout[-400:]}"
    )

    doc = run_cli(
        sandbox, "doctor", env={"ZHUYE_MODEL": "mock"}, timeout=60
    )
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
