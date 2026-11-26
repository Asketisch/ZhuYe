"""能力覆盖层 · 底层硬化:沙箱边界、进程组清理、MCP 互操作。

覆盖演进规划中的 P0/P1 验收路径(不经过 SDK,直接打 serve JSONL):
- Linux 沙箱失败关闭与网络策略上报;
- shell 动态语法不因宽放行规则自动执行;
- 前台超时/中断与宿主断开后的同组后代清理;
- bash 输出报告 path_protection;
- MCP elicitation / 工具清单刷新 / 资源内容 / 取消通知 / 启动失败。
"""

from __future__ import annotations

import json
import os
import socket
import sys
import time
from typing import Any

import pytest

from wire import DRAIN_TIMEOUT, MOCK_MCP, first_event, msg_types, require_binary

pytestmark = pytest.mark.usefixtures("sandbox")


def _tool_end(events: list[dict[str, Any]]) -> dict[str, Any]:
    event = first_event(events, "tool_call_end")
    assert event is not None, f"未见 tool_call_end:{msg_types(events)}"
    return event["msg"]


def _tool_ends(events: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [event["msg"] for event in events if event.get("msg", {}).get("type") == "tool_call_end"]


def _tool_text(msg: dict[str, Any]) -> str:
    content = msg.get("output", {}).get("content", [])
    return "".join(block.get("text", "") for block in content if isinstance(block, dict))


def _wait_for(path: Any, timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.exists():
            return
        time.sleep(0.05)
    raise AssertionError(f"{timeout}s 内未出现 {path}")


# 开启 elicitation 的默认 form 参数(与 mock_mcp_server::default_elicitation_params 对齐)。
_DEFAULT_ELICITATION = {
    "mode": "form",
    "message": "请补充信息",
    "requestedSchema": {
        "type": "object",
        "properties": {
            "answer": {
                "type": "string",
                "title": "回答",
                "description": "测试用输入",
            },
        },
        "required": ["answer"],
    },
}


def _require_mock_mcp() -> None:
    if not MOCK_MCP.is_file():
        pytest.skip("mock_mcp_server 未构建(cargo build -p zhuye-mcp --bin mock_mcp_server)")


def _mcp_ready(session: Any, server: str):
    ready = session.wait_global(
        lambda e: e.get("msg", {}).get("type") in {"mcp_server_started", "mcp_server_failed"}
        and e["msg"].get("server") == server
    )
    assert ready["msg"]["type"] == "mcp_server_started", ready
    return ready


@pytest.mark.skipif(sys.platform != "linux", reason="Linux seccomp 策略")
def test_linux非严格联网沙箱失败不能裸执行(serve_session, mock_script, sandbox) -> None:
    """即使允许联网并关闭 strict，无法安装文件沙箱也必须拒绝执行。"""
    require_binary()
    marker = sandbox.cwd / "must-not-run"
    missing = sandbox.cwd / "missing-writable"
    sandbox.write_config(
        f"\n[sandbox]\nallow_network = true\nwritable_dirs = [{json.dumps(str(missing))}]\n"
    )
    script = mock_script(
        [
            json.dumps({"type": "tool_call", "name": "bash", "args": {"cmd": f"touch '{marker}'"}}),
            '{"type":"text","text":"done"}',
        ]
    )
    with serve_session(
        env=sandbox.mock_env(
            script,
            ZHUYE_SANDBOX_OS_LEVEL="1",
            ZHUYE_SANDBOX_STRICT="0",
            ZHUYE_UNTRUSTED_ALLOW_BARE="1",
        )
    ) as session:
        events = session.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "执行命令"}]},
            timeout=DRAIN_TIMEOUT,
        )
    end = _tool_end(events)
    assert end["is_error"] is True, end
    assert "sandbox" in _tool_text(end).lower(), end
    assert not marker.exists()


