"""能力覆盖层 · 外部 Harness 会话导入(zhuye-import 插件 + 独立二进制)。

覆盖:
- I1 Claude Code JSONL 直跑导入:session_meta 保留源 uuid、cwd → workspace、
  assistant content 时序折叠(tool_use/tool_result 配对)、ai-title → .name;
- I2 Codex rollout 直跑导入:developer/注入行过滤、arguments 解析、
  compacted → compaction、token_count;
- I3 ZCode SQLite 导入:schema 同构 fixture,前缀剥 uuid、model-only 过滤;
- I4 dry-run 不落盘 / 重复导入幂等 / --force 覆盖;
- I5 --from auto 嗅探混合目录 + --json 结构化输出;
- I6 导入后 session ls / show 可见可读;
- I7 插件全链路:install + serve --plugin → mock 模型调用 MCP 工具导入成功;
- I8 导入会话 exec --resume 续跑:历史进入上下文(session_meta 无随机 id、
  追加同一文件)。

运行:./tests/run_all.sh functional-layer,或
  ZHUYE_BIN=... python3 -m pytest tests/functional/test_import.py -v
"""

from __future__ import annotations

import json
import shutil
import sqlite3
import subprocess
from pathlib import Path
from typing import Any

import pytest

from wire import Sandbox, ZHUYE_BIN, rollout_files, rollout_records, run_cli

pytestmark = pytest.mark.usefixtures("sandbox")

# 会话导入二进制:与 zhuye 同目录的 sibling 产物。
IMPORT_BIN = Path(ZHUYE_BIN).parent / "zhuye-import"
# 仓库根(wire.ZHUYE_BIN = <root>/target/release/zhuye)。
REPO_ROOT = Path(ZHUYE_BIN).resolve().parents[2]


def _require_import_bin() -> None:
    if not IMPORT_BIN.is_file():
        pytest.skip(f"zhuye-import 二进制不存在:{IMPORT_BIN}(先 cargo build --release -p zhuye-import)")


def run_import(sandbox: Sandbox, *args: str, timeout: float = 60.0) -> subprocess.CompletedProcess[str]:
    """跑 `zhuye-import <args>`(env 走 sandbox 隔离 HOME)。"""
    return subprocess.run(
        [str(IMPORT_BIN), *args],
        cwd=str(sandbox.cwd),
        env=sandbox.env(),
        capture_output=True,
        text=True,
        timeout=timeout,
    )


# ── 源格式 fixture(按本机实测形状构造,内容与各 adapter 单测同源) ──

CLAUDE_FIXTURE = [
    '{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"第一问:查端口"},"uuid":"u1","timestamp":"2026-05-01T10:00:00Z","cwd":"/work/fixture","sessionId":"af9ee90f-0000-4000-8000-000000000011","version":"2.0.0"}',
    '{"type":"ai-title","aiTitle":"端口排查","sessionId":"af9ee90f-0000-4000-8000-000000000011"}',
    '{"parentUuid":"u1","isSidechain":false,"type":"assistant","message":{"role":"assistant","model":"test-model","content":[{"type":"text","text":"我来查。"},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"lsof -i :8080"}}],"usage":{"input_tokens":100,"cache_read_input_tokens":50,"output_tokens":20}},"timestamp":"2026-05-01T10:00:05Z","sessionId":"af9ee90f-0000-4000-8000-000000000011"}',
    '{"parentUuid":"u1","isSidechain":false,"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","is_error":false,"content":"LISTEN 0.0.0.0:8080"}]},"timestamp":"2026-05-01T10:00:08Z","sessionId":"af9ee90f-0000-4000-8000-000000000011"}',
    '{"parentUuid":"u1","isSidechain":false,"type":"user","isMeta":true,"message":{"role":"user","content":"Caveat: 注入应被过滤"},"uuid":"u2","timestamp":"2026-05-01T10:00:09Z","sessionId":"af9ee90f-0000-4000-8000-000000000011"}',
    '{"parentUuid":"u1","isSidechain":false,"type":"assistant","message":{"role":"assistant","model":"test-model","content":[{"type":"text","text":"8080 在监听。"}],"usage":{"input_tokens":120,"cache_read_input_tokens":50,"output_tokens":30}},"timestamp":"2026-05-01T10:00:10Z","sessionId":"af9ee90f-0000-4000-8000-000000000011"}',
    '{"parentUuid":"u1","isSidechain":true,"type":"user","message":{"role":"user","content":"子代理输入,默认剔除"},"uuid":"side-1","timestamp":"2026-05-01T10:00:11Z","sessionId":"af9ee90f-0000-4000-8000-000000000011"}',
]

CODEX_FIXTURE = [
    '{"timestamp":"2026-06-01T08:00:00.000Z","type":"session_meta","payload":{"id":"bf9ee90f-0000-7000-8000-000000000022","timestamp":"2026-06-01T08:00:00.000Z","cwd":"/work/cx-fixture","originator":"Codex CLI","cli_version":"0.166.0"}}',
    '{"timestamp":"2026-06-01T08:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>沙箱</environment_context>"}]}}',
    '{"timestamp":"2026-06-01T08:00:02.000Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<skills_instructions>技能</skills_instructions>"}]}}',
    '{"timestamp":"2026-06-01T08:00:03.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"跑个测试"}]}}',
    '{"timestamp":"2026-06-01T08:00:04.000Z","type":"turn_context","payload":{"cwd":"/work/cx-fixture","model":"openai/gpt-test","effort":"high"}}',
    '{"timestamp":"2026-06-01T08:00:05.000Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call_x1","arguments":"{\\"cmd\\":\\"cargo test\\"}"}}',
    '{"timestamp":"2026-06-01T08:00:06.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_x1","output":"test result: ok"}}',
    '{"timestamp":"2026-06-01T08:00:07.000Z","type":"token_usage_record","payload":{"turn_token_usage":{"input_tokens":300,"cached_input_tokens":200,"output_tokens":40},"usage":{"input_tokens":300,"output_tokens":40}}}',
    '{"timestamp":"2026-06-01T08:00:08.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"测试全绿。"}]}}',
    '{"timestamp":"2026-06-01T08:00:09.000Z","type":"compacted","payload":{"message":{"type":"message","role":"user","content":[{"type":"input_text","text":"<SUMMARY>此前:跑测试全绿</SUMMARY>"}]}}}',
]


