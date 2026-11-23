"""能力覆盖层 · prompt cache 保温(cache warming,字段级断言)。

覆盖:
- 默认关闭:无任何 cache_warming 事件、无任何行为(零变化基线)。
- `[cache_warming] enabled + force`:turn 完成后空闲排程(scheduled)→
  TTL 90% 处重放(executed,字段级:action / phase / model /
  prompt_tokens),且重放**不消耗 mock 脚本行**(第二轮拿到的仍是
  脚本第 2 行,而非"固定回复"回退)、rollout 零新增记录(零污染不变式)。
- 观测门控(非 force):mock 回执 cached=0 → fire 时决策 skipped
  (reason=no_cache_hits),不发生重放。

依据:AGENTS.md 开发流程检查表 —— 能力断言到字段级,不接受"退出码 0"。
"""

import json
import time
from pathlib import Path

import pytest

from wire import (
    DRAIN_TIMEOUT,
    rollout_records,
    require_binary,
)

pytestmark = pytest.mark.usefixtures("sandbox")

# 保温刷新延时:config ttl_secs=3 + margin 0 → 90% 处 ≈ 2s;留裕量。
WARM_FIRE_TIMEOUT = 15.0


def _warming_events(session) -> list[dict]:
    """会话里的全部 cache_warming 全局事件(引擎主动推,id 为空)。"""
    out = []
    for ev in session.global_events():
        msg = ev.get("msg", {})
        if msg.get("type") == "cache_warming":
            out.append(msg)
    return out


def _wait_warming(session, action: str, timeout: float = WARM_FIRE_TIMEOUT) -> dict:
    """等一个指定 action 的 cache_warming 全局事件,超时抛断言。"""
    deadline = time.monotonic() + timeout
    seen: set[int] = set()
    while time.monotonic() < deadline:
        for i, msg in enumerate(_warming_events(session)):
            if i in seen:
                continue
            seen.add(i)
            if msg.get("action") == action:
                return msg
        time.sleep(0.1)
    got = [(m.get("action"), m.get("reason")) for m in _warming_events(session)]
    pytest.fail(f"{timeout}s 内未等到 cache_warming action={action};已见={got}")


def _assistant_text(events: list[dict]) -> str:
    """聚合某条 turn 事件流里的助手文本(delta + 整段)。"""
    parts: list[str] = []
    for ev in events:
        msg = ev.get("msg", {})
        if msg.get("type") == "agent_message_delta":
            parts.append(msg.get("delta", ""))
        elif msg.get("type") == "agent_message":
            parts.append(msg.get("text", ""))
    return "".join(parts)


def test_cache_warming_默认关闭零行为(serve_session, mock_script) -> None:
    """对照组:未配置 `[cache_warming]` → 全程无保温事件(行为零变化)。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"第一轮真实回复"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "你好"}]},
            timeout=DRAIN_TIMEOUT,
        )
        # 超过默认 TTL 窗口后仍无任何保温事件。
        time.sleep(3.0)
        assert _warming_events(s) == [], f"默认关闭不得有保温事件:{_warming_events(s)}"


def test_cache_warming_开启后空闲排程与重放_脚本行不被消耗(
    serve_session, mock_script, sandbox
) -> None:
    """enabled + force:turn 完成 → scheduled → executed;重放不偷脚本行,
    rollout 零新增(零污染),第二轮回复仍是脚本第 2 行。"""
    require_binary()
    sandbox.write_config(
        "\n[cache_warming]\nenabled = true\nforce = true\nttl_secs = 3\nrefresh_margin_secs = 0\n"
    )
    script = mock_script(
        [
            '{"type":"text","text":"第一轮真实回复"}',
            '{"type":"text","text":"第二轮真实回复"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        # 第一轮:消耗脚本第 1 行。
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "第一问"}]},
            timeout=DRAIN_TIMEOUT,
        )
        # turn 完成后 rollout 快照(重放前)。
        before = [r.get("type") for r in rollout_records(sandbox.home)]

        scheduled = _wait_warming(s, "scheduled")
        assert scheduled.get("phase") == "idle", f"scheduled.phase 字段:{scheduled}"
        assert "reason" not in scheduled or scheduled.get("reason") is None

        executed = _wait_warming(s, "executed")
        assert executed.get("phase") == "idle", f"executed.phase 字段:{executed}"
        # policy 解析后的完整 spec(mock 池默认模型 mock-1)。
        assert executed.get("model") == "mock/mock-1", f"executed.model 字段:{executed}"
        assert isinstance(executed.get("prompt_tokens"), int), f"prompt_tokens 字段:{executed}"
        # 重放流回的 usage(mock:cached=0)必须如实上报,不编造命中。
        assert executed.get("warm_cached_tokens") == 0, f"executed 字段:{executed}"

        # 零污染:重放后 rollout 记录类型序列不变(无 Message / TokenCount 新增)。
        time.sleep(0.5)
        after = [r.get("type") for r in rollout_records(sandbox.home)]
        assert after == before, f"保温重放不得写 rollout:前={before} 后={after}"

        # 第二轮:脚本第 2 行未被保温偷走 → 回复仍是脚本内容而非固定回退。
        evs2 = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "第二问"}]},
            timeout=DRAIN_TIMEOUT,
        )
        text = _assistant_text(evs2)
        assert text == "第二轮真实回复", (
            f"第二轮应消耗脚本第 2 行(保温不得偷行),实际:{text!r}"
        )


def test_cache_warming_无缓存命中回执时决策跳过(serve_session, mock_script, sandbox) -> None:
    """非 force:mock 回执 cached_tokens=0 → fire 时决策 skipped
    (reason=no_cache_hits),不发生任何重放 —— 观测门控端到端。"""
    require_binary()
    sandbox.write_config(
        "\n[cache_warming]\nenabled = true\nttl_secs = 3\nrefresh_margin_secs = 0\n"
    )
    script = mock_script(['{"type":"text","text":"第一轮真实回复"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "你好"}]},
            timeout=DRAIN_TIMEOUT,
        )
        skipped = _wait_warming(s, "skipped")
        assert skipped.get("reason") == "no_cache_hits", f"skip 原因字段:{skipped}"
        assert skipped.get("phase") == "idle", f"skipped.phase 字段:{skipped}"
        # 不应有 executed(决策 Stop 即结束周期)。
        assert all(
            m.get("action") != "executed" for m in _warming_events(s)
        ), f"观测门控后不得重放:{_warming_events(s)}"