@pytest.mark.skipif(sys.platform != "linux", reason="Linux seccomp 策略")
@pytest.mark.parametrize("allow", [False, True])
def test_linux沙箱网络策略经前台工具生效(serve_session, mock_script, sandbox, allow) -> None:
    """本地 TCP 连接随沙箱网络开关被阻止或放行。"""
    require_binary()
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        listener.settimeout(0.2)
        sandbox.write_config(f"\n[sandbox]\nallow_network = {str(allow).lower()}\n")
        command = f"bash -c 'echo probe > /dev/tcp/127.0.0.1/{listener.getsockname()[1]}'"
        script = mock_script(
            [
                json.dumps({"type": "tool_call", "name": "bash", "args": {"cmd": command}}),
                '{"type":"text","text":"done"}',
            ]
        )
        with serve_session(env=sandbox.mock_env(script)) as session:
            events = session.submit(
                {"type": "user_input", "items": [{"type": "text", "text": "网络策略"}]},
                timeout=DRAIN_TIMEOUT,
            )
        end = _tool_end(events)
        assert end["is_error"] is not allow, end
        assert (end["output"]["metadata"]["exit_code"] == 0) is allow, end
        assert end["output"]["metadata"]["network_restriction"] == (
            "unrestricted" if allow else "blocked_sockets"
        ), end
        if allow:
            connection, _ = listener.accept()
            with connection:
                assert connection.recv(16) == b"probe\n"
        else:
            with pytest.raises(TimeoutError):
                listener.accept()


@pytest.mark.skipif(os.name == "nt", reason="Unix 进程组行为")
@pytest.mark.parametrize("mode", ["timeout", "interrupt"])
def test_bash中断清理后台后代(serve_session, mock_script, sandbox, mode) -> None:
    """前台 bash 超时或中断后，后台后代不能继续写入。"""
    require_binary()
    ready = sandbox.cwd / "descendant-ready"
    marker = sandbox.cwd / "descendant-escaped"
    cmd = f"(printf ready > '{ready}'; sleep 2; printf escaped > '{marker}') & wait"
    args: dict[str, Any] = {"cmd": cmd}
    if mode == "timeout":
        args["timeout_ms"] = 500
    script = mock_script(
        [
            json.dumps({"type": "tool_call", "name": "bash", "args": args}),
            '{"type":"text","text":"done"}',
        ]
    )
    with serve_session(env=sandbox.mock_env(script)) as session:
        turn = session.send({"type": "user_input", "items": [{"type": "text", "text": "运行命令"}]})
        _wait_for(ready)
        if mode == "interrupt":
            interrupt = session.send({"type": "interrupt"})
            assert "submission_closed" in msg_types(session.collect(interrupt, timeout=10))
        events = session.collect(turn, timeout=10)
        if mode == "timeout":
            assert _tool_end(events)["is_error"] is True, events
        else:
            assert "turn_aborted" in msg_types(events), events
        time.sleep(2.4)
        assert not marker.exists(), "命令结束后后台后代仍在执行"


@pytest.mark.skipif(os.name == "nt", reason="Unix 进程组行为")
def test_宿主断开清理后台命令后代(serve_session, mock_script, sandbox) -> None:
    """后台任务启动后宿主关闭输入，后代不能在会话退出后继续写入。"""
    require_binary()
    ready = sandbox.cwd / "background-ready"
    marker = sandbox.cwd / "background-escaped"
    cmd = f"(printf ready > '{ready}'; sleep 2; printf escaped > '{marker}') & wait"
    script = mock_script(
        [
            json.dumps(
                {
                    "type": "tool_call",
                    "name": "bash",
                    "args": {"cmd": cmd, "run_in_background": True},
                }
            ),
            '{"type":"text","text":"done"}',
        ]
    )
    with serve_session(env=sandbox.mock_env(script)) as session:
        events = session.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "运行后台命令"}]},
            timeout=DRAIN_TIMEOUT,
        )
        end = _tool_end(events)
        assert end["is_error"] is False and "task" in _tool_text(end).lower(), end
        _wait_for(ready)
        assert session._proc.stdin is not None
        session._proc.stdin.close()
        assert session._proc.wait(timeout=10) == 0
        time.sleep(2.4)
        assert not marker.exists(), "会话退出后后台后代仍在执行"


@pytest.mark.parametrize("command", ["echo $(touch injected-marker)", "echo ok > injected-marker"])
def test_shell放行规则对动态语法要求审批(serve_session, mock_script, sandbox, command) -> None:
    """命中放行规则的动态 shell 语法仍需审批,拒绝后不得执行。"""
    require_binary()
    sandbox.write_config(
        '''
[[permissions.rule]]
tool = "bash"
action = "allow"
shell_pattern = "echo *"
'''
    )
    script = mock_script(
        [
            json.dumps({"type": "tool_call", "name": "bash", "args": {"cmd": command}}),
            '{"type":"text","text":"done"}',
        ]
    )
    seen: list[dict[str, Any]] = []
    with serve_session(
        env=sandbox.mock_env(script, ZHUYE_APPROVALS="1")
    ) as session:
        sid = session.send({"type": "user_input", "items": [{"type": "text", "text": "测试规则"}]})

        def respond(event: dict[str, Any]) -> None:
            msg = event.get("msg", {})
            if msg.get("type") == "approval_request":
                seen.append(msg)
                session.send(
                    {
                        "type": "tool_approval",
                        "id": msg["request_id"],
                        "decision": {"deny": {"reason": "review-denied"}},
                    }
                )

        events = session.collect(sid, timeout=DRAIN_TIMEOUT, on_event=respond)
    assert len(seen) == 1, seen
    end = _tool_end(events)
    assert end["is_error"] is True and "review-denied" in _tool_text(end), end
    assert not (sandbox.cwd / "injected-marker").exists()


