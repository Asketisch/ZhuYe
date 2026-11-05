"""能力覆盖层 · 工具面:内置工具、远程工具、审批链路。

直接驱动 `zhuye serve` 原始 JSONL wire(不经过任何 SDK)。

覆盖:
- T1 内置工具调用(mock 调 `echo` → tool_call_begin → tool_call_end);
- T2 模型调未注册工具 → 错误输出(core 侧 `tool not found`);
- T3 远程工具往返(register_tools → tool_execution_request → 回执 → tool_call_end);
- T4 远程工具失败回执(is_error=true);
- T5 工具审批全链路(ZHUYE_APPROVALS=1 + MCP 工具);
- T6 审批 deny 分支(工具不执行 + 错误输出);
- T7 shell 拒绝规则覆盖换行与后台命令;
- T8 OS 沙箱拒绝写入的错误退出码;
- T9 macOS 真实 HOME 敏感目录读取保护;
- T10 stdout/stderr 同时截断后的失败退出码。

运行:tests/run_all.sh functional,或
  ZHUYE_BIN=... python3 -m pytest tests/functional/test_tools.py -v
"""

from __future__ import annotations

import os
import time
from typing import Any

import pytest

from wire import (
    DRAIN_TIMEOUT,
    MOCK_MCP,
    first_event,
    msg_types,
    require_binary,
    rollout_files,
)

pytestmark = pytest.mark.usefixtures("sandbox")


def _tool_end(events: list[dict[str, Any]]) -> dict[str, Any]:
    """取 tool_call_end 事件(找不到直接失败,附上已见类型)。"""
    m = first_event(events, "tool_call_end")
    assert m is not None, f"未见 tool_call_end:{msg_types(events)}"
    return m["msg"]


def _tool_text(msg: dict[str, Any]) -> str:
    """tool_call_end 输出里的文本拼接。"""
    content = msg.get("output", {}).get("content", [])
    return "".join(c.get("text", "") for c in content if isinstance(c, dict))


@pytest.mark.skipif(os.name == "nt", reason="符号链接创建需要额外的系统权限")
def test_T24_文件工具拒绝指向工作区外的链接(serve_session, mock_script, sandbox) -> None:
    """读取和写入不得通过符号链接访问工作区外的文件。"""
    import json

    require_binary()
    victim = sandbox.home / "outside-secret.txt"
    victim.write_text("外部原始内容")
    (sandbox.cwd / "outside-link").symlink_to(victim)
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "read", "args": {"path": "outside-link"}}),
        json.dumps({"type": "tool_call", "name": "write", "args": {"path": "outside-link", "content": "篡改"}}),
        json.dumps({"type": "text", "text": "完成"}),
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as session:
        events = session.submit({"type": "user_input", "items": [{"type": "text", "text": "检查文件"}]}, timeout=DRAIN_TIMEOUT)
    ends = [event["msg"] for event in events if event["msg"]["type"] == "tool_call_end"]
    assert len(ends) == 2
    assert all(end["is_error"] for end in ends)
    assert all("外部原始内容" not in _tool_text(end) for end in ends)
    assert victim.read_text() == "外部原始内容"


def test_T25_用户权限随数据目录隔离(serve_session, mock_script, sandbox, monkeypatch) -> None:
    """用户级拒绝规则从 ZHUYE_HOME 读取，空值时回退 HOME。"""
    import json

    require_binary()
    monkeypatch.delenv("ZHUYE_APPROVALS", raising=False)
    default_rules = sandbox.home / ".zhuye/permissions.toml"
    default_rules.write_text('[[rule]]\ntool = "echo"\naction = "allow"\n')
    alternate = sandbox.cwd / "isolated-home"
    alternate.mkdir()
    (alternate / "permissions.toml").write_text('[[rule]]\ntool = "echo"\naction = "deny"\n')
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "echo", "args": {"text": "目录隔离"}}),
        json.dumps({"type": "text", "text": "完成"}),
    ])
    for environment, denied in [({"ZHUYE_HOME": str(alternate), "HOME": ""}, True),
                                ({"ZHUYE_HOME": ""}, False)]:
        environment.update({"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)})
        with serve_session(env=environment) as session:
            events = session.submit({"type": "user_input", "items": [{"type": "text", "text": "检查权限"}]}, timeout=DRAIN_TIMEOUT)
        end = _tool_end(events)
        assert end["is_error"] is denied, end
        assert ("denied by rule" in _tool_text(end)) is denied, end
        if not denied:
            assert "目录隔离" in _tool_text(end)


def test_T26_损坏权限文件拒绝工具并在修复后恢复(serve_session, mock_script, sandbox, monkeypatch) -> None:
    """权限文件损坏不能让规则链失效；修复后重新按规则执行。"""
    import json

    require_binary()
    monkeypatch.delenv("ZHUYE_APPROVALS", raising=False)
    permission_file = sandbox.home / ".zhuye/permissions.toml"
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "echo", "args": {"text": "恢复执行"}}),
        json.dumps({"type": "text", "text": "完成"}),
    ])
    for contents, denied in [("[[rule\n", True), ('[[rule]]\ntool = "echo"\naction = "allow"\n', False)]:
        permission_file.write_text(contents)
        with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script), "ZHUYE_HOME": ""}) as session:
            events = session.submit({"type": "user_input", "items": [{"type": "text", "text": "检查权限"}]}, timeout=DRAIN_TIMEOUT)
        end = _tool_end(events)
        assert end["is_error"] is denied, end
        if denied:
            assert "denied by rule" in _tool_text(end)
        else:
            assert "恢复执行" in _tool_text(end)


# ── T 组:工具调用 ───────────────────────────────────────────────────────


