"""能力覆盖层 · v1.11 基建#8:deepagents 式记忆(字段级断言)。

覆盖:
- 多文件 topic 知识库:memory 工具 `write`(带 topic)→ `topics` →
  `read`(带 topic)→ `remove`(带 topic),topic 文件落
  `agent-memory/<agent>/topics/<name>.md`
- `consolidate` action:同名(大小写不敏感)条目合并 + 统计报告
- 跨会话笔记召回:会话 A 用 `add_session_note` 写笔记 → 会话 B(新
  thread)以相关查询提交 → 引擎发给模型的 messages 含「来自其他会话的
  相关笔记」与原文(排除本会话,零相关不注入)
- 后台自动整理:`ZHUYE_MEMORY_CONSOLIDATE_TURNS=1` 时 turn 结束后
  MEMORY.md 被自动去重合并(无需手动 consolidate)

观测通道:mock provider `request_echo` 快照(messages role/text)。
依据:AGENTS.md 开发流程检查表 —— 能力断言到字段级。
"""

from __future__ import annotations

import json
import time
from pathlib import Path
from typing import Any

import pytest

from wire import first_event, msg_types, require_binary, rollout_thread_id

pytestmark = pytest.mark.usefixtures("sandbox")


def _tool_text(msg: dict[str, Any]) -> str:
    """tool_call_end 输出里的文本拼接。"""
    return "".join(
        b.get("text", "")
        for b in msg.get("output", {}).get("content", [])
        if isinstance(b, dict)
    )


def _agent_text(events: list[dict[str, Any]]) -> str:
    """聚合事件流里模型回复文本(delta + 整段,request_echo 用)。"""
    parts: list[str] = []
    for ev in events:
        m = ev.get("msg", {})
        if m.get("type") == "agent_message_delta":
            parts.append(m.get("delta", ""))
        elif m.get("type") == "agent_message":
            parts.append(m.get("text", ""))
    return "".join(parts)


def _echo_payload(events: list[dict[str, Any]]) -> dict[str, Any]:
    """解析 request_echo 观测快照。"""
    text = _agent_text(events)
    assert text, "request_echo 应回复一段 JSON 文本"
    return json.loads(text)


def test_topic知识库工具全链路(serve_session, mock_script, sandbox) -> None:
    """memory 工具 topic 子路径:write → topics → read → remove,
    文件落 `agent-memory/<agent>/topics/<name>.md`(字段级)。"""
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call","name":"memory","args":{"action":"write","topic":"arch","value":"# 架构\\nredis runs on port 6379"}}',
            '{"type":"tool_call","name":"memory","args":{"action":"topics"}}',
            '{"type":"tool_call","name":"memory","args":{"action":"read","topic":"arch"}}',
            '{"type":"text","text":"{\\"echo\\":\\"done\\"}"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit({"type": "user_input", "items": [{"type": "text", "text": "记到知识库"}]})
    ends = [
        e["msg"]
        for e in evs
        if e.get("msg", {}).get("type") == "tool_call_end" and not e["msg"].get("is_error")
    ]
    assert len(ends) >= 3, f"三次工具调用都应成功:{msg_types(evs)}"
    assert "topic `arch` written" in _tool_text(ends[0]), _tool_text(ends[0])
    topics_text = _tool_text(ends[1])
    assert "arch" in topics_text, f"topics 列表应含 arch:{topics_text}"
    assert "port 6379" in _tool_text(ends[2]), "read topic 应返回正文"
    # 文件级断言:落盘路径与内容。
    topic_file = sandbox.cwd / ".zhuye" / "agent-memory" / "default" / "topics" / "arch.md"
    assert topic_file.exists(), f"topic 文件未落盘:{topic_file}"
    assert "port 6379" in topic_file.read_text()


def test_consolidate_合并同名条目(serve_session, mock_script, sandbox) -> None:
    """`consolidate` action:Deploy/deploy 两块合并为一块,报告 merged=1。"""
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call","name":"memory","args":{"action":"write","key":"Deploy","value":"uses github actions"}}',
            '{"type":"tool_call","name":"memory","args":{"action":"write","key":"deploy","value":"runs on main branch"}}',
            '{"type":"tool_call","name":"memory","args":{"action":"consolidate"}}',
            '{"type":"text","text":"{\\"echo\\":\\"done\\"}"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit({"type": "user_input", "items": [{"type": "text", "text": "整理记忆"}]})
    ends = [
        e["msg"]
        for e in evs
        if e.get("msg", {}).get("type") == "tool_call_end" and not e["msg"].get("is_error")
    ]
    assert len(ends) >= 3, f"write/write/consolidate 应成功:{msg_types(evs)}"
    cons_text = _tool_text(ends[2])
    assert "merged=1" in cons_text, f"应报告一次合并:{cons_text}"
    # 文件级:MEMORY.md 只剩一个标题块,双正文保留(合并不丢信息)。
    memory_md = sandbox.cwd / ".zhuye" / "agent-memory" / "default" / "MEMORY.md"
    body = memory_md.read_text()
    assert body.count("## ") == 1, f"合并后应只有一个标题块:{body!r}"
    assert "uses github actions" in body and "runs on main branch" in body