def test_bash输出报告path_protection(serve_session, mock_script, sandbox) -> None:
    """bash 成功执行后,metadata.path_protection 必须同时给出 enforced/unsupported。"""
    require_binary()
    script = mock_script(
        [
            json.dumps({"type": "tool_call", "name": "bash", "args": {"cmd": "printf ok"}}),
            '{"type":"text","text":"done"}',
        ]
    )
    with serve_session(env=sandbox.mock_env(script)) as session:
        events = session.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "报告保护"}]},
            timeout=DRAIN_TIMEOUT,
        )
    end = _tool_end(events)
    assert end["is_error"] is False, end
    path_protection = end["output"]["metadata"]["path_protection"]
    assert isinstance(path_protection, dict), path_protection
    assert "enforced" in path_protection and "unsupported" in path_protection, path_protection
    assert isinstance(path_protection["enforced"], list), path_protection
    assert isinstance(path_protection["unsupported"], list), path_protection


@pytest.mark.parametrize("method", ["initialize", "tools/list"])
def test_MCP初始化超时上报宿主(serve_session, mock_script, sandbox, method) -> None:
    """本地 MCP 服务在握手或首次列工具时无响应，宿主收到超时失败事件。"""
    require_binary()
    _require_mock_mcp()
    sandbox.write_config(
        f'''
[mcp_servers.slow]
type = "stdio"
command = {json.dumps(str(MOCK_MCP))}
timeout_ms = 750
[mcp_servers.slow.env]
MOCK_PENDING_METHOD = {json.dumps(method)}
'''
    )
    script = mock_script(['{"type":"text","text":"done"}'])
    with serve_session(env=sandbox.mock_env(script)) as session:
        event = session.wait_global(
            lambda e: e.get("msg", {}).get("type") == "mcp_server_failed"
            and e["msg"].get("server") == "slow",
            timeout=10,
        )
        assert event["msg"]["will_retry"] is False, event
        assert "timeout" in event["msg"]["error"].lower(), event
        result = session.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "继续"}]},
            timeout=DRAIN_TIMEOUT,
        )
        assert "turn_complete" in msg_types(result), result


def test_MCP远程启动失败不虚报重试(serve_session, mock_script, sandbox) -> None:
    """不响应的本地 HTTP 服务触发超时，事件不承诺不存在的重试。"""
    require_binary()
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        sandbox.write_config(
            f'''
[mcp_servers.remote]
type = "http"
url = "http://127.0.0.1:{listener.getsockname()[1]}/mcp"
timeout_ms = 750
'''
        )
        script = mock_script(['{"type":"text","text":"done"}'])
        with serve_session(env=sandbox.mock_env(script)) as session:
            event = session.wait_global(
                lambda e: e.get("msg", {}).get("type") == "mcp_server_failed"
                and e["msg"].get("server") == "remote",
                timeout=10,
            )
            assert event["msg"]["will_retry"] is False, event
            assert "timeout" in event["msg"]["error"].lower(), event