def _write_fixture(sandbox: Sandbox, name: str, lines: list[str]) -> Path:
    p = sandbox.cwd / name
    p.write_text("\n".join(lines) + "\n")
    return p


def _records_of(sandbox: Sandbox, thread_id: str) -> list[dict[str, Any]]:
    """按 thread id 读单个会话文件的全部记录。"""
    matches = [f for f in rollout_files(sandbox.home) if f.stem == thread_id]
    assert matches, f"找不到 {thread_id} 的会话文件:{rollout_files(sandbox.home)}"
    out = []
    for line in matches[0].read_text().splitlines():
        if line.strip():
            out.append(json.loads(line))
    return out


# ── I1 Claude Code ───────────────────────────────────────────────────────


def test_I1_claude导入_元数据与时序折叠(sandbox: Sandbox) -> None:
    """Claude Code JSONL 导入:meta 保留源 uuid、cwd → workspace、
    tool_use/tool_result 折叠进 assistant content、ai-title 写 .name。

    依据:crates/integrations/zhuye-import/src/adapters/claude_code.rs。
    """
    _require_import_bin()
    src = _write_fixture(sandbox, "claude.jsonl", CLAUDE_FIXTURE)
    proc = run_import(sandbox, str(src), "--from", "auto")
    assert proc.returncode == 0, f"stderr={proc.stderr}"
    assert "imported" in proc.stdout, proc.stdout

    tid = "af9ee90f-0000-4000-8000-000000000011"
    recs = _records_of(sandbox, tid)
    # 首行 session_meta:索引契约 + 源 id/workspace 透传。
    assert recs[0]["type"] == "session_meta", recs[0]
    assert recs[0]["session_id"] == tid
    assert recs[0]["workspace"] == "/work/fixture"
    assert recs[0]["model"] == "test-model"
    # 消息形状:user 1 条 + assistant 1 条(isMeta 行被过滤,不开新轮)。
    msgs = [r for r in recs if r["type"] == "message"]
    assert [m["role"] for m in msgs] == ["user", "assistant"], msgs
    user_content = msgs[0]["content"]
    assert isinstance(user_content, list) and user_content[0]["type"] == "text"
    assert "第一问" in user_content[0]["text"]
    # assistant 时序折叠:text → tool_use → tool_result → text。
    blocks = msgs[1]["content"]
    assert [b["type"] for b in blocks] == ["text", "tool_use", "tool_result", "text"], blocks
    assert blocks[1]["id"] == "toolu_1"
    assert blocks[1]["args"]["command"] == "lsof -i :8080"
    assert blocks[2]["call_id"] == "toolu_1"
    assert blocks[2]["output"]["is_error"] is False
    assert "LISTEN" in blocks[2]["output"]["content"][0]["text"]
    # token_count 存在且带 provider 标记。
    tc = [r for r in recs if r["type"] == "token_count"]
    assert tc and tc[0]["provider"] == "claude-code"
    assert tc[0]["usage"]["input_tokens"] == 120
    # 落盘目录按源时间戳(2026-05-01)定日期。
    files = [f for f in rollout_files(sandbox.home) if f.stem == tid]
    assert files[0].parent.as_posix().endswith("2026/05/01"), files[0]
    # ai-title → .name 文件。
    name_file = sandbox.home / ".zhuye/sessions/_names" / f"{tid}.name"
    assert name_file.read_text() == "端口排查"


def test_I2_codex导入_过滤与降级(sandbox: Sandbox) -> None:
    """Codex rollout 导入:developer/注入 user 行过滤、arguments 解析为
    对象、compacted → compaction、token_count 挂轮。

    依据:crates/integrations/zhuye-import/src/adapters/codex.rs。
    """
    _require_import_bin()
    src = _write_fixture(sandbox, "codex.jsonl", CODEX_FIXTURE)
    proc = run_import(sandbox, str(src), "--from", "codex")
    assert proc.returncode == 0, f"stderr={proc.stderr}"

    tid = "bf9ee90f-0000-7000-8000-000000000022"
    recs = _records_of(sandbox, tid)
    assert recs[0]["type"] == "session_meta"
    assert recs[0]["model"] == "openai/gpt-test"
    assert recs[0]["workspace"] == "/work/cx-fixture"
    msgs = [r for r in recs if r["type"] == "message"]
    assert [m["role"] for m in msgs] == ["user", "assistant"], msgs
    assert "跑个测试" in msgs[0]["content"][0]["text"]
    blocks = msgs[1]["content"]
    assert [b["type"] for b in blocks] == ["tool_use", "tool_result", "text"], blocks
    assert blocks[0]["args"] == {"cmd": "cargo test"}, blocks[0]
    assert blocks[1]["output"]["content"][0]["text"] == "test result: ok"
    # compacted → compaction(位置在 turn 之后)。
    comp = [r for r in recs if r["type"] == "compaction"]
    assert comp and "此前:跑测试全绿" in comp[0]["summary"]
    assert comp[0]["strategy"] == "llm_summarize"
    tc = [r for r in recs if r["type"] == "token_count"]
    assert tc and tc[0]["provider"] == "codex"
    assert tc[0]["usage"]["cached_tokens"] == 200


# ── I3 ZCode ─────────────────────────────────────────────────────────────