def test_T1_内置工具调用(serve_session, mock_script) -> None:
    """mock 调内置 `echo` → tool_call_begin → tool_call_end(回显文本)。

    依据:tests/rust/tools_drive.sh 组 1(同名同参)。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call","name":"echo","args":{"text":"echo-marker"}}',
            '{"type":"text","text":"tool-done"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "调 echo"}]},
            timeout=DRAIN_TIMEOUT,
        )
    begin = first_event(evs, "tool_call_begin")
    assert begin is not None, f"未见 tool_call_begin:{msg_types(evs)}"
    assert begin["msg"]["tool_name"] == "echo", f"tool_name={begin['msg']['tool_name']!r}"
    end = _tool_end(evs)
    assert end["is_error"] is False, f"echo 应成功:{end}"
    assert "echo-marker" in _tool_text(end), f"输出未含回显:{_tool_text(end)!r}"
    assert end.get("elapsed_ms", -1) >= 0, f"elapsed_ms 异常:{end}"
    assert "turn_complete" in msg_types(evs), f"turn 未收尾:{msg_types(evs)}"


def test_T2_模型调未注册工具_错误输出(serve_session, mock_script) -> None:
    """模型调不存在的工具 → tool_call_end(is_error=true,含 `tool not found`)。

    依据:crates/abilities/zhuye-tools/src/queue.rs:296-304。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call","name":"no_such_tool_xyz","args":{}}',
            '{"type":"text","text":"recovered"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "调不存在的工具"}]},
            timeout=DRAIN_TIMEOUT,
        )
    end = _tool_end(evs)
    assert end["is_error"] is True, f"未注册工具应报错:{end}"
    assert "tool not found" in _tool_text(end), f"错误文案异常:{_tool_text(end)!r}"
    # 工具失败不应打死 turn。
    assert "turn_complete" in msg_types(evs), f"turn 未收尾:{msg_types(evs)}"


def test_T3_远程工具往返(serve_session, mock_script) -> None:
    """register_tools → 模型调用 → `tool_execution_request`(全局)→ 回执 → tool_call_end。

    依据:crates/abilities/zhuye-tools/src/remote.rs:66-110;
    sdks/PROTOCOL.md §5(远程工具执行流)。

    注意 call_id 的两套语义:`tool_execution_request.call_id` 是桥生成的
    uuid(回执用它配对),`tool_call_begin/end.call_id` 是模型的 tool id。
    """
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call","name":"remote_echo","args":{"text":"hi-remote"}}',
            '{"type":"text","text":"after-tool"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.send(
            {
                "type": "register_tools",
                "tools": [
                    {
                        "name": "remote_echo",
                        "description": "echo back",
                        "parameters": {"type": "object"},
                    }
                ],
            }
        )
        time.sleep(0.5)
        sid = s.send(
            {"type": "user_input", "items": [{"type": "text", "text": "调远程工具"}]}
        )
        req = s.wait_global(
            lambda e: e.get("msg", {}).get("type") == "tool_execution_request"
        )
        assert req["id"] == "", "远程工具请求是全局事件(id 为空)"
        rmsg = req["msg"]
        assert rmsg["tool"] == "remote_echo", f"tool={rmsg.get('tool')!r}"
        assert rmsg["args"] == {"text": "hi-remote"}, f"args={rmsg.get('args')!r}"
        # 回执用请求里的 call_id 配对。
        s.send(
            {
                "type": "tool_execution_response",
                "call_id": rmsg["call_id"],
                "output": {
                    "content": [{"type": "text", "text": "remote-ok"}],
                    "is_error": False,
                    "metadata": {},
                    "elapsed_ms": 1,
                },
            }
        )
        evs = s.collect(sid, timeout=DRAIN_TIMEOUT)
    end = _tool_end(evs)
    assert end["is_error"] is False, f"远程工具应成功:{end}"
    assert _tool_text(end) == "remote-ok", f"输出未透传:{_tool_text(end)!r}"
    assert "turn_complete" in msg_types(evs), f"turn 未收尾:{msg_types(evs)}"


