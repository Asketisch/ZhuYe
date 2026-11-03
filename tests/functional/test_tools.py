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
    assert end["output"]["metadata"]["exit_code"] != 0, end
    assert not outside.exists()


def test_T9_macOS沙箱保护真实HOME凭据路径(serve_session, mock_script, sandbox) -> None:
    """HOME 位于临时目录时,显式敏感读拒绝仍覆盖全盘读放行规则。"""
    import json
    import sys

    if sys.platform != "darwin":
        pytest.skip("仅验证 Seatbelt 敏感路径规则")
    secret = sandbox.home / ".ssh" / "dummy-key"
    secret.parent.mkdir()
    secret.write_text("FAKE-SECRET-MUST-NOT-BE-READ")
    script = mock_script([
        json.dumps({"type": "tool_call", "name": "bash", "args": {"cmd": f"cat '{secret}'"}}),
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