def _make_zcode_db(sandbox: Sandbox) -> Path:
    """按逆向 schema(ZCode v2,2026-11)构造同构 fixture 库。"""
    db_dir = sandbox.home / ".zcode/cli/db"
    db_dir.mkdir(parents=True)
    db = db_dir / "db.sqlite"
    conn = sqlite3.connect(db)
    conn.executescript(
        """
        CREATE TABLE session (
          id text primary key, directory text not null, title text,
          time_created integer not null, time_updated integer not null,
          time_deleted integer);
        CREATE TABLE message (
          id text primary key, session_id text not null,
          time_created integer not null, data text not null, sequence integer);
        CREATE TABLE part (
          id text primary key, message_id text not null, session_id text,
          time_created integer not null, data text not null, sequence integer);
        CREATE TABLE turn_usage (
          session_id text not null, turn_id text not null, user_message_id text,
          input_tokens integer not null default 0,
          output_tokens integer not null default 0,
          cache_read_input_tokens integer not null default 0,
          primary key(session_id, turn_id));
        """
    )
    sid = "sess_cf9ee90f-0000-4000-8000-000000000033"
    conn.execute(
        "INSERT INTO session VALUES (?,?,?,?,?,NULL)",
        (sid, "/work/zc-fixture", "ZCode 会话", 1775000000000, 1775000600000),
    )
    conn.execute(
        "INSERT INTO message VALUES ('m1',?,1775000000100,'{\"role\":\"user\",\"time\":{\"created\":1775000000100}}',0)",
        (sid,),
    )
    conn.execute(
        "INSERT INTO message VALUES ('m2',?,1775000000200,'{\"role\":\"user\",\"time\":{\"created\":1775000000200},\"metadata\":{\"visibility\":\"model-only\",\"source\":\"plan_mode_exit\"}}',1)",
        (sid,),
    )
    conn.execute(
        "INSERT INTO message VALUES ('m3',?,1775000000300,'{\"role\":\"assistant\",\"time\":{\"created\":1775000000300},\"modelId\":\"glm-test\",\"providerId\":\"p\"}',2)",
        (sid,),
    )
    conn.execute(
        "INSERT INTO part VALUES ('p1','m1','s',1,'{\"type\":\"text\",\"text\":\"# Environment\\n注入\"}',0)"
    )
    conn.execute(
        "INSERT INTO part VALUES ('p2','m1','s',2,'{\"type\":\"text\",\"text\":\"ZCode 的问题\"}',1)"
    )
    conn.execute(
        "INSERT INTO part VALUES ('p3','m2','s',3,'{\"type\":\"text\",\"text\":\"元消息\"}',0)"
    )
    conn.execute(
        "INSERT INTO part VALUES ('p4','m3','s',4,'{\"type\":\"reasoning\",\"text\":\"想一想\"}',0)"
    )
    conn.execute(
        "INSERT INTO part VALUES ('p5','m3','s',5,'{\"type\":\"tool\",\"callID\":\"call_z\",\"tool\":\"Bash\",\"state\":{\"status\":\"completed\",\"input\":{\"command\":\"ls\"},\"output\":\"a.rs\"}}',1)"
    )
    conn.execute(
        "INSERT INTO part VALUES ('p6','m3','s',6,'{\"type\":\"text\",\"text\":\"答完了\"}',2)"
    )
    conn.execute(
        "INSERT INTO turn_usage VALUES (?,?,?,500,60,300)",
        (sid, "turn_1", "m1"),
    )
    conn.commit()
    conn.close()
    return db


def test_I3_zcode导入_sqlite来源(sandbox: Sandbox) -> None:
    """ZCode SQLite 导入:--all --from zcode 走 discover;前缀剥 uuid、
    model-only 与注入块过滤、tool 配对、turn_usage 挂轮。

    依据:crates/integrations/zhuye-import/src/adapters/zcode.rs。
    """
    _require_import_bin()
    _make_zcode_db(sandbox)
    proc = run_import(sandbox, "--all", "--from", "zcode", "--json")
    assert proc.returncode == 0, f"stderr={proc.stderr}"
    lines = [json.loads(l) for l in proc.stdout.splitlines() if l.strip()]
    imported = [l for l in lines if l["status"] == "imported"]
    assert len(imported) == 1, proc.stdout

    tid = "cf9ee90f-0000-4000-8000-000000000033"
    assert imported[0]["thread_id"] == tid
    recs = _records_of(sandbox, tid)
    assert recs[0]["type"] == "session_meta"
    assert recs[0]["workspace"] == "/work/zc-fixture"
    assert recs[0]["model"] == "glm-test"
    msgs = [r for r in recs if r["type"] == "message"]
    assert [m["role"] for m in msgs] == ["user", "assistant"], msgs
    assert msgs[0]["content"][0]["text"] == "ZCode 的问题"
    blocks = msgs[1]["content"]
    assert [b["type"] for b in blocks] == ["tool_use", "tool_result", "text"], blocks
    assert blocks[0]["name"] == "Bash"
    assert blocks[0]["args"] == {"command": "ls"}
    assert blocks[1]["output"]["content"][0]["text"] == "a.rs"
    tc = [r for r in recs if r["type"] == "token_count"]
    assert tc and tc[0]["usage"]["input_tokens"] == 500


# ── I4 幂等 / dry-run / force ────────────────────────────────────────────


def test_I4_dry_run_幂等与force(sandbox: Sandbox) -> None:
    """--dry-run 不落盘;重复导入 skipped;--force 覆盖且文件仍可解析。

    依据:crates/integrations/zhuye-import/src/write.rs write_session。
    """
    _require_import_bin()
    src = _write_fixture(sandbox, "c.jsonl", CLAUDE_FIXTURE)
    tid = "af9ee90f-0000-4000-8000-000000000011"

    dry = run_import(sandbox, str(src), "--dry-run")
    assert dry.returncode == 0 and "dry-run" in dry.stdout, dry.stdout
    assert rollout_files(sandbox.home) == [], "dry-run 不得落盘"

    first = run_import(sandbox, str(src))
    assert first.returncode == 0
    n1 = len(_records_of(sandbox, tid))

    again = run_import(sandbox, str(src))
    assert again.returncode == 0
    assert "已存在" in again.stdout or "failed" in again.stdout, again.stdout
    assert len(_records_of(sandbox, tid)) == n1, "重复导入不得追加"

    force = run_import(sandbox, str(src), "--force")
    assert force.returncode == 0
    recs = _records_of(sandbox, tid)
    assert len(recs) == n1, "force 覆盖应为等量记录"
    assert recs[0]["type"] == "session_meta"


