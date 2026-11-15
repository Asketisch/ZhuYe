"""能力覆盖层 · v1.11 F5:大输出溢出落盘(spill)与 read 读回路径。

覆盖(gap-report F5,`crates/abilities/zhuye-tools/src/builtins/bash.rs`):
- 超过 `OUTPUT_LIMIT`(100 KiB)的 bash 输出 → 工具文本截断 + 落盘标记
  `[完整输出已落盘: <path>]`,路径必须在 **workspace 内**
  `<workspace>/.zhuye/spill/`(v1.11 修复点:原 /tmp 路径被 read 工具的
  workspace 边界检查挡死,「供 read 取回」承诺形同虚设);
- 落盘文件字节数 > 截断上限(完整输出,不是截断副本);
- **read 工具可读回 `.zhuye/spill/` 路径**(边界放行的 wire 级验证 ——
  与溢出场景独立成两段:溢出路径含 pid+纳秒时间戳,静态脚本无法预知,
  用固定 probe 文件验证同目录前缀的可达性)。
"""

from __future__ import annotations

import os
import re
from typing import Any

import pytest

from wire import DRAIN_TIMEOUT, first_event, msg_types, require_binary

pytestmark = pytest.mark.usefixtures("sandbox")

SPILL_MARKER = "[完整输出已落盘: "
# bash.rs 的截断上限(字节);超出即触发溢出落盘。
OUTPUT_LIMIT = 100 * 1024


def _tool_end(events: list[dict[str, Any]]) -> dict[str, Any]:
    """取 tool_call_end 事件(找不到直接失败,附上已见类型)。"""
    m = first_event(events, "tool_call_end")
    assert m is not None, f"未见 tool_call_end:{msg_types(events)}"
    return m["msg"]


def _tool_text(msg: dict[str, Any]) -> str:
    """tool_call_end 输出里的文本拼接。"""
    content = msg.get("output", {}).get("content", [])
    return "".join(c.get("text", "") for c in content if isinstance(c, dict))


def test_大输出溢出落盘到workspace且文件完整(serve_session, mock_script, sandbox) -> None:
    """>100KiB 的 bash 输出:工具文本截断 + 落盘标记;spill 文件在
    `<workspace>/.zhuye/spill/` 且字节数超过上限(完整输出)。"""
    require_binary()
    # 触发口径与 bash.rs 单测一致:单流捕获恰好封顶在 OUTPUT_LIMIT 不触发
    # 截断,必须 **stdout + stderr 合并**超限 —— stdout 大量输出(捕获被
    # 封顶在 102400)+ stderr 5000 字节,合并文本 > 102400 触发落盘。
    # 命令刻意不含引号,避免在 JSON 脚本行里做嵌套转义。
    big_cmd = "yes 0123456789 | head -c 132000; yes 0123456789 | head -c 5000 1>&2"
    script = mock_script(
        [
            f'{{"type": "tool_call", "name": "bash", "args": {{"cmd": "{big_cmd}"}}}}',
            '{"type": "text", "text": "done"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "大输出"}]},
            timeout=DRAIN_TIMEOUT,
        )
    text = _tool_text(_tool_end(evs))
    # 截断 + 落盘标记同时存在,且能解出落盘路径。
    assert SPILL_MARKER in text, f"缺落盘标记:{text[-200:]!r}"
    m = re.search(re.escape(SPILL_MARKER) + r"([^\]]+)\]", text)
    assert m is not None, f"标记里解不出路径:{text[-200:]!r}"
    spill_path = m.group(1)
    # v1.11 F5 修复口径:必须落 在workspace 内的 .zhuye/spill/ 下。
    assert ".zhuye/spill/" in spill_path.replace("\\", "/"), f"路径不在 spill 目录:{spill_path}"
    # 完整输出落盘(非截断副本):字节数必须超过截断上限。
    size = os.path.getsize(spill_path)
    assert size > OUTPUT_LIMIT, f"落盘文件仅 {size} 字节,应含完整输出(>{OUTPUT_LIMIT})"
    # 工具文本本身已被截断(不含第 11999 行的完整重复,长度远小于全量)。
    assert len(text) < size, f"工具文本({len(text)})不应大于落盘全文({size})"


def test_spill文件可被read工具读回(serve_session, mock_script) -> None:
    """read 工具读 `<workspace>/.zhuye/spill/` 下文件 → 原文返回
    (workspace 边界对该目录放行 —— v1.11 修复前 /tmp 路径会被拒)。"""
    require_binary()
    script = mock_script(
        [
            '{"type": "tool_call", "name": "bash", "args": {"cmd": "mkdir -p .zhuye/spill && printf spill-probe-marker > .zhuye/spill/probe.log"}}',
            '{"type": "tool_call", "name": "read", "args": {"path": ".zhuye/spill/probe.log"}}',
            '{"type": "text", "text": "done"}',
        ]
    )
    with serve_session(env={"ZHUYE_MODEL": "mock", "ZHUYE_MOCK_SCRIPT": str(script)}) as s:
        evs = s.submit(
            {"type": "user_input", "items": [{"type": "text", "text": "读回 spill"}]},
            timeout=DRAIN_TIMEOUT,
        )
    ends = [e["msg"] for e in evs if e.get("msg", {}).get("type") == "tool_call_end"]
    assert len(ends) >= 2, f"应有两次工具回执(bash + read):{msg_types(evs)}"
    bash_end, read_end = ends[0], ends[1]
    assert bash_end["is_error"] is False, f"probe 写入失败:{bash_end}"
    assert read_end["is_error"] is False, f"read 读回应成功(边界放行):{read_end}"
    assert "spill-probe-marker" in _tool_text(read_end), f"读回内容不符:{_tool_text(read_end)!r}"
    assert "turn_complete" in msg_types(evs), f"turn 未收尾:{msg_types(evs)}"