def test_T4_远程工具失败回执(serve_session, mock_script) -> None:
    """回执 is_error=true → tool_call_end(is_error=true),turn 仍收尾。"""
    require_binary()
    script = mock_script(
        [
            '{"type":"tool_call","name":"remote_fail","args":{"boom":true}}',
            '{"type":"text","text":"remote-done"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.send(
            {
                "type": "register_tools",
                "tools": [
                    {
                        "name": "remote_fail",
                        "description": "always fails",
                        "parameters": {"type": "object"},
                    }
                ],
            }
        )
        time.sleep(0.5)
        sid = s.send(
            {"type": "user_input", "items": [{"type": "text", "text": "调会失败的工具"}]}
        )
        req = s.wait_global(
            lambda e: e.get("msg", {}).get("type") == "tool_execution_request"
        )
        s.send(
            {
                "type": "tool_execution_response",
                "call_id": req["msg"]["call_id"],
                "output": {
                    "content": [{"type": "text", "text": "boom failed"}],
                    "is_error": True,
                    "metadata": {},
                    "elapsed_ms": 1,
                },
            }
        )
        evs = s.collect(sid, timeout=DRAIN_TIMEOUT)
    end = _tool_end(evs)
    assert end["is_error"] is True, f"失败回执应产出 is_error:{end}"
    assert "boom failed" in _tool_text(end), f"错误文本未透传:{_tool_text(end)!r}"
    assert "turn_complete" in msg_types(evs), f"turn 未收尾:{msg_types(evs)}"


def test_T5_工具审批全链路(serve_session, mock_script, sandbox) -> None:
    """MCP 工具(Prompt 权限)→ approval_request → 回执 → 工具真执行 → turn_complete。

    依据:crates/runtime/zhuye-core/src/submission_loop.rs:173-180
    (ZHUYE_APPROVALS=1 开启 gate)。
    """
    require_binary()
    if not MOCK_MCP.is_file():
        pytest.skip("mock_mcp_server 未构建(cargo build -p zhuye-mcp)")
    sandbox.write_config(
        f'\n[mcp_servers.fsx]\ntype = "stdio"\ncommand = "{MOCK_MCP}"\n'
    )
    script = mock_script(
        [
            '{"type":"tool_call","name":"mcp__fsx__echo","args":{"text":"probe"}}',
            '{"type":"text","text":"approved-done"}',
        ]
    )
    with serve_session(
        env={
            "ZHUYE_MODEL": "mock",
            "ZHUYE_MOCK_SCRIPT": str(script),
            "ZHUYE_APPROVALS": "1",
        }
    ) as s:
        ready = s.wait_global(lambda e: e.get("msg", {}).get("type") in {"mcp_server_started", "mcp_server_failed"} and e["msg"].get("server") == "fsx")
        assert ready["msg"]["type"] == "mcp_server_started", ready
        assert ready["msg"]["tool_count"] >= 1, ready
        sid = s.send(
            {"type": "user_input", "items": [{"type": "text", "text": "调 echo 工具"}]}
        )
        seen: dict[str, Any] = {}

        def _on_event(ev: dict[str, Any]) -> None:
            m = ev.get("msg", {})
            if m.get("type") == "approval_request" and "request_id" not in seen:
                seen.update(m)
                s.send(
                    {
                        "type": "tool_approval",
                        "id": m["request_id"],
                        "decision": "approve",
                    }
                )

        evs = s.collect(sid, timeout=40.0, on_event=_on_event)
    assert seen, f"未见 approval_request:{msg_types(evs)}"
    assert seen["kind"]["type"] == "tool", f"kind={seen['kind']!r}"
    assert seen["kind"]["tool_name"] == "mcp__fsx__echo", f"tool_name={seen['kind']!r}"
    assert seen.get("risk") in ("low", "medium", "high"), f"缺 risk:{seen!r}"
    end = _tool_end(evs)
    assert end["is_error"] is False, f"放行后工具应成功:{end}"
    assert "probe" in _tool_text(end), f"输出未含回显:{_tool_text(end)!r}"
    assert "turn_complete" in msg_types(evs), f"审批后 turn 未收尾:{msg_types(evs)}"


def test_T6_审批deny_工具不执行(serve_session, mock_script, sandbox) -> None:
    """审批回 deny → 工具**未执行**,tool_call_end(is_error=true),turn 仍收尾。

    依据:crates/abilities/zhuye-tools/src/approval/mod.rs:96-110。
    """
    require_binary()
    if not MOCK_MCP.is_file():
        pytest.skip("mock_mcp_server 未构建(cargo build -p zhuye-mcp)")
    sandbox.write_config(
        f'\n[mcp_servers.fsx]\ntype = "stdio"\ncommand = "{MOCK_MCP}"\n'
    )
    script = mock_script(
        [
            '{"type":"tool_call","name":"mcp__fsx__echo","args":{"text":"probe"}}',
            '{"type":"text","text":"denied-done"}',
        ]
    )
    with serve_session(
        env={
            "ZHUYE_MODEL": "mock",
            "ZHUYE_MOCK_SCRIPT": str(script),
            "ZHUYE_APPROVALS": "1",
        }
    ) as s:
        ready = s.wait_global(lambda e: e.get("msg", {}).get("type") in {"mcp_server_started", "mcp_server_failed"} and e["msg"].get("server") == "fsx")
        assert ready["msg"]["type"] == "mcp_server_started", ready
        assert ready["msg"]["tool_count"] >= 1, ready
        sid = s.send(
            {"type": "user_input", "items": [{"type": "text", "text": "调 echo 工具"}]}
        )
        seen: dict[str, Any] = {}

        def _on_event(ev: dict[str, Any]) -> None:
            m = ev.get("msg", {})
            if m.get("type") == "approval_request" and "request_id" not in seen:
                seen.update(m)
                s.send(
                    {
                        "type": "tool_approval",
                        "id": m["request_id"],
                        "decision": {"deny": {"reason": "DENY-REASON-XYZ"}},
                    }
                )

        evs = s.collect(sid, timeout=40.0, on_event=_on_event)
    assert seen, f"未见 approval_request:{msg_types(evs)}"
    end = _tool_end(evs)
    assert end["is_error"] is True, f"deny 后工具应报错:{end}"
    assert "DENY-REASON-XYZ" in _tool_text(end), f"deny 理由未透传:{_tool_text(end)!r}"
    assert "turn_complete" in msg_types(evs), f"deny 后 turn 未收尾:{msg_types(evs)}"


@pytest.mark.parametrize("separator", ["\n", " & "])
def test_T7_shell拒绝规则覆盖换行与后台命令(serve_session, mock_script, sandbox, separator) -> None:
    """附加段命中显式拒绝规则时,整条 Bash 命令应拒绝执行。"""
    import json

    require_binary()
    sandbox.write_config('''
[[permissions.rule]]
tool = "bash"
action = "deny"
shell_pattern = "touch *"
''')
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "bash", "args": {"cmd": f"echo harmless{separator}touch denied-marker"}}),
        '{"type":"text","text":"done"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script), "ZHUYE_APPROVALS": "1"}) as s:
        events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "测试拒绝规则"}]}, timeout=DRAIN_TIMEOUT)
    end = _tool_end(events)
    assert end["is_error"] is True, end
    assert "denied" in _tool_text(end).lower(), end
    assert not (sandbox.cwd / "denied-marker").exists()


def test_T15_资源拒绝规则只匹配指定URL(serve_session, mock_script, sandbox) -> None:
    """资源规则拒绝匹配 URL,不匹配 URL 仍按正常审批路径处理。"""
    import json

    sandbox.write_config('''
[[permissions.rule]]
tool = "web_fetch"
action = "deny"
resource_glob = "https://blocked.invalid/**"
''')
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "web_fetch", "args": {"url": "https://blocked.invalid/item"}}),
        json.dumps({"type": "tool_call", "name": "web_fetch", "args": {"url": "https://other.invalid/item"}}),
        '{"type":"text","text":"done"}',
    ])
    seen = []
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script), "ZHUYE_APPROVALS": "1"}) as session:
        sid = session.send({"type": "user_input", "items": [{"type": "text", "text": "测试资源规则"}]})
        def respond(event: dict[str, Any]) -> None:
            message = event.get("msg", {})
            if message.get("type") == "approval_request":
                seen.append(message)
                session.send({"type": "tool_approval", "id": message["request_id"], "decision": {"deny": {"reason": "test-deny"}}})
        events = session.collect(sid, timeout=DRAIN_TIMEOUT, on_event=respond)
    ends = [event["msg"] for event in events if event["msg"]["type"] == "tool_call_end"]
    assert len(ends) == 2, ends
    assert ends[0]["is_error"] is True and "denied by rule" in _tool_text(ends[0]), ends[0]
    assert len(seen) == 1, seen
    assert ends[1]["is_error"] is True and "test-deny" in _tool_text(ends[1]), ends[1]
    assert "turn_complete" in msg_types(events)


def test_T16_会话放行不能绕过资源拒绝(serve_session, mock_script, sandbox) -> None:
    """同一工具先获得会话放行,后续匹配资源拒绝规则仍必须拒绝。"""
    import json

    sandbox.write_config('''
[[permissions.rule]]
tool = "web_fetch"
action = "deny"
resource_glob = "https://blocked.invalid/**"
''')
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "web_fetch", "args": {"url": "http://127.0.0.1/"}}),
        json.dumps({"type": "tool_call", "name": "web_fetch", "args": {"url": "https://blocked.invalid/item"}}),
        '{"type":"text","text":"done"}',
    ])
    seen = []
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script), "ZHUYE_APPROVALS": "1"}) as session:
        sid = session.send({"type": "user_input", "items": [{"type": "text", "text": "测试会话放行"}]})
        def respond(event: dict[str, Any]) -> None:
            message = event.get("msg", {})
            if message.get("type") == "approval_request":
                seen.append(message)
                session.send({"type": "tool_approval", "id": message["request_id"], "decision": "approve_for_session"})
        events = session.collect(sid, timeout=DRAIN_TIMEOUT, on_event=respond)
    ends = [event["msg"] for event in events if event["msg"]["type"] == "tool_call_end"]
    assert len(ends) == 2 and len(seen) == 1, (ends, seen)
    assert ends[1]["is_error"] is True and "denied by rule" in _tool_text(ends[1]), ends[1]
    assert "turn_complete" in msg_types(events)