# ── I5 auto 嗅探 + --json ────────────────────────────────────────────────


def test_I5_auto嗅探混合目录(sandbox: Sandbox) -> None:
    """混合目录一次导入:claude/codex 各自被嗅探正确;--json 逐源一行。

    依据:crates/integrations/zhuye-import/src/adapter.rs sniff。
    """
    _require_import_bin()
    d = sandbox.cwd / "mixed"
    d.mkdir()
    (d / "a.jsonl").write_text("\n".join(CLAUDE_FIXTURE) + "\n")
    (d / "b.jsonl").write_text("\n".join(CODEX_FIXTURE) + "\n")

    proc = run_import(sandbox, str(d), "--from", "auto", "--json")
    assert proc.returncode == 0, f"stderr={proc.stderr}"
    lines = [json.loads(l) for l in proc.stdout.splitlines() if l.strip()]
    imported = sorted(l["thread_id"] for l in lines if l["status"] == "imported")
    assert imported == [
        "af9ee90f-0000-4000-8000-000000000011",
        "bf9ee90f-0000-7000-8000-000000000022",
    ], lines


# ── I6 session ls / show 可见 ────────────────────────────────────────────


def test_I6_导入会话对原生CLI可见(sandbox: Sandbox) -> None:
    """导入产物是普通会话:`session ls` 列出、`session show` 可读、
    标题显示 ai-title。

    依据:crates/resources/zhuye-rollout/src/index/mod.rs list_sessions。
    """
    _require_import_bin()
    src = _write_fixture(sandbox, "c.jsonl", CLAUDE_FIXTURE)
    run_import(sandbox, str(src))
    tid = "af9ee90f-0000-4000-8000-000000000011"

    ls = run_cli(sandbox, "session", "ls")
    assert ls.returncode == 0, ls.stderr
    assert tid in ls.stdout, ls.stdout

    show = run_cli(sandbox, "session", "show", tid)
    assert show.returncode == 0, show.stderr
    assert "第一问" in show.stdout, show.stdout
    # 自定义名(.name 文件)由导入时的 ai-title 写入。
    name_file = sandbox.home / ".zhuye/sessions/_names" / f"{tid}.name"
    assert name_file.read_text() == "端口排查"


# ── I7 插件全链路 ────────────────────────────────────────────────────────


def test_I7_插件全链路_MCP工具导入(sandbox: Sandbox, mock_script, serve_session) -> None:
    """install + serve --plugin → mock 模型调用
    mcp__plugin:harness-import:importer__import_session → 导入成功。

    依据:crates/resources/zhuye-plugin/src/loader.rs register(MCP 工具
    命名)、crates/integrations/zhuye-import/src/mcp_server.rs。
    """
    _require_import_bin()
    plugin_dir = REPO_ROOT / "plugins" / "harness-import"
    if not plugin_dir.is_dir():
        pytest.skip("plugins/harness-import 不存在")
    # ${PLUGIN_ROOT}/bin/zhuye-import 由测试装配(bin/ 内容不入库)。
    bin_dir = plugin_dir / "bin"
    bin_dir.mkdir(exist_ok=True)
    shutil.copy2(IMPORT_BIN, bin_dir / "zhuye-import")

    inst = run_cli(sandbox, "plugin", "install", str(plugin_dir), "--scope", "user")
    assert inst.returncode == 0, inst.stderr + inst.stdout

    src = _write_fixture(sandbox, "claude.jsonl", CLAUDE_FIXTURE)
    # 工具全名:plugin_id 含 marketplace 后缀(@inline),见
    # zhuye-mcp/src/manager.rs tool_descriptor_from_rmcp(mcp__{scoped}__{tool})。
    tool_name = "mcp__plugin:harness-import@inline:importer__import_session"
    script = mock_script(
        [
            json.dumps(
                {
                    "type": "tool_call",
                    "name": tool_name,
                    "args": {"path": str(src), "source_kind": "auto"},
                }
            ),
            '{"type":"text","text":"导入完成"}',
        ]
    )
    s = serve_session(args=["--plugin", "harness-import@inline"], env=sandbox.mock_env(script))
    # ServeSession.submit(op) 发送并收集到终态,直接返回事件流。
    events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "帮我导入这个会话"}]})
    types = [e["msg"]["type"] for e in events]
    assert "turn_complete" in types, types

    end = next(e for e in events if e["msg"]["type"] == "tool_call_end")
    assert end["msg"]["is_error"] is False, f"MCP 工具失败:{end['msg']}"
    out_text = "".join(
        c.get("text", "")
        for c in end["msg"].get("output", {}).get("content", [])
        if isinstance(c, dict)
    )
    assert "imported" in out_text, out_text

    tid = "af9ee90f-0000-4000-8000-000000000011"
    recs = _records_of(sandbox, tid)
    assert recs[0]["type"] == "session_meta" and recs[0]["session_id"] == tid


# ── I8 导入会话可续跑 ────────────────────────────────────────────────────


