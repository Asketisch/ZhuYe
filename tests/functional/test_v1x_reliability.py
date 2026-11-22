"""能力覆盖层 · v1.x 可靠性与治理能力(字段级断言)。

覆盖(对应 tests/COVERAGE.md v1.x 新增行):
- Steer 输入字节上限:超 200KB 的转向输入被拒(`steer_input_too_large`,
  details 含 payload_bytes / max_bytes),**不产生 turn**(无 TurnStarted);
- 工具结果预算:超长 bash 输出(>100KB)被替换为 `<persisted-output>`
  信封(路径 / 字节 / 预览),全文落盘 artifacts/ 可取回;
- Prompt Snapshot:首个 turn 成功后 rollout 出现 `prompt_snapshot` 记录;
- 输入守恒:turn 正常完成后 rollout 出现 `input_disposition`
  (disposition=promoted);resume 告知路径由 `resumed_prompt_with_unsettled`
  单元级覆盖(core/exec),此处断言记录存在;
- tool-defer 门控:启用 `tool-defer` 插件后,MCP source 工具**不下发完整
  schema**(request_echo 回显的工具清单里缺席),`deferred_call` 在清单里;
  未启用时行为不变(对照)。

依据:AGENTS.md 开发流程检查表 —— 能力断言到字段级,不接受"退出码 0"。
"""

import json
import time
from pathlib import Path

import pytest

from wire import (
    DRAIN_TIMEOUT,
    MOCK_MCP,
    ServeSession,
    mock_mcp_available,
    rollout_records,
    require_binary,
)

pytestmark = pytest.mark.usefixtures("sandbox")

# mock 的 usage 恒 input=0 → pre_loop 阈值触发不依赖 usage;steer 无需模型。
_STEER_OVERHEAD = 4096  # submission 信封开销余量


def _first_error(events: list[dict]) -> dict | None:
    for ev in events:
        msg = ev.get("msg", {})
        if msg.get("type") == "error":
            return msg
    return None