def test_T17_无审批门仍执行拒绝策略(serve_session, mock_script, sandbox, monkeypatch) -> None:
    """无交互审批时,资源拒绝和默认风险硬拒仍阻止工具执行。"""
    import json

    monkeypatch.delenv("ZHUYE_APPROVALS", raising=False)
    sandbox.write_config('''
[[permissions.rule]]
tool = "web_fetch"
action = "deny"
resource_glob = "https://blocked.invalid/**"
[[permissions.rule]]
tool = "read"
action = "ask"
''')
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "echo", "args": {"text": "unrelated-ok"}}),
        json.dumps({"type": "tool_call", "name": "web_fetch", "args": {"url": "https://blocked.invalid/item"}}),
        json.dumps({"type": "tool_call", "name": "bash", "args": {"cmd": "sudo echo must-not-run"}}),
        json.dumps({"type": "tool_call", "name": "read", "args": {"path": "should-not-read"}}),
        '{"type":"text","text":"done"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as session:
        events = session.submit({"type": "user_input", "items": [{"type": "text", "text": "测试无审批门策略"}]}, timeout=DRAIN_TIMEOUT)
    ends = [event["msg"] for event in events if event["msg"]["type"] == "tool_call_end"]
    assert len(ends) == 4, ends
    assert ends[0]["is_error"] is False and "unrelated-ok" in _tool_text(ends[0]), ends[0]
    for end in ends[1:]:
        assert end["is_error"] is True and "denied by rule" in _tool_text(end), end
    assert "approval_request" not in msg_types(events)
    assert "turn_complete" in msg_types(events)


def test_T18_资源规则覆盖相对路径和符号链接(serve_session, mock_script, sandbox) -> None:
    """绝对路径拒绝规则同时覆盖相对路径与指向目标目录的符号链接。"""
    import json

    private = sandbox.cwd / "private"
    private.mkdir()
    (sandbox.cwd / "alias").symlink_to(private, target_is_directory=True)
    sandbox.write_config(
        '\n[[permissions.rule]]\ntool = "write"\naction = "deny"\nresource_glob = '
        + json.dumps(str(private) + "/**") + "\n"
    )
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "write", "args": {"path": "private/a.txt", "content": "denied"}}),
        json.dumps({"type": "tool_call", "name": "write", "args": {"path": "alias/b.txt", "content": "denied"}}),
        json.dumps({"type": "tool_call", "name": "write", "args": {"path": "allowed.txt", "content": "allowed"}}),
        '{"type":"text","text":"done"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as session:
        events = session.submit({"type": "user_input", "items": [{"type": "text", "text": "测试路径权限"}]}, timeout=DRAIN_TIMEOUT)
    ends = [event["msg"] for event in events if event["msg"]["type"] == "tool_call_end"]
    assert len(ends) == 3, ends
    assert all(end["is_error"] and "denied by rule" in _tool_text(end) for end in ends[:2]), ends
    assert ends[2]["is_error"] is False, ends[2]
    assert not (private / "a.txt").exists() and not (private / "b.txt").exists()
    assert (sandbox.cwd / "allowed.txt").read_text() == "allowed"


def test_T19_无效资源拒绝规则失败关闭(serve_session, mock_script, sandbox) -> None:
    """拒绝规则的资源 glob 无效时,匹配工具仍拒绝,无关工具可运行。"""
    import json

    sandbox.write_config('''
[[permissions.rule]]
tool = "read"
action = "deny"
resource_glob = "["
''')
    (sandbox.cwd / "secret.txt").write_text("must-not-leak")
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "echo", "args": {"text": "unrelated-ok"}}),
        json.dumps({"type": "tool_call", "name": "read", "args": {"path": "secret.txt"}}),
        '{"type":"text","text":"done"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as session:
        events = session.submit({"type": "user_input", "items": [{"type": "text", "text": "测试无效资源规则"}]}, timeout=DRAIN_TIMEOUT)
    ends = [event["msg"] for event in events if event["msg"]["type"] == "tool_call_end"]
    assert len(ends) == 2, ends
    assert not ends[0]["is_error"] and "unrelated-ok" in _tool_text(ends[0]), ends[0]
    assert ends[1]["is_error"] and "denied by rule" in _tool_text(ends[1]), ends[1]
    assert "must-not-leak" not in _tool_text(ends[1])


def test_T8_沙箱拒绝写入必须报错(serve_session, mock_script, sandbox) -> None:
    """OS 沙箱拒绝 workspace 外的写入时,工具必须返回失败与非零退出码。"""
    import json
    import sys
    import uuid
    from pathlib import Path

    if sys.platform not in ("darwin", "linux"):
        pytest.skip("当前平台没有 OS 沙箱后端")
    require_binary()
    outside = Path("/etc") / ("zhuye-sandbox-" + uuid.uuid4().hex)
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "bash", "args": {"cmd": f"printf blocked > '{outside}'"}}),
        '{"type":"text","text":"done"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script), "ZHUYE_SANDBOX_OS_LEVEL": "1", "ZHUYE_SANDBOX_STRICT": "1"}) as s:
        events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "测试沙箱拒绝"}]}, timeout=DRAIN_TIMEOUT)
    end = _tool_end(events)
    assert end["is_error"] is True, end
    assert "exit_code" in end["output"]["metadata"], end
    assert end["output"]["metadata"]["exit_code"] != 0, end
    assert not outside.exists()