def test_I8_导入会话_resume_续跑(sandbox: Sandbox, mock_script) -> None:
    """导入后 `exec --resume <id>` 续跑:追加同一文件,且文件内全部
    session_meta 的 session_id 一致(无随机 id 二次写入 —— resume 回填
    session_id 的回归断言)。

    依据:crates/runtime/zhuye-exec/src/headless.rs bootstrap_resumed
    (with_session_id)、crates/runtime/zhuye-core/src/submission_loop.rs。
    """
    _require_import_bin()
    src = _write_fixture(sandbox, "c.jsonl", CLAUDE_FIXTURE)
    run_import(sandbox, str(src))
    tid = "af9ee90f-0000-4000-8000-000000000011"
    path = next(f for f in rollout_files(sandbox.home) if f.stem == tid)
    before = len(_records_of(sandbox, tid))

    script = mock_script(['{"type":"text","text":"续跑回复"}'])
    proc = run_cli(sandbox, "exec", "--resume", tid, env=sandbox.mock_env(script), input_text="")
    assert proc.returncode == 0, proc.stderr + proc.stdout

    recs = _records_of(sandbox, tid)
    assert len(recs) > before, "resume 应在同一文件追加"
    metas = [r for r in recs if r["type"] == "session_meta"]
    assert all(m["session_id"] == tid for m in metas), [m["session_id"] for m in metas]
    # 续跑 turn 的回复落盘(assistant 追加)。
    assert recs[-1]["type"] == "message" and recs[-1]["role"] == "assistant"
    assert "续跑回复" in json.dumps(recs[-1]["content"], ensure_ascii=False)


# ── I9 三个 CLI 开关端到端 ───────────────────────────────────────────────

# 带 thinking 块的 Claude 会话(默认丢弃思考链,开关保留)。
CLAUDE_THINKING_FIXTURE = [
    '{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"问个问题"},"uuid":"u1","timestamp":"2026-05-01T10:00:00Z","cwd":"/w","sessionId":"df9ee90f-0000-4000-8000-000000000044"}',
    '{"parentUuid":"u1","isSidechain":false,"type":"assistant","message":{"role":"assistant","model":"m","content":[{"type":"thinking","thinking":"内部推演过程"},{"type":"text","text":"可见答复"}]},"timestamp":"2026-05-01T10:00:05Z","sessionId":"df9ee90f-0000-4000-8000-000000000044"}',
]


def test_I9a_include_thinking开关(sandbox: Sandbox) -> None:
    """--include-thinking:thinking 块折叠为 <thinking> 文本进 assistant content;
    默认(不带)则丢弃。

    依据:crates/integrations/zhuye-import/src/convert.rs append_turn。
    """
    _require_import_bin()
    src = _write_fixture(sandbox, "t.jsonl", CLAUDE_THINKING_FIXTURE)
    tid = "df9ee90f-0000-4000-8000-000000000044"

    run_import(sandbox, str(src))
    off = json.dumps(_records_of(sandbox, tid), ensure_ascii=False)
    assert "内部推演过程" not in off, "默认应丢弃 thinking"
    assert "可见答复" in off

    run_import(sandbox, str(src), "--include-thinking", "--force")
    on = json.dumps(_records_of(sandbox, tid), ensure_ascii=False)
    assert "<thinking>内部推演过程</thinking>" in on, on
    assert "可见答复" in on, "正文不得被开关影响"


def test_I9b_keep_injected开关(sandbox: Sandbox) -> None:
    """--keep-injected:Codex 注入行(environment_context / developer)默认
    过滤,开关打开后作为 user 上下文保留。

    依据:crates/integrations/zhuye-import/src/adapters/codex.rs is_injected。
    """
    _require_import_bin()
    src = _write_fixture(sandbox, "x.jsonl", CODEX_FIXTURE)
    tid = "bf9ee90f-0000-7000-8000-000000000022"

    run_import(sandbox, str(src))
    off = json.dumps(_records_of(sandbox, tid), ensure_ascii=False)
    assert "沙箱" not in off, "默认应过滤注入 user 行"
    assert "技能" not in off, "默认应过滤 developer 行"

    run_import(sandbox, str(src), "--keep-injected", "--force")
    on = json.dumps(_records_of(sandbox, tid), ensure_ascii=False)
    assert "沙箱" in on, on
    assert "技能" in on, on


def test_I9c_include_sidechain开关(sandbox: Sandbox) -> None:
    """--include-sidechain:Claude isSidechain 子代理行默认跳过,开关打开
    后作为独立 turn 导入。

    依据:crates/integrations/zhuye-import/src/adapters/claude_code.rs parse_lines。
    """
    _require_import_bin()
    src = _write_fixture(sandbox, "s.jsonl", CLAUDE_FIXTURE)
    tid = "af9ee90f-0000-4000-8000-000000000011"

    run_import(sandbox, str(src))
    off = json.dumps(_records_of(sandbox, tid), ensure_ascii=False)
    assert "子代理输入" not in off, "默认应跳过 sidechain"

    run_import(sandbox, str(src), "--include-sidechain", "--force")
    on = json.dumps(_records_of(sandbox, tid), ensure_ascii=False)
    assert "子代理输入" in on, on


# ── I10 --all discover(claude-code / codex 目录扫描) ─────────────────────


def test_I10_all_discover_claude与codex(sandbox: Sandbox) -> None:
    """--all --from claude-code / codex 扫描本机默认安装位置
    (~/.claude/projects/*、~/.codex/sessions/**),各自导入成功。

    依据:crates/integrations/zhuye-import/src/adapters/{claude_code,codex}.rs discover。
    """
    _require_import_bin()
    # Claude:~/.claude/projects/<escaped-cwd>/<uuid>.jsonl
    proj = sandbox.home / ".claude/projects/-w-proj"
    proj.mkdir(parents=True)
    (proj / "af9ee90f-0000-4000-8000-000000000011.jsonl").write_text(
        "\n".join(CLAUDE_FIXTURE) + "\n"
    )
    # Codex:~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl(递归收集)
    cx = sandbox.home / ".codex/sessions/2026/06/01"
    cx.mkdir(parents=True)
    (cx / "rollout-2026-06-01T08-00-00-bf9ee90f-0000-7000-8000-000000000022.jsonl").write_text(
        "\n".join(CODEX_FIXTURE) + "\n"
    )

    p1 = run_import(sandbox, "--all", "--from", "claude-code", "--json")
    assert p1.returncode == 0, p1.stderr
    lines = [json.loads(l) for l in p1.stdout.splitlines() if l.strip()]
    imported = [l for l in lines if l["status"] == "imported"]
    assert [l["thread_id"] for l in imported] == ["af9ee90f-0000-4000-8000-000000000011"], lines
    assert "af9ee90f-0000-4000-8000-000000000011" in run_cli(sandbox, "session", "ls").stdout

    p2 = run_import(sandbox, "--all", "--from", "codex", "--json")
    assert p2.returncode == 0, p2.stderr
    lines2 = [json.loads(l) for l in p2.stdout.splitlines() if l.strip()]
    imported2 = [l for l in lines2 if l["status"] == "imported"]
    assert [l["thread_id"] for l in imported2] == ["bf9ee90f-0000-7000-8000-000000000022"], lines2

    # 重复 --all 幂等:第二次应全部 skipped(退出码仍为 0)。
    p3 = run_import(sandbox, "--all", "--from", "claude-code", "--json")
    assert p3.returncode == 0, p3.stderr
    lines3 = [json.loads(l) for l in p3.stdout.splitlines() if l.strip()]
    assert all(l["status"] == "skipped" for l in lines3), lines3


