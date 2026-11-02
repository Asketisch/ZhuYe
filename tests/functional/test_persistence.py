"""能力覆盖层 · 持久化面:会话 JSONL 的记录集、轨迹与物理截断。

直接驱动 `zhuye serve` 原始 JSONL wire(不经过任何 SDK)。

覆盖:
- P1 rollout 记录集(session_meta 首行 + message);
- P2 权限模式轨迹(from/to/at);
- P3 rewind 物理截断(记录被丢弃 + `.rewind-<ts>.bak` 备份);
- P4 会话索引(`zhuye session ls` 能列出该会话);
- P5 旧日期会话通过双 ID、继续与序号入口恢复后续写原文件。

运行:tests/run_all.sh functional,或
  ZHUYE_BIN=... python3 -m pytest tests/functional/test_persistence.py -v
"""

from __future__ import annotations

import json
import time
from datetime import datetime
from typing import Any

import pytest

from wire import (
    DRAIN_TIMEOUT,
    require_binary,
    rollout_files,
    rollout_records,
    rollout_thread_id,
    run_cli,
)

pytestmark = pytest.mark.usefixtures("sandbox")


def test_P1_rollout记录集(serve_session, mock_script, sandbox) -> None:
    """会话 JSONL:首行 `session_meta` + 每轮的 user/assistant `message`。

    依据:crates/resources/zhuye-rollout/src/writer.rs(首行 = SessionMeta
    是索引契约)。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"hello"}',
            '{"type":"text","text":"world"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]},
            timeout=DRAIN_TIMEOUT,
        )
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "第二轮"}]},
            timeout=DRAIN_TIMEOUT,
        )
    time.sleep(0.8)
    files = rollout_files(sandbox.home)
    assert files, f"未落盘会话文件:{sandbox.home}/.zhuye/sessions"
    lines = [json_line for json_line in files[-1].read_text().splitlines() if json_line.strip()]
    recs = [__import__("json").loads(json_line) for json_line in lines]
    assert recs[0].get("type") == "session_meta", f"首行应为 session_meta:{recs[0]}"
    assert recs[0].get("session_id"), "session_meta 缺 session_id"
    msgs = [r for r in recs if r.get("type") == "message"]
    assert len(msgs) >= 4, f"两轮应有 ≥4 条 message:{len(msgs)}"
    roles = {r.get("role") for r in msgs}
    assert {"user", "assistant"} <= roles, f"缺角色:{roles}"


def test_P2_权限模式轨迹落盘(serve_session, mock_script, sandbox) -> None:
    """权限模式切换的 from→to 轨迹 + at 时间戳落盘。

    依据:tests/COVERAGE.md §4(SDK 审批面 · rollout 持久化)。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "一轮"}]},
            timeout=DRAIN_TIMEOUT,
        )
        s.submit({"type": "set_permission_mode", "mode": "plan"})
        s.submit({"type": "cycle_permission_mode"})
    time.sleep(0.8)
    recs = rollout_records(sandbox.home)
    pmc = [r for r in recs if r.get("type") == "permission_mode_changed"]
    assert pmc, f"无 permission_mode_changed 记录:{[r.get('type') for r in recs]}"
    assert all(r.get("at") for r in pmc), f"pmc 记录缺 at:{pmc}"
    # 持久化任务可并发落盘,按每条记录自带的时间戳还原权限轨迹。
    pmc.sort(key=lambda r: datetime.fromisoformat(r["at"].replace("Z", "+00:00")))
    assert [(r.get("from"), r.get("to")) for r in pmc] == [
        ("auto", "plan"),
        ("plan", "auto"),
    ], f"pmc 轨迹不符:{pmc}"