def test_T13_默认沙箱拒绝工作区外写入(serve_session, mock_script, monkeypatch) -> None:
    """未设置沙箱开关时,模型命令不能写入操作系统允许的工作区外路径。"""
    import json
    import os
    import sys
    import uuid
    from pathlib import Path

    if sys.platform not in ("darwin", "linux"):
        pytest.skip("当前平台没有 OS 沙箱后端")
    parent = Path("/Users/Shared") if sys.platform == "darwin" else Path("/dev/shm")
    outside = parent / ("zhuye-sandbox-default-" + uuid.uuid4().hex)
    if not os.access(outside.parent, os.W_OK):
        pytest.skip("共享目录不可写,无法区分系统权限与沙箱限制")
    for key in ("ZHUYE_SANDBOX_OS_LEVEL", "ZHUYE_SANDBOX_STRICT", "ZHUYE_UNTRUSTED_ALLOW_BARE", "ZHUYE_SANDBOX_WRITABLE"):
        monkeypatch.delenv(key, raising=False)
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "bash", "args": {"cmd": f"printf blocked > '{outside}'"}}),
        '{"type":"text","text":"done"}',
    ])
    try:
        with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
            events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "测试默认沙箱"}]}, timeout=DRAIN_TIMEOUT)
        end = _tool_end(events)
        assert end["is_error"] is True, end
        assert end["output"]["metadata"]["exit_code"] != 0, end
        assert not outside.exists()
    finally:
        outside.unlink(missing_ok=True)


def test_T14_不可信命令沙箱初始化失败不能降级(serve_session, mock_script, sandbox) -> None:
    """严格开关关闭时,模型命令的沙箱初始化失败仍必须拒绝执行。"""
    import json
    import sys

    if sys.platform != "darwin":
        pytest.skip("通过 Seatbelt 临时配置文件创建失败验证降级路径")
    marker = sandbox.cwd / "must-not-run"
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "bash", "args": {"cmd": f"touch '{marker}'"}}),
        '{"type":"text","text":"done"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script),
                            "ZHUYE_SANDBOX_OS_LEVEL": "1", "ZHUYE_SANDBOX_STRICT": "0",
                            "ZHUYE_UNTRUSTED_ALLOW_BARE": "0", "TMPDIR": str(sandbox.cwd / "missing")}) as s:
        events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "测试沙箱初始化失败"}]}, timeout=DRAIN_TIMEOUT)
    end = _tool_end(events)
    assert end["is_error"] is True, end
    assert "sandbox" in _tool_text(end).lower(), end
    assert not marker.exists()


def test_T9_macOS沙箱保护真实HOME凭据路径(serve_session, mock_script, sandbox) -> None:
    """HOME 位于临时目录时,显式敏感读拒绝仍覆盖全盘读放行规则。"""
    import json
    import sys

    if sys.platform != "darwin":
        pytest.skip("仅验证 Seatbelt 敏感路径规则")
    secret = sandbox.home / ".ssh" / "dummy-key"
    secret.parent.mkdir()
    secret.write_text("FAKE-SECRET-MUST-NOT-BE-READ")
    # 拆分目录名,让命令实际到达 Seatbelt,而非先被风险分类规则拒绝。
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "bash", "args": {"cmd": 'cat "$HOME/.s""sh/dummy-key"'}}),
        '{"type":"text","text":"done"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script), "ZHUYE_SANDBOX_OS_LEVEL": "1", "ZHUYE_SANDBOX_STRICT": "1"}) as s:
        events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "测试敏感路径"}]}, timeout=DRAIN_TIMEOUT)
    end = _tool_end(events)
    assert end["is_error"] is True, end
    assert end["output"]["metadata"]["exit_code"] != 0, end
    assert "FAKE-SECRET-MUST-NOT-BE-READ" not in _tool_text(end)


def test_T10_双流截断后保留失败退出码(serve_session, mock_script) -> None:
    """两条流超过捕获上限后,tool_call_end 仍保留原命令的失败状态。"""
    import json
    import sys

    if sys.platform not in ("darwin", "linux"):
        pytest.skip("需要 Unix head 与 /dev/zero")
    require_binary()
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "bash", "args": {
            "cmd": "head -c 110000 /dev/zero; head -c 110000 /dev/zero >&2; false"
        }}),
        '{"type":"text","text":"done"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        events = s.submit({"type": "user_input", "items": [{"type": "text", "text": "测试截断后的错误状态"}]}, timeout=DRAIN_TIMEOUT)
    end = _tool_end(events)
    assert end["is_error"] is True, end
    metadata = end["output"]["metadata"]
    assert metadata["exit_code"] == 1, metadata
    assert metadata["stdout_bytes"] == 100 * 1024, metadata
    assert metadata["stderr_bytes"] == 100 * 1024, metadata


@pytest.mark.skipif(os.name == "nt", reason="测试依赖可执行 Git shell hook")
def test_T20_干净checkpoint可回退且安全快照失败时停止(serve_session, mock_script, sandbox) -> None:
    """干净普通提交创建的 checkpoint 必须有标签,安全快照失败不得重置。"""
    import json
    import subprocess

    require_binary()
    def git(*args):
        return subprocess.run(["git", *args], cwd=sandbox.cwd, capture_output=True, text=True, check=True)
    git("init", "-q")
    git("config", "user.email", "test@localhost")
    git("config", "user.name", "test")
    target = sandbox.cwd / "tracked.txt"
    target.write_text("original\n")
    git("add", "-A")
    git("commit", "-q", "-m", "ordinary")
    script = mock_script([
        '{"type":"tool_call","name":"checkpoint","args":{"action":"create","label":"clean"}}',
        '{"type":"text","text":"created"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        end = _tool_end(s.submit({"type": "user_input", "items": [{"type": "text", "text": "保存"}]}))
    assert end["is_error"] is False, end
    sha = end["output"]["metadata"]["sha"]
    assert sha == git("rev-parse", "HEAD").stdout.strip()
    assert f"zhuye-cp/{sha}" in git("tag", "--points-at", sha).stdout
    target.write_text("unsaved changes\n")
    hook = sandbox.cwd / ".git/hooks/reference-transaction"
    hook.write_text('#!/bin/sh\nwhile read old new ref; do\n case "$ref" in refs/tags/zhuye-cp/*) exit 1;; esac\ndone\nexit 0\n')
    hook.chmod(0o755)
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "rewind", "args": {"sha": sha, "scope": "workspace"}}),
        '{"type":"text","text":"attempted"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        end = _tool_end(s.submit({"type": "user_input", "items": [{"type": "text", "text": "回退"}]}))
    assert end["is_error"] is True, end
    assert "标签创建失败" in _tool_text(end)
    assert target.read_text() == "unsaved changes\n"
    assert git("rev-parse", "HEAD").stdout.strip() != sha
    assert git("show", "HEAD:tracked.txt").stdout == "unsaved changes\n"