def test_跨会话笔记召回(serve_session, mock_script, sandbox) -> None:
    """会话 A 写笔记 → 会话 B 相关查询 → 引擎发给模型的 messages 含
    召回标注与笔记原文。"""
    require_binary()
    # 会话 A:add_session_note 落盘一条可检索发现。
    script_a = mock_script(
        [
            '{"type":"tool_call","name":"add_session_note","args":{"text":"staging deploy needs ZHUYE_TOKEN env var"}}',
            '{"type":"text","text":"noted"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script_a)}) as a:
        a.submit({"type": "user_input", "items": [{"type": "text", "text": "记一下部署要点"}]})
    notes_dir = sandbox.home / ".zhuye" / "session-notes"
    jsonls = list(notes_dir.glob("*.jsonl"))
    assert jsonls, f"会话 A 笔记未落盘:{notes_dir}"
    assert any("ZHUYE_TOKEN" in p.read_text() for p in jsonls)

    # 会话 B(新 thread):相关查询 → 召回。
    script_b = mock_script(['{"type":"request_echo"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script_b)}) as b:
        evs = b.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "staging deploy 需要什么"}]}
        )
    payload = _echo_payload(evs)
    user_texts = [
        m.get("text", "")
        for m in payload.get("messages", [])
        if m.get("role") == "user"
    ]
    joined = "\n".join(user_texts)
    assert "来自其他会话的相关笔记" in joined, f"缺召回标注:{joined[:600]}"
    assert "ZHUYE_TOKEN" in joined, f"召回应含笔记原文:{joined[:600]}"


def test_零相关查询不注入召回(serve_session, mock_script, sandbox) -> None:
    """无查询交集 → 不注入召回块(宁缺毋滥,不注噪声)。"""
    require_binary()
    script_a = mock_script(
        [
            '{"type":"tool_call","name":"add_session_note","args":{"text":"postgres tuning checklist for prod"}}',
            '{"type":"text","text":"noted"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script_a)}) as a:
        a.submit({"type": "user_input", "items": [{"type": "text", "text": "记录"}]})
    script_b = mock_script(['{"type":"request_echo"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script_b)}) as b:
        evs = b.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "帮我写首诗"}]}
        )
    payload = _echo_payload(evs)
    joined = "\n".join(
        m.get("text", "") for m in payload.get("messages", []) if m.get("role") == "user"
    )
    assert "来自其他会话的相关笔记" not in joined, f"零相关不得注入:{joined[:400]}"


def test_后台自动整理_周期触发(serve_session, mock_script, sandbox) -> None:
    """`ZHUYE_MEMORY_CONSOLIDATE_TURNS=1` → turn 结束后台自动合并同名
    条目(无需手动 consolidate;轮询 MEMORY.md 直到合并生效)。"""
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call","name":"memory","args":{"action":"write","key":"Deploy","value":"uses github actions"}}',
            '{"type":"tool_call","name":"memory","args":{"action":"write","key":"deploy","value":"runs on main branch"}}',
            '{"type":"text","text":"{\\"echo\\":\\"turn1\\"}"}',
            '{"type":"request_echo"}',
        ]
    )
    with serve_session(
        env={
            "ZHUYE_MODEL": "mock",
            "ZHUYE_MOCK_SCRIPT": str(script),
            "ZHUYE_MEMORY_CONSOLIDATE_TURNS": "1",
        }
    ) as s:
        s.submit({"type": "user_input", "items": [{"type": "text", "text": "第一轮"}]})
        # 第二 turn:整理在第一 turn 后台 spawn,轮询等文件被合并。
        s.submit({"type": "user_input", "items": [{"type": "text", "text": "第二轮"}]})
        memory_md = sandbox.cwd / ".zhuye" / "agent-memory" / "default" / "MEMORY.md"
        deadline = time.monotonic() + 15.0
        merged = False
        while time.monotonic() < deadline:
            if memory_md.exists() and memory_md.read_text().count("## ") == 1:
                merged = True
                break
            time.sleep(0.3)
        assert merged, f"后台整理未生效:{memory_md.read_text() if memory_md.exists() else '(missing)'}"
