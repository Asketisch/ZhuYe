"""能力覆盖层 · browser 插件 shim(降级路径,CI 安全:不依赖真实浏览器)。

覆盖(全部字段级断言,直接以原始 wire 对话 shim 二进制):
- initialize:协议版本 / serverInfo 名 / capabilities.tools 结构
- tools/list:8 个工具、`agent_browser_*` 命名、每个都带 inputSchema
- tools/call:降级态统一 isError,文案含可操作修复路径
  (ZHUYE_BROWSER_EXECUTABLE / Chrome 安装提示)
- ping / 未知 method:结构化应答,不 panic 不挂死

降级注入方式:`ZHUYE_BROWSER_EXECUTABLE` 指向不存在的路径 —— 显式配置
错误必须报错(不静默换路),于是 ensure_browser 第一步即失败,进入
BrowserMissing 降级分支;沙箱隔离的 ZHUYE_HOME 保证无残留
DevToolsActivePort 可 attach。

运行:ZHUYE_BIN=... python3 -m pytest tests/functional/runtime/test_browser_shim.py -v
"""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

import pytest

from wire import Sandbox

pytestmark = pytest.mark.usefixtures("sandbox")

PROTOCOL_VERSION = "2025-06-18"
EXPECTED_TOOLS = [
    "agent_browser_open",
    "agent_browser_snapshot",
    "agent_browser_click",
    "agent_browser_fill",
    "agent_browser_press",
    "agent_browser_scroll",
    "agent_browser_screenshot",
    "agent_browser_close",
]


def _shim_bin() -> Path:
    """shim 与被测主二进制同目录(run_all.sh 的 --bins 构建保证存在)。"""
    zhuye_bin = Path(
        os.environ.get("ZHUYE_BIN", "target/release/zhuye")
    ).resolve()
    shim = zhuye_bin.parent / "zhuye-browser-server"
    if not shim.is_file():
        pytest.skip(f"缺少 shim 二进制:{shim}(先跑 tests/run_all.sh 或 cargo build)")
    return shim


def _degrade_env(sandbox: Sandbox) -> dict[str, str]:
    """隔离 env + 指向不存在浏览器的显式覆盖(确定性降级注入)。"""
    return sandbox.env(
        extra={"ZHUYE_BROWSER_EXECUTABLE": str(sandbox.home / "no-such-browser")}
    )


class _ShimSession:
    """与 shim 的 line-delimited JSON-RPC 会话(按 id 匹配应答)。"""

    def __init__(self, shim: Path, env: dict[str, str]) -> None:
        self.proc = subprocess.Popen(
            [str(shim)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            text=True,
        )
        self._next_id = 0

    def call(self, method: str, params: dict | None = None, timeout: float = 15) -> dict:
        self._next_id += 1
        req_id = self._next_id
        msg = {"jsonrpc": "2.0", "id": req_id, "method": method, "params": params or {}}
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()
        assert self.proc.stdout is not None
        while True:
            line = self.proc.stdout.readline()
            if not line:
                err = self.proc.stderr.read() if self.proc.stderr else ""
                raise RuntimeError(f"shim stdout EOF(可能崩溃): {err[:400]}")
            resp = json.loads(line)
            if resp.get("id") == req_id:
                return resp

    def close(self) -> None:
        if self.proc.stdin:
            self.proc.stdin.close()
        self.proc.wait(timeout=10)


@pytest.fixture()
def degraded(sandbox: Sandbox) -> _ShimSession:
    """降级态 shim 会话:浏览器不可用(显式坏路径注入)。"""
    session = _ShimSession(_shim_bin(), _degrade_env(sandbox))
    yield session
    session.close()


def test_initialize_协议结构与版本(degraded: _ShimSession) -> None:
    """initialize 字段级:协议版本、serverInfo 名、tools capability。"""
    resp = degraded.call("initialize", {"protocolVersion": PROTOCOL_VERSION})
    result = resp["result"]
    assert result["protocolVersion"] == PROTOCOL_VERSION
    assert result["serverInfo"]["name"] == "zhuye-browser"
    assert result["capabilities"]["tools"] == {"listChanged": False}


def test_tools_list_上游命名八工具带schema(degraded: _ShimSession) -> None:
    """tools/list 字段级:8 个 agent_browser_* 工具,全部带 inputSchema。"""
    resp = degraded.call("tools/list")
    tools = resp["result"]["tools"]
    names = [t["name"] for t in tools]
    assert len(tools) == len(EXPECTED_TOOLS), f"工具数不符: {names}"
    for expected in EXPECTED_TOOLS:
        assert expected in names, f"缺 {expected}"
    for tool in tools:
        assert isinstance(tool["inputSchema"], dict), f"{tool['name']} 缺 inputSchema"
        assert tool["inputSchema"].get("type") == "object"


def test_tools_call_降级统一isError含修复指引(degraded: _ShimSession) -> None:
    """降级态调用必须 isError,文案含可操作修复路径(字段级)。"""
    resp = degraded.call(
        "tools/call", {"name": "agent_browser_snapshot", "arguments": {}}
    )
    result = resp["result"]
    assert result["isError"] is True, f"降级调用应失败: {result}"
    text = result["content"][0]["text"]
    assert "ZHUYE_BROWSER_EXECUTABLE" in text, f"提示应含浏览器路径覆盖 env: {text}"
    assert "Chrome" in text, f"提示应含浏览器安装指引: {text}"
    assert "agent_browser_snapshot" in text, f"提示应回显工具名: {text}"


def test_tools_call_多工具一致降级(degraded: _ShimSession) -> None:
    """任意工具在降级态行为一致:isError=true,不 panic。"""
    for name in ("agent_browser_open", "agent_browser_click", "agent_browser_close"):
        resp = degraded.call(
            "tools/call", {"name": name, "arguments": {"url": "https://example.com"}}
        )
        assert resp["result"]["isError"] is True, f"{name} 应降级失败"


def test_ping与未知method结构化应答(degraded: _ShimSession) -> None:
    """ping 应空对象;未知 method 报 not implemented 且 isError。"""
    resp = degraded.call("ping")
    assert resp["result"] == {}
    resp = degraded.call("resources/list")
    assert resp["result"]["isError"] is True
    assert "resources/list" in resp["result"]["content"][0]["text"]


def test_shim_宿主断开即退出(sandbox: Sandbox) -> None:
    """宿主关闭 stdin 后 shim 必须自行退出(不留孤儿进程;降级态同验)。"""
    session = _ShimSession(_shim_bin(), _degrade_env(sandbox))
    session.call("initialize", {"protocolVersion": PROTOCOL_VERSION})
    session.close()
    assert session.proc.returncode is not None, "shim 应随 stdin 关闭退出"