def test_T11_记忆整份文档拒绝条目修改(serve_session, mock_script, sandbox) -> None:
    """旧格式记忆只能整份编辑,条目写入或删除不能改变原文件。"""
    require_binary()
    script = mock_script([
        '{"type":"tool_call","name":"memory","args":{"action":"write","scope":"project","key":"one","value":"seed"}}',
        '{"type":"text","text":"seeded"}',
        '{"type":"tool_call","name":"memory","args":{"action":"write","scope":"project","key":"one","value":"changed"}}',
        '{"type":"text","text":"write attempted"}',
        '{"type":"tool_call","name":"memory","args":{"action":"remove","scope":"project","key":"one"}}',
        '{"type":"text","text":"remove attempted"}',
    ])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        submit = {"type": "user_input", "items": [{"type": "text", "text": "操作记忆"}]}
        assert _tool_end(s.submit(submit, timeout=DRAIN_TIMEOUT))["is_error"] is False
        files = list((sandbox.cwd / ".zhuye/agent-memory").glob("*/MEMORY.md"))
        assert len(files) == 1
        original = "preamble\n## one\nkeep this\n"
        files[0].write_text(original)
        for _ in range(2):
            end = _tool_end(s.submit(submit, timeout=DRAIN_TIMEOUT))
            assert end["is_error"] is True, end
            assert "use whole-document editing" in _tool_text(end), end
            assert files[0].read_text() == original


def test_T11_memory工具读写删全链路(serve_session, mock_script, sandbox) -> None:
    """mock 调内置 `memory` 工具:write → read → remove,逐轮断言
    MEMORY.md 落盘形态(`## <key>` 块)、读回内容、删除后条目消失,
    以及 rollout 的 MemoryAccess::Write 审计记录(字段级)。

    依据:crates/abilities/zhuye-memory/src/edit.rs(条目级编辑)+
    crates/runtime/zhuye-exec/src/memory_tool.rs(工具面)。
    """
    import json

    require_binary()
    script = mock_script([
        '{"type":"tool_call","name":"memory","args":{"action":"write","scope":"project",'
        '"key":"deploy","value":"发布前先跑 ./tests/run_all.sh functional"}}',
        '{"type":"text","text":"written"}',
        '{"type":"tool_call","name":"memory","args":{"action":"read","scope":"project"}}',
        '{"type":"text","text":"read"}',
        '{"type":"tool_call","name":"memory","args":{"action":"remove","scope":"project","key":"deploy"}}',
        '{"type":"text","text":"removed"}',
    ])
    memory_files = lambda: list((sandbox.cwd / ".zhuye/agent-memory").glob("*/MEMORY.md"))

    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        # 1) write:工具回执 + 落盘形态(`## deploy` 整行标题 + 正文)。
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "记一下发布流程"}]},
            timeout=DRAIN_TIMEOUT,
        )
        end = _tool_end(evs)
        assert end["is_error"] is False, f"memory write 应成功:{end}"
        assert "deploy" in _tool_text(end), f"回执未提 key:{_tool_text(end)!r}"
        files = memory_files()
        assert files, "未生成 MEMORY.md"
        body = files[0].read_text()
        assert "## deploy\n" in body, f"缺条目块:{body!r}"
        assert "发布前先跑" in body, f"条目正文缺失:{body!r}"

        # 2) read:工具输出含刚写入的正文。
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "读一下记忆"}]},
            timeout=DRAIN_TIMEOUT,
        )
        end = _tool_end(evs)
        assert end["is_error"] is False, f"memory read 应成功:{end}"
        assert "发布前先跑" in _tool_text(end), f"读回内容缺失:{_tool_text(end)!r}"

        # 3) remove:条目消失(幂等删除,文件可残留为空)。
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "删掉它"}]},
            timeout=DRAIN_TIMEOUT,
        )
        end = _tool_end(evs)
        assert end["is_error"] is False, f"memory remove 应成功:{end}"
        assert "deploy" not in files[0].read_text(), "条目未删除"

    # 4) rollout 审计:MemoryAccess::Write 记录(字段级)。
    records = [
        json.loads(line)
        for f in rollout_files(sandbox.home)
        for line in f.read_text().splitlines()
        if line.strip()
    ]
    writes = [
        r for r in records
        if r.get("type") == "memory_access" and r.get("op") == "write"
    ]
    assert writes, f"无 MemoryAccess::Write 记录:{[r.get('type') for r in records]}"
    assert writes[0]["agent_type"], "审计记录缺 agent_type"
    assert writes[0]["turn_id"], "审计记录缺 turn_id"


@pytest.mark.skipif(
    __import__("sys").platform not in ("darwin", "linux"),
    reason="当前平台没有 OS 沙箱后端",
)
def test_T12_sandbox配置下沉显式关闭与env逃生口(serve_session, mock_script, sandbox) -> None:
    """`[sandbox].os_level = false` 显式关闭必须生效(config 此前不桥接
    `Some(false)`,用户关不掉);env `ZHUYE_SANDBOX_OS_LEVEL=1` 仍可临时
    覆盖回来(逃生口)。

    依据:crates/resources/zhuye-sandbox/src/os_sandbox.rs
    `resolve_enabled`(env > config > 基线)+ headless 的
    `AgentConfig.sandbox_default` 注入(取代 unsafe set_var 桥接)。
    """
    import json
    import sys
    import uuid
    from pathlib import Path

    require_binary()
    # 目标选**OS 可写、但沙箱 allow 列表外**的路径(macOS /Users/Shared
    # 是 1777;Linux 用 /dev/shm)。不能选 HOME / TMPDIR —— Seatbelt
    # profile 对 /tmp、/private/var/folders 等临时目录本就放行,区分不出
    # 沙箱开关;也不能选 /etc(非 root 本来就写不进去)。
    outside = (Path("/Users/Shared") if sys.platform == "darwin" else Path("/dev/shm")) / (
        "zhuye-cfg-sandbox-" + uuid.uuid4().hex
    )
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "bash",
                    "args": {"cmd": f"printf leaked > '{outside}'"}}),
        '{"type":"text","text":"done"}',
    ])

    # 1) config 显式关 → 写入放行(sandbox_default=Some(false) 生效)。
    #
    # v1.9 S2:模型来源命令默认强制沙箱(config/env 关沙箱对其无效),
    # 故验证 config 下沉管道需同时打开显式逃生口
    # ZHUYE_UNTRUSTED_ALLOW_BARE=1 —— 闸门放行(BareByOverride)后,
    # Some(false) 的关闭语义才对这条 bash 生效。逃生口缺省时该场景
    # 写入会被强制沙箱拒绝(S2 的正确行为,见 zhuye-sandbox::untrusted)。
    sandbox.write_config('\n[sandbox]\nos_level = false\n')
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script),
                            "ZHUYE_SANDBOX_STRICT": "0",
                            "ZHUYE_UNTRUSTED_ALLOW_BARE": "1"}) as s:
        events = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "写文件"}]},
            timeout=DRAIN_TIMEOUT,
        )
    end = _tool_end(events)
    assert end["is_error"] is False, f"config 显式关闭后写入应放行:{end}"
    assert outside.exists(), "sandbox 未按 config 关闭(Some(false) 未生效)"
    outside.unlink(missing_ok=True)

    # 2) 同一 config + env 逃生口 → 写入被拒(env 优先)。
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script),
                            "ZHUYE_SANDBOX_OS_LEVEL": "1", "ZHUYE_SANDBOX_STRICT": "1"}) as s:
        events = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "写文件"}]},
            timeout=DRAIN_TIMEOUT,
        )
    end = _tool_end(events)
    assert end["is_error"] is True, f"env 逃生口应压掉 config 的关闭:{end}"
    assert not outside.exists(), "env=1 时写入必须被拒"