# ── I11 MCP list_importable + 批量失败退出码 ─────────────────────────────


def _mcp_call(bin_path: Path, home: Path, method: str, params: dict[str, Any]) -> dict[str, Any]:
    """直连 `zhuye-import mcp` 发一个请求读一个响应(stdin 关闭即退出)。"""
    proc = subprocess.run(
        [str(bin_path), "mcp"],
        input=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}) + "\n",
        capture_output=True,
        text=True,
        env=sandbox_env(home),
        timeout=30,
    )
    lines = [l for l in proc.stdout.splitlines() if l.strip()]
    assert lines, f"MCP server 无响应:{proc.stderr[-300:]}"
    return json.loads(lines[0])


def sandbox_env(home: Path) -> dict[str, Any]:
    import os

    env = dict(os.environ)
    env["HOME"] = str(home)
    return env


def test_I11a_mcp_list_importable工具(sandbox: Sandbox) -> None:
    """MCP list_importable 成功路径:扫描 fixture HOME 的 Claude 目录,
    返回来源清单。

    依据:crates/integrations/zhuye-import/src/mcp_server.rs tool_list_importable。
    """
    _require_import_bin()
    proj = sandbox.home / ".claude/projects/-w-proj"
    proj.mkdir(parents=True)
    (proj / "af9ee90f-0000-4000-8000-000000000011.jsonl").write_text(
        "\n".join(CLAUDE_FIXTURE) + "\n"
    )
    resp = _mcp_call(
        IMPORT_BIN,
        sandbox.home,
        "tools/call",
        {"name": "list_importable", "arguments": {"source_kind": "claude-code"}},
    )
    assert resp["result"]["isError"] is False, resp
    text = resp["result"]["content"][0]["text"]
    assert "发现 1 个来源" in text, text
    assert "af9ee90f-0000-4000-8000-000000000011.jsonl" in text, text


def test_I11b_批量失败退出码非零(sandbox: Sandbox) -> None:
    """全部来源失败(格式无法识别)→ 退出码 1(脚本可感知);部分成功 + 部分
    失败 → 退出码 0(批量语义)。

    依据:crates/integrations/zhuye-import/src/bin/zhuye-import.rs run_import。
    """
    _require_import_bin()
    bad = sandbox.cwd / "bad.jsonl"
    bad.write_text('{"hello":1}\n')

    proc = run_import(sandbox, str(bad))
    assert proc.returncode == 1, f"全失败应非零退出:{proc.stdout}"

    # 混入一个好来源 → 部分失败,退出码 0 且失败行可见。
    good = _write_fixture(sandbox, "good.jsonl", CLAUDE_FIXTURE)
    proc2 = run_import(sandbox, str(bad), str(good))
    assert proc2.returncode == 0, proc2.stdout
    assert "failed" in proc2.stdout and "imported" in proc2.stdout, proc2.stdout


def test_I12_KMS插件全链路_MCP工具读写(sandbox: Sandbox, mock_script, serve_session) -> None:
    """install + serve --plugin → mock 模型调用
    mcp__plugin:kms@inline:kms__kms_save_page / kms_get_page → 落盘到
    `~/.zhuye/kms/`。与 I7(harness-import)同范式的第二个二进制插件。

    依据:crates/abilities/zhuye-kms(KnowledgeManager)+
    crates/abilities/zhuye-kms/src/bin/zhuye-kms-server.rs(MCP server)+
    plugins/kms/。
    """
    kms_bin = Path(ZHUYE_BIN).parent / "zhuye-kms-server"
    if not kms_bin.is_file():
        pytest.skip(f"zhuye-kms-server 二进制不存在:{kms_bin}(先 ./scripts/build_kms_plugin.sh)")
    plugin_dir = REPO_ROOT / "plugins" / "kms"
    if not plugin_dir.is_dir():
        pytest.skip("plugins/kms 不存在")
    bin_dir = plugin_dir / "bin"
    bin_dir.mkdir(exist_ok=True)
    shutil.copy2(kms_bin, bin_dir / "zhuye-kms-server")

    inst = run_cli(sandbox, "plugin", "install", str(plugin_dir), "--scope", "user")
    assert inst.returncode == 0, inst.stderr + inst.stdout

    tool_create = "mcp__plugin:kms@inline:kms__kms_create_wiki"
    tool_save = "mcp__plugin:kms@inline:kms__kms_save_page"
    tool_get = "mcp__plugin:kms@inline:kms__kms_get_page"
    script = mock_script([
        json.dumps({
            "type": "tool_call",
            "name": tool_create,
            "args": {"name": "deploy", "description": "发布运维"},
        }),
        json.dumps({
            "type": "tool_call",
            "name": tool_save,
            "args": {"wiki": "deploy", "page": "runbook",
                     "content": "# 发布流程\n先跑 functional 套件", "tags": ["ops"]},
        }),
        json.dumps({
            "type": "tool_call",
            "name": tool_get,
            "args": {"wiki": "deploy", "page": "runbook"},
        }),
        '{"type":"text","text":"已记录"}',
    ])
    s = serve_session(args=["--plugin", "kms@inline"], env=sandbox.mock_env(script))
    events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "记一下发布流程"}]})
    types = [e["msg"]["type"] for e in events]
    assert "turn_complete" in types, types

    # 1) save 工具成功。
    ends = [e for e in events if e["msg"]["type"] == "tool_call_end"]
    assert len(ends) == 3, f"应有三个 tool_call_end:{types}"
    assert ends[0]["msg"]["is_error"] is False, f"create 失败:{ends[0]['msg']}"
    assert ends[1]["msg"]["is_error"] is False, f"save 失败:{ends[1]['msg']}"
    save_text = "".join(
        c.get("text", "") for c in ends[1]["msg"]["output"]["content"] if isinstance(c, dict)
    )
    assert "runbook" in save_text, save_text

    # 2) get 工具读回原文(落盘到 ~/.zhuye/kms/)。
    assert ends[2]["msg"]["is_error"] is False, f"get 失败:{ends[2]['msg']}"
    get_text = "".join(
        c.get("text", "") for c in ends[2]["msg"]["output"]["content"] if isinstance(c, dict)
    )
    assert "先跑 functional 套件" in get_text, get_text

    # 3) 磁盘上有真实文件(不经 server 视角,直接验证存储)。
    page = sandbox.home / ".zhuye/kms/deploy/pages/runbook.md"
    assert page.exists(), f"页面未落盘:{page}"
    assert "发布流程" in page.read_text()