def test_P3_rewind_物理截断并备份(serve_session, mock_script, sandbox) -> None:
    """rewind → 被回退的 turn 记录从 JSONL 中消失,并留下 `.rewind-<ts>.bak` 备份。

    依据:crates/resources/zhuye-rollout/src/writer.rs:254(备份)、
    :309-334(按 cut 点物理截断)。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"text","text":"one"}',
            '{"type":"text","text":"two"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]},
            timeout=DRAIN_TIMEOUT,
        )
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "第二轮"}]},
            timeout=DRAIN_TIMEOUT,
        )
        time.sleep(0.8)
        before = len(rollout_records(sandbox.home))
        assert before >= 4, f"两轮记录数不足:{before}"
        # rewind 必须在**同一会话**里做:换进程会写新的会话文件,截断的不是这份。
        s.submit({"type": "rewind"}, timeout=DRAIN_TIMEOUT)
    time.sleep(0.8)
    after = len(rollout_records(sandbox.home))
    assert after < before, f"rewind 应物理截断记录({before} → {after})"
    baks = list((sandbox.home / ".zhuye/sessions").glob("*/*/*/*.bak"))
    assert baks, "rewind 应留下 .bak 备份"


def test_P4_会话索引可列出(serve_session, mock_script, sandbox) -> None:
    """`zhuye session ls` 能列出刚写入的会话(首行 = SessionMeta 的索引契约)。

    注意:索引里的 `session_id` 取自 **SessionMeta 记录**,与**文件名**里的
    thread id 不是同一个 uuid(exec/serve 路径未注入 `with_session_id`)。
    """
    require_binary()
    script = mock_script(['{"type":"text","text":"t1"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "一轮"}]},
            timeout=DRAIN_TIMEOUT,
        )
    time.sleep(0.8)
    files = rollout_files(sandbox.home)
    assert files, "无会话文件"
    meta = [
        r for r in rollout_records(sandbox.home) if r.get("type") == "session_meta"
    ]
    assert meta, "无 session_meta 记录"
    session_id = meta[-1]["session_id"]

    proc = run_cli(sandbox, "session", "ls", env={"ZHUYE_MODEL": "mock"}, timeout=30)
    assert proc.returncode == 0, f"session ls 退出码={proc.returncode}:{proc.stderr}"
    assert session_id in proc.stdout, (
        f"session ls 未列出该会话:{session_id!r}\nstdout={proc.stdout!r}"
    )


@pytest.mark.parametrize("resume_args", ["thread", "session", "continue", "index"])
def test_P5_旧日期会话恢复后续写原文件(serve_session, mock_script, sandbox, resume_args) -> None:
    """任意日期的会话可按双 ID 或序号恢复,新消息写回原活跃文件。"""
    import json
    import uuid

    require_binary()
    thread_id, session_id, turn_id = (str(uuid.uuid4()) for _ in range(3))
    day = sandbox.home / ".zhuye/sessions/2020/01/02"
    day.mkdir(parents=True)
    meta = {"type": "session_meta", "session_id": session_id,
            "model": "mock", "started_at": "2020-01-02T00:00:00Z"}
    history = {"type": "message", "turn_id": turn_id, "role": "user", "content": "旧消息"}
    active = day / f"{thread_id}.jsonl"
    active.write_text(json.dumps(meta) + "\n" + json.dumps(history) + "\n")
    args = {"thread": ["--resume", thread_id], "session": ["--resume", session_id],
            "continue": ["-c"], "index": ["-r", "1"]}[resume_args]
    script = mock_script(['{"type":"text","text":"continued"}'])
    with serve_session(args=args, env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "新消息"}]}, timeout=DRAIN_TIMEOUT)
    assert any(e["msg"]["type"] == "turn_complete" for e in events)
    records = [json.loads(line) for line in active.read_text().splitlines()]
    assert records[0] == meta
    assert history in records
    messages = [r for r in records if r.get("type") == "message"]
    assert any(r["role"] == "assistant" and "continued" in json.dumps(r["content"]) for r in messages)
    assert list((sandbox.home / ".zhuye/sessions").glob("*/*/*/*.jsonl")) == [active]


def test_P6_标题精化_block数组与自定义名合并(serve_session, mock_script, sandbox) -> None:
    """`zhuye session show` 的 Title 字段:block 数组 content 提取 text 块,
    自定义名(`session rename`)优先于派生标题。

    依据:crates/resources/zhuye-rollout/src/index/mod.rs
    `extract_message_text`(block 数组)/ `resolve_display_title`(合并口径)。
    此前 block 数组走 `to_string()`,标题是整段 JSON 序列化串。
    """
    import json
    import uuid

    require_binary()
    thread_id = str(uuid.uuid4())
    day = sandbox.home / ".zhuye/sessions/2026/11/02"
    day.mkdir(parents=True)
    meta = {"type": "session_meta", "session_id": thread_id,
            "model": "mock", "started_at": "2026-11-02T00:00:00Z"}
    # block 数组格式的 user 消息(新格式),混入 image 块。
    history = {"type": "message", "turn_id": str(uuid.uuid4()), "role": "user",
               "content": [{"type": "text", "text": "修复 rollout 标题精化"},
                           {"type": "image", "data": [1, 2], "mime_type": "image/png"}]}
    active = day / f"{thread_id}.jsonl"
    active.write_text(json.dumps(meta) + "\n" + json.dumps(history) + "\n")

    # 1) 未命名 → Title 为 text 块文本,不含 JSON 键名。
    proc = run_cli(sandbox, "session", "show", thread_id, env={"ZHUYE_MODEL": "mock"}, timeout=30)
    assert proc.returncode == 0, f"session show 退出码={proc.returncode}:{proc.stderr}"
    title_lines = [ln for ln in proc.stdout.splitlines() if ln.startswith("Title:")]
    assert title_lines, f"session show 未输出 Title 行:\n{proc.stdout!r}"
    assert title_lines[0] == "Title:   修复 rollout 标题精化", (
        f"block 数组标题应为 text 块拼接,实际={title_lines[0]!r}"
    )
    assert "mime_type" not in proc.stdout and '"type"' not in proc.stdout, (
        f"标题不得泄漏 block JSON 键名:\n{proc.stdout!r}"
    )

    # 2) rename 后 → Title 为自定义名(优先于派生标题)。
    proc = run_cli(sandbox, "session", "rename", thread_id, "我的会话",
                   env={"ZHUYE_MODEL": "mock"}, timeout=30)
    assert proc.returncode == 0, f"session rename 退出码={proc.returncode}:{proc.stderr}"
    proc = run_cli(sandbox, "session", "show", thread_id, env={"ZHUYE_MODEL": "mock"}, timeout=30)
    assert proc.returncode == 0, f"session show 退出码={proc.returncode}:{proc.stderr}"
    title_lines = [ln for ln in proc.stdout.splitlines() if ln.startswith("Title:")]
    assert title_lines == ["Title:   我的会话"], (
        f"自定义名应优先于派生标题,实际={title_lines!r}"
    )


def test_P7_导出稿含Usage段与模型级拆分(serve_session, mock_script, sandbox) -> None:
    """`zhuye session export` 输出完整转写稿:对话内容 + 文末 Usage 段
    (per-model 表 / token 总量),不再走 4000 字符截断的 Pill 预览版。

    依据:crates/resources/zhuye-rollout/src/export.rs
    `render_transcript` / `render_usage_section`(收编自 ReflectDesktop)。
    """
    require_binary()
    # mock 两轮:第一轮普通文本,第二轮触发一次 bash 工具调用(带 token_count)。
    script = mock_script([
        '{"type":"text","text":"第一轮回复"}',
        '{"type":"tool_call","name":"bash","arguments":{"command":"echo hi"}}',
        '{"type":"text","text":"工具已完成"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]},
            timeout=DRAIN_TIMEOUT,
        )
    time.sleep(0.8)
    files = rollout_files(sandbox.home)
    assert files, "无会话文件"
    records = [json.loads(line) for f in files for line in f.read_text().splitlines() if line.strip()]
    thread_id = None
    for r in records:
        if r.get("type") == "session_meta":
            thread_id = r["session_id"]
    assert thread_id, "无 session_meta"

    proc = run_cli(sandbox, "session", "export", thread_id, env={"ZHUYE_MODEL": "mock"}, timeout=30)
    assert proc.returncode == 0, f"session export 退出码={proc.returncode}:{proc.stderr}"
    out = proc.stdout
    # 1) 对话转写稿:轮次标题 + 角色标记。
    assert "## Turn" in out and "**user**" in out and "**agent**" in out, (
        f"缺对话转写稿:\n{out[:800]!r}"
    )
    # 2) Usage 段:总量行 + per-model 表(字段级)。
    assert "## Usage" in out, f"缺 Usage 段:\n{out[:800]!r}"
    assert "- tokens:" in out and "in /" in out, f"缺 token 总量行:\n{out[:800]!r}"
    assert "### Per model" in out, f"缺 per-model 表:\n{out[:800]!r}"
    # 3) 导出稿不被 4000 字符上限截断(长会话仍完整)。
    assert "[truncated" not in out, "导出稿不应带 Pill 预览版的截断注脚"
    # 4) --out 落盘:文件内容与 stdout 一致,且是原子写的完整文件。
    dest = sandbox.cwd / "export.md"
    proc = run_cli(sandbox, "session", "export", thread_id, "--out", str(dest),
                   env={"ZHUYE_MODEL": "mock"}, timeout=30)
    assert proc.returncode == 0, f"session export --out 退出码={proc.returncode}:{proc.stderr}"
    assert dest.exists(), "导出文件未生成"
    assert "## Usage" in dest.read_text(), "落盘稿缺 Usage 段"


def test_P8_cron任务跨进程持久化(serve_session, mock_script, sandbox) -> None:
    """mock 调 `cron create` → job 落 `~/.zhuye/cron/<thread_id>.json`;
    `--resume` 同一 thread id 重启后 `cron list` 仍列出该 job(字段级)。

    依据:crates/integrations/zhuye-stream/src/cron.rs `with_store` /
    `store_path_for`(此前 docstring 承诺落盘但代码无 fs 写入,进程退出
    即丢)。
    """
    require_binary()

    # 1) 第一段会话:创建 job。
    script = mock_script([
        '{"type":"tool_call","name":"cron","args":{"action":"create",'
        '"schedule":"0 9 * * 1-5","prompt":"standup","name":"晨会"}}',
        '{"type":"text","text":"scheduled"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "建个定时"}]},
            timeout=DRAIN_TIMEOUT,
        )
        end = None
        for e in evs:
            if e["msg"]["type"] == "tool_call_end" and not e["msg"].get("is_error"):
                end = e["msg"]
                break
        assert end is not None, f"cron create 应成功:{[e['msg']['type'] for e in evs]}"
        thread_id = rollout_thread_id(sandbox.home)

    # 2) 落盘文件存在且含 schedule / prompt(字段级)。
    store = sandbox.home / ".zhuye/cron" / f"{thread_id}.json"
    assert store.exists(), f"cron store 未落盘:{store}"
    jobs = json.loads(store.read_text())
    assert len(jobs) == 1, f"应恰好 1 条 job:{jobs}"
    assert jobs[0]["schedule"] == "0 9 * * 1-5", f"schedule 不符:{jobs[0]}"
    assert jobs[0]["prompt"] == "standup", f"prompt 不符:{jobs[0]}"
    assert jobs[0]["name"] == "晨会", f"name 不符:{jobs[0]}"
    assert jobs[0]["enabled"] is True
    job_id = jobs[0]["id"]

    # 3) --resume 同一 thread id 重启:job 从 store 加载,cron list 可见。
    script = mock_script([
        '{"type":"tool_call","name":"cron","args":{"action":"list"}}',
        '{"type":"text","text":"listed"}',
    ])
    with serve_session(args=["--resume", thread_id],
                       env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "看看任务"}]},
            timeout=DRAIN_TIMEOUT,
        )
        listed = None
        for e in evs:
            if e["msg"]["type"] == "tool_call_end" and not e["msg"].get("is_error"):
                listed = json.dumps(e["msg"])
                break
        assert listed is not None, f"cron list 应成功:{[e['msg']['type'] for e in evs]}"
        assert job_id in listed and "0 9 * * 1-5" in listed and "standup" in listed, (
            f"重启后 job 应完整恢复:{listed[:400]}"
        )