def test_T21_MCP图片到事件与模型请求(sandbox, serve_session) -> None:
    """真实 MCP 图片解码为字节,并在整批工具回执后传给模型。"""
    import base64
    import json
    import sys
    import threading
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    require_binary()
    png = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a1s8AAAAASUVORK5CYII=")
    server_script = sandbox.cwd / "image_mcp.py"
    server_script.write_text("""import json, sys
for line in sys.stdin:
    req = json.loads(line)
    if 'id' not in req:
        continue
    method = req.get('method')
    if method == 'initialize':
        result = {'protocolVersion':'2025-06-18','serverInfo':{'name':'image','version':'1'},'capabilities':{'tools':{}}}
    elif method == 'tools/list':
        result = {'tools':[{'name':'image','description':'测试图片','inputSchema':{'type':'object','properties':{}}}]}
    elif method == 'tools/call':
        result = {'content':[{'type':'image','mimeType':'image/png','data':'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a1s8AAAAASUVORK5CYII='}],'isError':False}
    else:
        result = {}
    print(json.dumps({'jsonrpc':'2.0','id':req['id'],'result':result}), flush=True)
""")
    received = []
    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            received.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            if len(received) == 1:
                delta = {"role":"assistant", "tool_calls":[
                    {"index":0, "id":"img-call", "type":"function", "function":{"name":"mcp__img__image", "arguments":"{}"}},
                    {"index":1, "id":"echo-call", "type":"function", "function":{"name":"echo", "arguments":'{"text":"done"}'}},
                ]}
                finish = "tool_calls"
            else:
                delta = {"content":"看到了"}
                finish = "stop"
            payload = {"id":"test", "model":"gpt-4o", "choices":[{"index":0, "delta":delta, "finish_reason":finish}]}
            body = ("data: " + json.dumps(payload, ensure_ascii=False) + "\n\ndata: [DONE]\n\n").encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        def log_message(self, *args):
            pass
    gateway = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=gateway.serve_forever, daemon=True)
    thread.start()
    try:
        sandbox.write_config(
            f'\n[active]\nprovider = "openai"\n[openai]\napi_key = "test"\nbase_url = "http://127.0.0.1:{gateway.server_port}"\n'
            f'\n[mcp_servers.img]\ntype = "stdio"\ncommand = {json.dumps(sys.executable)}\nargs = [{json.dumps(str(server_script))}]\n'
        )
        with serve_session(env={"ZHUYE_MODEL":"openai/gpt-4o"}) as session:
            ready = session.wait_global(lambda event: event.get("msg", {}).get("type") in {"mcp_server_started", "mcp_server_failed"} and event["msg"].get("server") == "img")
            assert ready["msg"]["type"] == "mcp_server_started", ready
            events = session.submit({"type":"user_input", "items":[{"type":"text", "text":"看图片"}]})
        assert any(event["msg"]["type"] == "turn_complete" for event in events), events
        ends = [event["msg"] for event in events if event["msg"]["type"] == "tool_call_end"]
        image_end = next(end for end in ends if end["call_id"] == "img-call")
        image = image_end["output"]["content"][0]
        assert image["type"] == "image" and image["mime_type"] == "image/png", image
        assert image["data"] == list(png), image
        assert len(received) == 2, received
        messages = received[1]["messages"]
        assistant = next(i for i, message in enumerate(messages) if message.get("tool_calls"))
        assert [message["role"] for message in messages[assistant:assistant + 4]] == ["assistant", "tool", "tool", "user"], messages
        user_parts = messages[assistant + 3]["content"]
        image_url = next(part["image_url"]["url"] for part in user_parts if part["type"] == "image_url")
        assert base64.b64decode(image_url.split(",", 1)[1]) == png
    finally:
        gateway.shutdown()
        thread.join()
        gateway.server_close()


def test_T22_Responses用户与工具图片(sandbox, serve_session) -> None:
    """Responses 实际请求携带用户图片和工具输出图片的原始字节。"""
    import base64
    import json
    import threading
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    require_binary()
    png = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a1s8AAAAASUVORK5CYII=")
    (sandbox.cwd / "sample.png").write_bytes(png)
    received = []
    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            received.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            if len(received) == 1:
                frames = [
                    ("response.output_item.added", {"item":{"type":"function_call", "call_id":"image-call", "name":"image_view"}}),
                    ("response.function_call_arguments.delta", {"delta":'{"path":"sample.png"}'}),
                    ("response.completed", {"response":{}}),
                ]
            else:
                frames = [("response.output_text.delta", {"delta":"完成"}), ("response.completed", {"response":{}})]
            body = "".join("event: " + event + "\ndata: " + json.dumps(payload, ensure_ascii=False) + "\n\n" for event, payload in frames).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        def log_message(self, *args):
            pass
    gateway = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=gateway.serve_forever, daemon=True)
    thread.start()
    try:
        sandbox.write_config(f'\n[active]\nprovider = "openai"\n[openai]\napi_key = "test"\nresponses_api = true\nbase_url = "http://127.0.0.1:{gateway.server_port}"\n')
        with serve_session(env={"ZHUYE_MODEL":"openai/gpt-4o"}) as session:
            events = session.submit({"type":"user_input", "items":[{"type":"text", "text":"看图片"}, {"type":"image", "data":list(png), "mime_type":"image/png"}]})
        assert any(event["msg"]["type"] == "turn_complete" for event in events), events
        assert len(received) == 2, received
        first_user = next(item for item in received[0]["input"] if item.get("role") == "user")
        first_image = next(item for item in first_user["content"] if item["type"] == "input_image")
        assert base64.b64decode(first_image["image_url"].split(",", 1)[1]) == png
        result = next(item for item in received[1]["input"] if item.get("type") == "function_call_output" and item.get("call_id") == "image-call")
        tool_image = next(item for item in result["output"] if item["type"] == "input_image")
        assert base64.b64decode(tool_image["image_url"].split(",", 1)[1]) == png
    finally:
        gateway.shutdown()
        thread.join()
        gateway.server_close()


