"""能力覆盖层 · v1.11 基建#9:默认读取 AGENTS.md(字段级断言)。

覆盖(v1.11 重构后:注入以 UserPromptSubmit 内置 hook 的
`<system-reminder>` user 消息形态,见 exec::builtin_hooks::AgentsMdHook):
- 默认注入:工作区根 AGENTS.md 存在 → 引擎发给模型的 user 消息含
  `## Project Instructions` 段 + AGENTS.md 正文 + 来源标注
- 用户级 + 项目级并存:用户级(`ZHUYE_HOME/AGENTS.md`)在前、项目级在后
- 显式关闭:`[active] agents_md = false` → hook 不注册,不再注入
- 无文件:hook 读不到内容则 Allow,不注入空段

依据:AGENTS.md 开发流程检查表 —— 能力断言到字段级,不接受"退出码 0"。
观测通道:mock provider 的 `request_echo` 快照(messages 各条文本)。
"""

from __future__ import annotations

import json
from typing import Any

from wire import require_binary


def _agent_text(events: list[dict[str, Any]]) -> str:
    """聚合一条 submission 的 agent_message_delta / agent_message 文本。"""
    parts: list[str] = []
    for ev in events:
        m = ev.get("msg", {})
        if m.get("type") == "agent_message_delta":
            parts.append(m.get("delta", ""))
        elif m.get("type") == "agent_message":
            parts.append(m.get("text", ""))
    return "".join(parts)


def _request_echo_payload(events: list[dict[str, Any]]) -> dict[str, Any]:
    """从 mock 的 request_echo 回复文本里解析观测快照。"""
    text = _agent_text(events)
    assert text, "request_echo 应回复一段 JSON 文本"
    return json.loads(text)


def test_默认注入项目根AGENTS_MD(serve_session, mock_script, sandbox) -> None:  # noqa: F811
    """工作区根 AGENTS.md → system core 块含 `## Project Instructions`
    段、正文与来源标注。"""
    require_binary()
    agents_md = sandbox.cwd / "AGENTS.md"
    agents_md.write_text("# 项目规范\n- 提交信息一律中文(type(scope): 描述)\n")
    script = mock_script(['{"type":"request_echo"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "hi"}]})
    payload = _request_echo_payload(events)
    # hook 注入形态:messages 里含 `<system-reminder>` 包裹的 user 消息。
    user_texts = [
        m.get("text", "")
        for m in payload.get("messages", [])
        if m.get("role") == "user"
    ]
    joined = "\n".join(user_texts)
    assert "## Project Instructions" in joined, (
        f"user 注入必须含 Project Instructions 段头: {joined[:400]}"
    )
    assert "提交信息一律中文" in joined, "AGENTS.md 正文必须完整注入"
    assert str(agents_md) in joined, "来源标注(Source: <path>)必须含项目级文件路径"


def test_用户级在前项目级在后(serve_session, mock_script, sandbox, tmp_path) -> None:
    """用户级 `ZHUYE_HOME/AGENTS.md` 与项目级并存 → 合并注入,
    用户级在前、项目级在后,各带来源标注。"""
    require_binary()
    # ZHUYE_HOME 重定向用户数据目录(config 一并从该目录读,写最小配置
    # 保持 hooks 关闭,与 Sandbox 默认一致)。
    user_home = tmp_path / "zhuye-home"
    user_home.mkdir()
    (user_home / "config.toml").write_text("[hooks]\nenabled = []\n")
    (user_home / "AGENTS.md").write_text("USER-LEVEL-RULE-用户级规范")
    (sandbox.cwd / "AGENTS.md").write_text("PROJECT-LEVEL-RULE-项目级规范")
    script = mock_script(['{"type":"request_echo"}'])
    with serve_session(
        env={
            "ZHUYE_MODEL": "mock",
            "ZHUYE_MOCK_SCRIPT": str(script),
            "ZHUYE_HOME": str(user_home),
        }
    ) as s:
        events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "hi"}]})
    core_text = "\n".join(_request_echo_payload(events)["messages"] and [
        m.get("text", "")
        for m in _request_echo_payload(events)["messages"]
        if m.get("role") == "user"
    ])
    user_pos = core_text.find("USER-LEVEL-RULE-用户级规范")
    proj_pos = core_text.find("PROJECT-LEVEL-RULE-项目级规范")
    assert user_pos != -1, f"用户级 AGENTS.md 必须注入: {core_text[:400]}"
    assert proj_pos != -1, "项目级 AGENTS.md 必须注入"
    assert user_pos < proj_pos, "用户级在前、项目级在后(项目级更具体放更后)"


def test_显式关闭agents_md(serve_session, mock_script, sandbox) -> None:
    """`[active] agents_md = false` → core 不含 Project Instructions 段
    (等价旧行为,显式 opt-out 生效)。"""
    require_binary()
    (sandbox.cwd / "AGENTS.md").write_text("SHOULD-NOT-APPEAR-不应出现")
    sandbox.write_config("\n[active]\nagents_md = false\n")
    script = mock_script(['{"type":"request_echo"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "hi"}]})
    payload = _request_echo_payload(events)
    joined = "\n".join(
        m.get("text", "") for m in payload.get("messages", []) if m.get("role") == "user"
    )
    assert "## Project Instructions" not in joined, "显式关闭后不得注入"
    assert "SHOULD-NOT-APPEAR" not in joined, "关闭后正文不得泄漏"


def test_无AGENTS_MD时不注入空段(serve_session, mock_script) -> None:
    """工作区无 AGENTS.md → core 不含 Project Instructions 段头(向后兼容,
    既有会话 system prompt 不变)。"""
    require_binary()
    script = mock_script(['{"type":"request_echo"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "hi"}]})
    joined = "\n".join(
        m.get("text", "") for m in _request_echo_payload(events).get("messages", [])
        if m.get("role") == "user"
    )
    assert "## Project Instructions" not in joined, "无文件不得注入空段"
