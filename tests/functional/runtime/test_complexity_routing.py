"""能力覆盖层 · v1.11 桌面#3:复杂度路由组(`[routing.main.complexity]`)。

覆盖:
- effort 档位映射模型:`set_effort low` 后下一轮 model_call 的请求
  `model` = 映射 spec(mock request_echo 快照含 `model` 字段,字段级)
- 未配置映射的档位回退 routing.main.primary
- 缓存说明文案(schema 文档 + GUI 设置页)由 L2 前端层验证,此处不重复

依据:AGENTS.md 开发流程检查表 —— 能力断言到字段级。
"""

from __future__ import annotations

import json
from typing import Any

from wire import msg_types, require_binary


def _agent_text(events: list[dict[str, Any]]) -> str:
    parts: list[str] = []
    for ev in events:
        m = ev.get("msg", {})
        if m.get("type") == "agent_message_delta":
            parts.append(m.get("delta", ""))
        elif m.get("type") == "agent_message":
            parts.append(m.get("text", ""))
    return "".join(parts)


def _echo_payload(events: list[dict[str, Any]]) -> dict[str, Any]:
    text = _agent_text(events)
    assert text, "request_echo 应回复一段 JSON 文本"
    return json.loads(text)


def test_复杂度路由_effort档位映射模型(serve_session, mock_script, sandbox) -> None:
    """`[routing.main]` primary + `[routing.main.complexity]` low 映射 →
    默认档(low)走映射模型;`set_effort high`(未配置映射)回退 primary。"""
    require_binary()
    sandbox.write_config(
        "\n[routing.main]\n"
        'primary = "mock/mock-primary"\n'
        "[routing.main.complexity]\n"
        'low = "mock/mock-cheap"\n'
    )
    script = mock_script(
        [
            '{"type":"request_echo"}',
            '{"type":"request_echo"}',
        ]
    )
    with serve_session(
        env={"ZHUYE_MODEL": "mock/mock-primary", "ZHUYE_MOCK_SCRIPT": str(script)}
    ) as s:
        # 默认 effort=Low → 命中映射 mock-cheap。
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "hi"}]}
        )
        payload = _echo_payload(evs)
        assert payload["model"] == "mock-cheap", (
            f"low 档应路由到映射模型 mock-cheap: {payload['model']}"
        )
        # 切到未配置映射的 high 档 → 回退 primary。
        s.send({"type": "set_effort", "effort": "high"})
        evs2 = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "hi again"}]}
        )
        payload2 = _echo_payload(evs2)
        assert payload2["model"] == "mock-primary", (
            f"未配置档位应回退 primary: {payload2['model']}"
        )


def test_无映射时模型不变(serve_session, mock_script, sandbox) -> None:
    """不写 `[routing.main.complexity]` → effort 切换不改变模型(向后兼容)。"""
    require_binary()
    script = mock_script(['{"type":"request_echo"}'])
    with serve_session(
        env={"ZHUYE_MODEL": "mock/mock-plain", "ZHUYE_MOCK_SCRIPT": str(script)}
    ) as s:
        s.send({"type": "set_effort", "effort": "ultra"})
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "hi"}]}
        )
        payload = _echo_payload(evs)
        assert payload["model"] == "mock-plain", (
            f"无映射时模型不变: {payload['model']}"
        )
