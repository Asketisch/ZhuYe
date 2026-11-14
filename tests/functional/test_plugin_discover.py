"""能力覆盖层 · v1.11 基建#13:GitHub topic 插件发现。

覆盖:
- 参数面(离线):`zhuye plugin marketplace discover --help` 暴露
  `--topic`(默认 `zhuye-plugin`)与 `--limit`(默认 20)
- 网络行为(默认跳过,`ZHUYE_FUNCTIONAL_NETWORK=1` 才跑):真实 GitHub
  Search API 返回清单,每条含 `owner/repo` 与接入命令提示

发现只读不写:输出即清单 + `marketplace add` 接入命令提示,不落任何
marketplace 注册状态(供应链安全:发现 ≠ 自动接入)。
"""

from __future__ import annotations

import os
import subprocess

import pytest

from wire import ZHUYE_BIN, Sandbox, require_binary, run_cli

pytestmark = pytest.mark.usefixtures("sandbox")


def test_discover参数面(sandbox: Sandbox) -> None:
    """`--help` 字段级:--topic 默认 zhuye-plugin、--limit 默认 20。"""
    require_binary()
    proc = subprocess.run(
        [str(ZHUYE_BIN), "plugin", "marketplace", "discover", "--help"],
        cwd=str(sandbox.cwd),
        env=sandbox.env(),
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert proc.returncode == 0, f"discover --help 失败:{proc.stderr}"
    assert "--topic" in proc.stdout + proc.stderr
    assert "zhuye-plugin" in proc.stdout + proc.stderr, "默认 topic 应为 zhuye-plugin"
    assert "--limit" in proc.stdout + proc.stderr


@pytest.mark.skipif(
    os.environ.get("ZHUYE_FUNCTIONAL_NETWORK") != "1",
    reason="真实 GitHub API 需网络;离线套件默认跳过(ZHUYE_FUNCTIONAL_NETWORK=1 开启)",
)
def test_discover真实网络(sandbox: Sandbox) -> None:
    """真实查询(用必然有结果的 topic `mcp-server` 验证解析链路)→
    输出 repo 清单与接入命令提示。默认 topic `zhuye-plugin` 在 GitHub
    生态尚无仓库,空结果是合法输出(`no repositories found`)。"""
    require_binary()
    proc = run_cli(
        sandbox,
        "plugin",
        "marketplace",
        "discover",
        "--topic",
        "mcp-server",
        "--limit",
        "5",
        timeout=60,
    )
    assert proc.returncode == 0, proc.stderr
    out = proc.stdout
    assert "stars" in out, f"表头应含 stars 列:{out[:400]}"
    assert "marketplace add" in out, "应输出接入命令提示"