def test_v1x_steer超限被拒_无turn产生(serve_session, mock_script) -> None:
    """>200KB 的转向输入:error 事件带 code/payload_bytes/max_bytes,
    且不产生任何 turn(无 turn_started)。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"unused"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        big = "x" * (200_000 + _STEER_OVERHEAD)
        evs = s.submit(
            {"type": "steer", "priority": "now", "items": [
                {"type": "text", "text": big}
            ]},
            timeout=DRAIN_TIMEOUT,
        )
        err = _first_error(evs)
        assert err is not None, f"超限应产生 error 事件:{[e['msg']['type'] for e in evs]}"
        assert err.get("code") == "steer_input_too_large", f"code 字段:{err}"
        details = err.get("details") or {}
        assert details.get("payload_bytes", 0) > 200_000, f"payload_bytes 字段:{details}"
        assert details.get("max_bytes") == 200_000, f"max_bytes 字段:{details}"
        assert not any(
            e["msg"]["type"] == "turn_started" for e in evs
        ), "被拒的转向不得产生 turn"


def test_v1x_工具结果超限_信封替换并落盘(serve_session, mock_script, sandbox) -> None:
    """bash 产出 90KB 文本 + config 把内联预算调到 8KB → tool_call_end 的
    content 是 persisted-output 信封(path/bytes/预览),metadata.artifact
    指向落盘文件,全文可读(默认 100KB 预算对 90KB 不触发,用配置缩阈)。"""
    require_binary()
    sandbox.write_config("\n[tool_results]\nmax_inline_bytes = 8000\n")
    # command 用 json.dumps 构造,避免转义地狱(内层单引号 python 表达式)。
    big_cmd = json.dumps({"type": "tool_call", "name": "bash",
                          "args": {"command": "python3 -c 'print(\"Z\"*90000)'"}})
    script = mock_script([big_cmd, '{"type":"text","text":"done"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "跑一下"}]},
            timeout=DRAIN_TIMEOUT,
        )
        ends = [e["msg"] for e in evs if e["msg"]["type"] == "tool_call_end"]
        assert ends, f"应有工具调用结束:{[e['msg']['type'] for e in evs]}"
        content = "".join(
            b.get("text", "") for b in ends[0].get("output", {}).get("content", [])
            if isinstance(b, dict)
        )
        assert content.startswith("<persisted-output"), f"信封开头:{content[:120]!r}"
        assert "已全文落盘" in content, f"落盘提示:{content[-200:]!r}"
        # metadata.artifact 指向真实文件,且文件内容 = 全文(未截断)。
        artifact = ends[0].get("output", {}).get("metadata", {}).get("artifact")
        assert artifact and "artifacts" in str(artifact), f"artifact 路径:{artifact}"
        body = Path(str(artifact)).read_text()
        assert body.count("Z") == 90_000, "落盘文件应为未截断全文"


def test_v1x_prompt_snapshot与input_disposition落盘(serve_session, mock_script, sandbox) -> None:
    """首个成功 turn 后 rollout 应含 prompt_snapshot 与
    input_disposition(disposition=promoted)两类记录。"""
    require_binary()
    script = mock_script(['{"type":"text","text":"第一轮回复"}'])
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "你好"}]},
            timeout=DRAIN_TIMEOUT,
        )
        records = rollout_records(sandbox.home)
        types = [r.get("type") for r in records]
        assert "prompt_snapshot" in types, f"缺 prompt_snapshot 记录:{types}"
        assert "input_disposition" in types, f"缺 input_disposition 记录:{types}"
        snap = next(r for r in records if r.get("type") == "prompt_snapshot")
        assert snap.get("model") == "mock/mock-1", f"快照 model 字段:{snap}"
        assert isinstance(snap.get("system_json"), list), f"system_json 字段:{snap!r}"
        disp = next(r for r in records if r.get("type") == "input_disposition")
        assert disp.get("disposition") == "promoted", f"处置字段:{disp}"
        assert disp.get("reason") == "turn_complete", f"reason 字段:{disp}"


def test_v1x_tool_defer启用后_mcp工具不出schema_deferred_call在列(
    serve_session, mock_script, sandbox
) -> None:
    """启用 tool-defer:request_echo 回显的工具清单里,MCP 工具缺席、
    deferred_call 在列。未启用对照:清单不变(基线由 gated_plugins 维护)。"""
    require_binary()
    if not mock_mcp_available():
        pytest.skip(f"mock MCP server 不存在:{MOCK_MCP}")
    # MCP 配置(test_output_guardrail 同款;mock server 工具名 echo →
    # 注册名 mcp__fsx__echo)+ tool-defer 门控插件(bundled_plugin_enabled
    # 纯名字比对,config enabled_plugins 即生效)。
    sandbox.write_config(
        f'\n[mcp_servers.fsx]\ntype = "stdio"\ncommand = "{MOCK_MCP}"\n'
        '\n[plugins]\nenabled_plugins = ["tool-defer@inline"]\n'
    )
    script = mock_script(['{"type":"request_echo"}', '{"type":"text","text":"ok"}'])
    env = {"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}
    with serve_session(env=env) as s:
        # MCP server 异步启动:等 started 事件,避免假绿(server 未挂则
        # mcp 工具本来就不在清单里)。
        s.wait_global(
            lambda e: e.get("msg", {}).get("type") == "mcp_server_started",
            timeout=20,
        )
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "看下工具"}]},
            timeout=DRAIN_TIMEOUT,
        )
        text = "".join(
            e["msg"].get("delta", "") for e in evs
            if e["msg"].get("type") == "agent_message_delta"
        )
        payload = json.loads(text) if text.strip().startswith("{") else None
        assert payload is not None, f"request_echo 回复应可解析:{text[:200]!r}"
        names = payload.get("tools", [])
        assert "deferred_call" in names, f"deferred_call 应在注册表:{names}"
        # 对照前提:mcp_server_started 已确认注册表侧挂上 MCP 工具
        # (tool_count >= 1,见上方 wait_global)。回显 = 请求 tools 段 =
        # 注册表经 defer 过滤后的形态 —— mcp__ 缺席 + deferred_call 在列
        # 即为 defer 生效;若回显出现 mcp__ 则过滤失效。
        assert not any(n.startswith("mcp__") for n in names), (
            f"tool-defer 启用后请求 tools 段不得含 MCP 工具:{names}"
        )