def test_MCP资源正文与二进制完整传递(serve_session, mock_script, sandbox) -> None:
    """本地 MCP 服务的资源列表、混合资源内容和嵌入资源到达宿主。"""
    require_binary()
    _require_mock_mcp()
    sandbox.write_config(
        f'''
[mcp_servers.res]
type = "stdio"
command = {json.dumps(str(MOCK_MCP))}
[mcp_servers.res.env]
MOCK_RESOURCES = "1"
MOCK_LIST_CHANGED = "1"
'''
    )
    script = mock_script(
        [
            json.dumps({"type": "tool_call", "name": "ListMcpResources", "args": {}}),
            json.dumps(
                {
                    "type": "tool_call",
                    "name": "ReadMcpResource",
                    "args": {"server": "res", "uri": "file:///mixed"},
                }
            ),
            json.dumps({"type": "tool_call", "name": "mcp__res__extra", "args": {}}),
            '{"type":"text","text":"done"}',
        ]
    )
    with serve_session(env=sandbox.mock_env(script)) as session:
        _mcp_ready(session, "res")
        events = session.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "读取资源"}]},
            timeout=DRAIN_TIMEOUT,
        )
    ends = _tool_ends(events)
    assert len(ends) == 3 and all(not end["is_error"] for end in ends), ends
    assert "file:///mixed" in _tool_text(ends[0]), ends[0]
    output = ends[1]["output"]
    assert output["content"][0]["text"] == "资源正文", output
    assert '"blob":"AAEC/w=="' in output["content"][1]["text"], output
    assert output["metadata"]["content_descriptors"] == [
        {"uri": "file:///text", "mime_type": "text/plain", "kind": "text"},
        {
            "uri": "file:///blob",
            "mime_type": "application/octet-stream",
            "kind": "blob",
            "encoding": "base64",
        },
    ], output
    assert "嵌入正文" in _tool_text(ends[2]) and "file:///embedded" in _tool_text(ends[2]), ends[2]
    assert "关联资源" in _tool_text(ends[2]) and "file:///linked" in _tool_text(ends[2]), ends[2]


def test_MCP资源读取中断通知服务端(serve_session, mock_script, sandbox) -> None:
    """宿主中断资源读取时，服务端收到该请求的取消通知。"""
    require_binary()
    _require_mock_mcp()
    pending = sandbox.cwd / "resource-pending"
    cancelled = sandbox.cwd / "resource-cancelled"
    sandbox.write_config(
        f'''
[mcp_servers.pending]
type = "stdio"
command = {json.dumps(str(MOCK_MCP))}
[mcp_servers.pending.env]
MOCK_RESOURCES = "1"
MOCK_RESOURCE_PENDING_MARKER = {json.dumps(str(pending))}
MOCK_CANCEL_MARKER = {json.dumps(str(cancelled))}
'''
    )
    script = mock_script(
        [
            json.dumps(
                {
                    "type": "tool_call",
                    "name": "ReadMcpResource",
                    "args": {"server": "pending", "uri": "file:///slow"},
                }
            )
        ]
    )
    with serve_session(env=sandbox.mock_env(script)) as session:
        _mcp_ready(session, "pending")
        turn = session.send({"type": "user_input", "items": [{"type": "text", "text": "读取慢资源"}]})
        _wait_for(pending)
        interrupt = session.send({"type": "interrupt"})
        events = session.collect(interrupt, timeout=10)
        _wait_for(cancelled)
        notification = json.loads(cancelled.read_text())
        assert notification["params"]["requestId"] == json.loads(pending.read_text()), notification
        assert "submission_closed" in msg_types(events), events
        turn_events = session.collect(turn, timeout=10)
        assert "turn_aborted" in msg_types(turn_events), turn_events


def test_MCP调用中断开返回错误(serve_session, mock_script, sandbox) -> None:
    """MCP 服务在工具调用中退出时，宿主收到失败结果并结束回合。"""
    require_binary()
    _require_mock_mcp()
    sandbox.write_config(
        f'''
[mcp_servers.lost]
type = "stdio"
command = {json.dumps(str(MOCK_MCP))}
[mcp_servers.lost.env]
MOCK_DISCONNECT_ON_CALL = "1"
'''
    )
    script = mock_script(
        [
            json.dumps({"type": "tool_call", "name": "mcp__lost__echo", "args": {"text": "x"}}),
            '{"type":"text","text":"done"}',
        ]
    )
    with serve_session(env=sandbox.mock_env(script)) as session:
        ready = _mcp_ready(session, "lost")
        assert "mcp__lost__echo" in ready["msg"]["tool_names"], ready
        events = session.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "调用工具"}]},
            timeout=DRAIN_TIMEOUT,
        )
    end = _tool_end(events)
    assert end["is_error"] is True, end
    assert "turn_complete" in msg_types(events), msg_types(events)