def test_T23_MCP损坏图片返回错误(sandbox, mock_script, serve_session) -> None:
    """MCP 图片不是合法 Base64 时报告工具错误,且不泄露原始载荷。"""
    import json
    import sys
    require_binary()
    server_script = sandbox.cwd / "bad_image_mcp.py"
    server_script.write_text("""import json, sys
for line in sys.stdin:
    req = json.loads(line)
    if 'id' not in req: continue
    method = req.get('method')
    if method == 'initialize':
        result = {'protocolVersion':'2025-06-18','serverInfo':{'name':'bad','version':'1'},'capabilities':{'tools':{}}}
    elif method == 'tools/list':
        result = {'tools':[{'name':'image','description':'错误图片','inputSchema':{'type':'object','properties':{}}}]}
    elif method == 'tools/call':
        result = {'content':[{'type':'image','mimeType':'image/png','data':'not-base64!'}],'isError':False}
    else: result = {}
    print(json.dumps({'jsonrpc':'2.0','id':req['id'],'result':result}), flush=True)
""")
    sandbox.write_config(f'\n[mcp_servers.bad]\ntype = "stdio"\ncommand = {json.dumps(sys.executable)}\nargs = [{json.dumps(str(server_script))}]\n')
    script = mock_script(['{"type":"tool_call","name":"mcp__bad__image","args":{}}', '{"type":"text","text":"done"}'])
    with serve_session(env=sandbox.mock_env(script)) as session:
        ready = session.wait_global(lambda event: event.get("msg", {}).get("type") in {"mcp_server_started", "mcp_server_failed"} and event["msg"].get("server") == "bad")
        assert ready["msg"]["type"] == "mcp_server_started", ready
        events = session.submit({"type":"user_input", "items":[{"type":"text", "text":"读取图片"}]})
    end = _tool_end(events)
    assert end["is_error"] is True, end
    assert "无效" in _tool_text(end) and "not-base64" not in _tool_text(end), end
    assert "turn_complete" in msg_types(events), msg_types(events)


def test_T27_用户记忆随数据目录隔离(serve_session, mock_script, sandbox) -> None:
    """用户记忆只写入并读取当前 ZHUYE_HOME，不影响另一数据目录。"""
    import json

    require_binary()
    home_a = sandbox.cwd / "memory-home-a"
    home_b = sandbox.cwd / "memory-home-b"
    for home in (home_a, home_b):
        home.mkdir()
        (home / "config.toml").write_text('[hooks]\nenabled = []\n')
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "memory", "args": {"action": "write", "scope": "user", "key": "secret", "value": "仅A可见"}}),
        json.dumps({"type": "tool_call", "name": "memory", "args": {"action": "read", "scope": "user"}}),
        json.dumps({"type": "text", "text": "done"}),
    ])
    with serve_session(env=sandbox.mock_env(script, ZHUYE_HOME=str(home_a))) as session:
        events = session.submit({"type": "user_input", "items": [{"type": "text", "text": "写入记忆"}]}, timeout=DRAIN_TIMEOUT)
    ends = [event["msg"] for event in events if event["msg"]["type"] == "tool_call_end"]
    assert len(ends) == 2 and all(not end["is_error"] for end in ends), ends
    assert "仅A可见" in _tool_text(ends[1])
    files = list((home_a / "agent-memory").glob("*/MEMORY.md"))
    assert len(files) == 1 and "仅A可见" in files[0].read_text()
    assert not (home_b / "agent-memory").exists()
    assert not (sandbox.home / ".zhuye/agent-memory").exists()
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "memory", "args": {"action": "read", "scope": "user"}}),
        json.dumps({"type": "text", "text": "done"}),
    ])
    with serve_session(env=sandbox.mock_env(script, ZHUYE_HOME=str(home_b))) as session:
        events = session.submit({"type": "user_input", "items": [{"type": "text", "text": "读取记忆"}]}, timeout=DRAIN_TIMEOUT)
    end = _tool_end(events)
    assert not end["is_error"] and "仅A可见" not in _tool_text(end), end
    assert "no memory" in _tool_text(end)


def test_T28_checkpoint审计随数据目录隔离(serve_session, mock_script, sandbox) -> None:
    """checkpoint 工具在对应会话 rollout 中写入完整审计记录。"""
    import json
    import subprocess

    require_binary()
    def git(*args):
        return subprocess.run(["git", *args], cwd=sandbox.cwd, capture_output=True, text=True, check=True)
    git("init", "-q")
    git("config", "user.email", "test@localhost")
    git("config", "user.name", "test")
    (sandbox.cwd / "tracked.txt").write_text("tracked\n")
    git("add", "tracked.txt")
    git("commit", "-q", "-m", "initial")
    data = sandbox.cwd / "checkpoint-home"
    data.mkdir()
    (data / "config.toml").write_text('[hooks]\nenabled = []\n')
    script = mock_script(['{"type":"tool_call","name":"checkpoint","args":{"action":"create","label":"隔离检查"}}',
                          '{"type":"text","text":"done"}'])
    with serve_session(env=sandbox.mock_env(script, ZHUYE_HOME=str(data), ZHUYE_ROLLOUT_BASE="")) as session:
        events = session.submit({"type": "user_input", "items": [{"type": "text", "text": "保存检查点"}]}, timeout=DRAIN_TIMEOUT)
    end = _tool_end(events)
    assert not end["is_error"], end
    sha = end["output"]["metadata"]["sha"]
    files = list((data / "sessions").glob("*/*/*/*.jsonl"))
    assert len(files) == 1, files
    records = [json.loads(line) for line in files[0].read_text().splitlines() if line.strip()]
    checkpoints = [record for record in records if record.get("type") == "checkpoint"]
    assert len(checkpoints) == 1, records
    assert checkpoints[0]["sha"] == sha and checkpoints[0]["label"] == "隔离检查", checkpoints
    assert not (sandbox.home / ".zhuye/sessions").exists()