def test_I13_computer_use插件_降级不panic(sandbox: Sandbox, mock_script, serve_session) -> None:
    """computer-use 插件(headless 构建无 real feature)→ 工具调用返回
    Unavailable(isError 文案),**不 panic**、会话存活。

    依据:crates/integrations/zhuye-computer-use/src/backend.rs(优雅降级)
    + plugins/computer-use/。CI 无屏幕,真机路径(manual):构建 real feature
    后本地验证截屏。
    """
    cu_bin = Path(ZHUYE_BIN).parent / "zhuye-computer-use-server"
    if not cu_bin.is_file():
        pytest.skip(f"zhuye-computer-use-server 二进制不存在:{cu_bin}")
    plugin_dir = REPO_ROOT / "plugins" / "computer-use"
    if not plugin_dir.is_dir():
        pytest.skip("plugins/computer-use 不存在")
    bin_dir = plugin_dir / "bin"
    bin_dir.mkdir(exist_ok=True)
    shutil.copy2(cu_bin, bin_dir / "zhuye-computer-use-server")

    inst = run_cli(sandbox, "plugin", "install", str(plugin_dir), "--scope", "user")
    assert inst.returncode == 0, inst.stderr + inst.stdout

    tool = "mcp__plugin:computer-use@inline:computer__computer_screenshot"
    script = mock_script([
        json.dumps({"type": "tool_call", "name": tool, "args": {}}),
        '{"type":"text","text":"done"}',
    ])
    s = serve_session(args=["--plugin", "computer-use@inline"], env=sandbox.mock_env(script))
    events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "截个屏"}]})
    types = [e["msg"]["type"] for e in events]
    assert "turn_complete" in types, types

    end = next(e for e in events if e["msg"]["type"] == "tool_call_end")
    # headless 二进制(无 real feature)→ isError + Unavailable 文案,
    # 而非 crash / 工具不存在。
    out_text = "".join(
        c.get("text", "") for c in end["msg"]["output"]["content"] if isinstance(c, dict)
    )
    if end["msg"]["is_error"]:
        assert "不可用" in out_text or "Unavailable" in out_text or "rebuild" in out_text, (
            f"降级文案异常:{out_text}"
        )
    else:
        # 真机(real feature 构建且授权)→ 成功元信息。
        assert "screenshot captured" in out_text, out_text

    # 会话存活:turn 正常收尾(terminal 是 turn_complete,submission_closed
    # 是 wire 收尾标记,不算 turn 事件)。
    turn_types = [t for t in types if t != "submission_closed"]
    assert turn_types[-1] == "turn_complete", types


def test_I14_shell插件_常驻会话增读与kill(sandbox: Sandbox, mock_script, serve_session) -> None:
    """shell 插件:`shell_run` 起后台会话(立即返回)→ `shell_read` 增量
    取输出 → `shell_kill` 清理。与一次性 bash 工具互补的长任务面。

    依据:crates/integrations/zhuye-shell(ShellSessions 增量游标 / 锁不跨
    await / env_clear)+ plugins/shell/。
    """
    sh_bin = Path(ZHUYE_BIN).parent / "zhuye-shell-server"
    if not sh_bin.is_file():
        pytest.skip(f"zhuye-shell-server 二进制不存在:{sh_bin}")
    plugin_dir = REPO_ROOT / "plugins" / "shell"
    if not plugin_dir.is_dir():
        pytest.skip("plugins/shell 不存在")
    bin_dir = plugin_dir / "bin"
    bin_dir.mkdir(exist_ok=True)
    shutil.copy2(sh_bin, bin_dir / "zhuye-shell-server")

    inst = run_cli(sandbox, "plugin", "install", str(plugin_dir), "--scope", "user")
    assert inst.returncode == 0, inst.stderr + inst.stdout

    tool_run = "mcp__plugin:shell@inline:shell__shell_run"
    tool_read = "mcp__plugin:shell@inline:shell__shell_read"
    tool_kill = "mcp__plugin:shell@inline:shell__shell_kill"
    # 连续 tool_call 行(中间不放 text —— mock 的纯文本行会结束 turn)。
    # 单次 read(run 与 read 之间无引擎间隔,输出可能尚未写入缓冲 ——
    # 增量语义由 zhuye-shell crate 单测锁定;此处验插件链路与会话生命周期)。
    script = mock_script([
        json.dumps({
            "type": "tool_call",
            "name": tool_run,
            "args": {"command": "echo shell-plugin-live; sleep 30"},
        }),
        json.dumps({
            "type": "tool_call",
            "name": tool_read,
            "args": {"session_id": "sh-00000000"},
        }),
        json.dumps({
            "type": "tool_call",
            "name": tool_kill,
            "args": {"session_id": "sh-00000000"},
        }),
        '{"type":"text","text":"all done"}',
    ])
    s = serve_session(args=["--plugin", "shell@inline"], env=sandbox.mock_env(script))
    events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "起个后台任务"}]})
    types = [e["msg"]["type"] for e in events]
    assert "turn_complete" in [t for t in types if t != "submission_closed"], types

    ends = [e for e in events if e["msg"]["type"] == "tool_call_end"]
    assert len(ends) == 3, f"应有三个 tool_call_end:{types}"
    # 1) run:立即返回 session_id。
    assert ends[0]["msg"]["is_error"] is False, f"run 失败:{ends[0]['msg']}"
    run_text = _text_of(ends[0]["msg"])
    assert "sh-00000000" in run_text, run_text
    # 2) read:不 error 且带状态行(running / exited 或增量输出)。
    assert ends[1]["msg"]["is_error"] is False, f"read 失败:{ends[1]['msg']}"
    read_text = _text_of(ends[1]["msg"])
    assert "--- " in read_text, f"read 应带状态分段:{read_text}"
    # 3) kill:成功。
    assert ends[2]["msg"]["is_error"] is False, f"kill 失败:{ends[2]['msg']}"
    assert "killed" in _text_of(ends[2]["msg"])