def test_MCP用户交互请求回执驱动工具结果(serve_session, mock_script, sandbox) -> None:
    """MCP server 发起 elicitation:宿主收到请求并回执,工具结果带回用户输入。"""
    require_binary()
    _require_mock_mcp()
    sandbox.write_config(
        f'''
[mcp_servers.elicit]
type = "stdio"
command = {json.dumps(str(MOCK_MCP))}
[mcp_servers.elicit.env]
MOCK_ELICITATION = {json.dumps(json.dumps(_DEFAULT_ELICITATION, ensure_ascii=False))}
'''
    )
    script = mock_script(
        [
            json.dumps(
                {
                    "type": "tool_call",
                    "name": "mcp__elicit__echo",
                    "args": {"text": "prefix"},
                }
            ),
            '{"type":"text","text":"done"}',
        ]
    )
    with serve_session(env=sandbox.mock_env(script)) as session:
        _mcp_ready(session, "elicit")
        sub_id = session.send({"type": "user_input", "items": [{"type": "text", "text": "调用工具"}]})
        request = session.wait_global(lambda e: e.get("msg", {}).get("type") == "mcp_elicitation")
        payload = request["msg"]
        assert payload["server"] == "elicit", payload
        assert payload["message"] == "请补充信息", payload
        assert payload["kind"]["type"] == "form", payload
        fields = payload["kind"]["fields"]
        assert [field["name"] for field in fields] == ["answer"], fields
        assert fields[0]["required"] is True, fields
        assert fields[0]["kind"] == {"type": "text", "secret": False}, fields
        session.send(
            {
                "type": "mcp_elicitation_response",
                "id": payload["request_id"],
                "action": "accept",
                "content": {"answer": "宿主答复"},
            }
        )
        events = session.collect(sub_id)
    end = _tool_end(events)
    assert end["is_error"] is False, end
    text = _tool_text(end)
    assert text.startswith("prefix|elicit:"), text
    assert "宿主答复" in text, text
    assert "accept" in text, text
    assert "turn_complete" in msg_types(events), msg_types(events)


def test_MCP用户交互请求超时按取消了结(serve_session, mock_script, sandbox) -> None:
    """宿主不回执时,elicitation 在死线到期后按 cancel 了结,回合不再挂起。"""
    require_binary()
    _require_mock_mcp()
    sandbox.write_config(
        f'''
[mcp_servers.elicit]
type = "stdio"
command = {json.dumps(str(MOCK_MCP))}
[mcp_servers.elicit.env]
MOCK_ELICITATION = {json.dumps(json.dumps(_DEFAULT_ELICITATION, ensure_ascii=False))}
'''
    )
    script = mock_script(
        [
            json.dumps(
                {
                    "type": "tool_call",
                    "name": "mcp__elicit__echo",
                    "args": {"text": "waiting"},
                }
            ),
            '{"type":"text","text":"done"}',
        ]
    )
    with serve_session(
        env=sandbox.mock_env(script, ZHUYE_MCP_ELICITATION_TTL_SECS="1")
    ) as session:
        _mcp_ready(session, "elicit")
        events = session.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "调用工具"}]},
            timeout=DRAIN_TIMEOUT,
        )
    end = _tool_end(events)
    assert end["is_error"] is False, end
    text = _tool_text(end)
    assert "waiting|elicit:" in text, text
    assert "cancel" in text, text
    assert "turn_complete" in msg_types(events), msg_types(events)


def test_MCP工具清单变化后注册表同步(serve_session, mock_script, sandbox) -> None:
    """服务端通知工具清单变化后:新工具可调用,下线的工具不再可见。"""
    require_binary()
    _require_mock_mcp()
    sandbox.write_config(
        f'''
[mcp_servers.fsx]
type = "stdio"
command = {json.dumps(str(MOCK_MCP))}
[mcp_servers.fsx.env]
MOCK_LIST_CHANGED = "1"
'''
    )
    script = mock_script(
        [
            '{"type":"tool_call","name":"mcp__fsx__extra","args":{}}',
            '{"type":"tool_call","name":"mcp__fsx__slow","args":{}}',
            '{"type":"text","text":"done"}',
        ]
    )
    with serve_session(env=sandbox.mock_env(script)) as session:
        _mcp_ready(session, "fsx")
        events = session.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "调用工具"}]},
            timeout=DRAIN_TIMEOUT,
        )
    ends = _tool_ends(events)
    assert len(ends) == 2, [event["msg"]["type"] for event in events]
    assert ends[0]["is_error"] is False, ends[0]
    assert "extra called" in _tool_text(ends[0]), ends[0]
    begins = [event["msg"] for event in events if event["msg"]["type"] == "tool_call_begin"]
    assert begins[0]["tool_name"] == "mcp__fsx__extra", begins
    assert ends[1]["is_error"] is True, ends[1]
    assert "mcp__fsx__slow" in json.dumps(ends[1], ensure_ascii=False), ends[1]
    assert "turn_complete" in msg_types(events), msg_types(events)