def _text_of(msg: dict) -> str:
    return "".join(
        c.get("text", "") for c in msg.get("output", {}).get("content", []) if isinstance(c, dict)
    )


def test_I15_git_tools插件_探测与checkpoint(sandbox: Sandbox, mock_script, serve_session) -> None:
    """git-tools 插件:install → serve --plugin → `git_branch_summary`
    (真 git 仓库:分支名 + 无 PR)→ `git_checkpoint_create` → tag 落盘
    (`git tag` 可见 zhuye-cp/*)。

    依据:crates/resources/zhuye-git(branch 探测 + checkpoint)+
    crates/integrations/zhuye-git-tools + plugins/git-tools/。
    """
    gt_bin = Path(ZHUYE_BIN).parent / "zhuye-git-tools-server"
    if not gt_bin.is_file():
        pytest.skip(f"zhuye-git-tools-server 二进制不存在:{gt_bin}")
    plugin_dir = REPO_ROOT / "plugins" / "git-tools"
    if not plugin_dir.is_dir():
        pytest.skip("plugins/git-tools 不存在")
    bin_dir = plugin_dir / "bin"
    bin_dir.mkdir(exist_ok=True)
    shutil.copy2(gt_bin, bin_dir / "zhuye-git-tools-server")
    hook_bin = Path(ZHUYE_BIN).parent / "zhuye-cp-hook"
    if hook_bin.is_file():
        shutil.copy2(hook_bin, bin_dir / "zhuye-cp-hook")

    # 真 git 仓库(git 缺失则整测跳过)。
    import subprocess
    repo = sandbox.cwd / "repo"
    repo.mkdir(exist_ok=True)
    git = lambda *a, **kw: subprocess.run(["git", *a], cwd=repo, capture_output=True, text=True, **kw)
    if git("init", "-q").returncode != 0:
        pytest.skip("git 不可用")
    git("config", "user.email", "t@zhuye.local")
    git("config", "user.name", "zhuye-test")
    (repo / "a.txt").write_text("v1\n")
    git("add", "-A")
    git("commit", "-q", "-m", "init")

    inst = run_cli(sandbox, "plugin", "install", str(plugin_dir), "--scope", "user")
    assert inst.returncode == 0, inst.stderr + inst.stdout

    tool_summary = "mcp__plugin:git-tools@inline:git__git_branch_summary"
    tool_cp = "mcp__plugin:git-tools@inline:git__git_checkpoint_create"
    script = mock_script([
        json.dumps({
            "type": "tool_call",
            "name": tool_summary,
            "args": {"cwd": str(repo)},
        }),
        json.dumps({
            "type": "tool_call",
            "name": tool_cp,
            "args": {"cwd": str(repo), "label": "I15 snapshot"},
        }),
        '{"type":"text","text":"done"}',
    ])
    s = serve_session(args=["--plugin", "git-tools@inline"], env=sandbox.mock_env(script))
    events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "看看仓库"}]})
    types = [e["msg"]["type"] for e in events]
    assert "turn_complete" in [t for t in types if t != "submission_closed"], types

    ends = [e for e in events if e["msg"]["type"] == "tool_call_end"]
    assert len(ends) == 2, f"应有二个 tool_call_end:{types}"
    # 1) 探测:分支名(master/main)+ 无 PR。
    assert ends[0]["msg"]["is_error"] is False, f"summary 失败:{ends[0]['msg']}"
    summary = _text_of(ends[0]["msg"])
    assert "branch: " in summary, summary
    assert "open PR:" in summary, summary
    # 2) checkpoint:无改动 → no-changes(初始提交后工作区干净)。
    assert ends[1]["msg"]["is_error"] is False, f"checkpoint 失败:{ends[1]['msg']}"
    cp_text = _text_of(ends[1]["msg"])
    assert "no-changes" in cp_text or "checkpoint" in cp_text, cp_text

    # 3) checkpoint 的落盘验证:写点改动 → 让 server 直接建快照 → tag 存在。
    (repo / "b.txt").write_text("v2\n")
    req1 = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
    req2 = json.dumps({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "git_checkpoint_create",
                   "arguments": {"cwd": str(repo), "label": "I15"}},
    })
    cp2 = subprocess.run(
        [str(gt_bin)], input=req1 + "\n" + req2 + "\n",
        capture_output=True, text=True, timeout=30,
    )
    assert '"isError":false' in cp2.stdout, cp2.stdout + cp2.stderr
    tags = git("tag", "--list").stdout
    assert "zhuye-cp/" in tags, f"checkpoint tag 未打:{tags}"
