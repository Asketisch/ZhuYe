"""能力覆盖层的 pytest fixture(唯一职责:提供隔离环境与被测对象)。

共享设施在 `wire.py`(唯一命名模块)—— 不用 `from conftest import ...` 的写法:
两个测试层各有一个 conftest.py,同名导入在一次性收集整个 tests/ 时会按
sys.path 顺序解析到错误的那一份。

用例按被测能力分入六个子目录(protocol / tools / orchestration /
persistence / cli / runtime)。子目录里的用例 `from wire import ...` 靠
下面这行显式钉住的 sys.path 解析 —— pytest 默认的 prepend 模式只把
「用例文件所在目录」插进 sys.path,子目录里的用例就找不到上一层的
wire.py 了。
"""

from __future__ import annotations

from pathlib import Path
from typing import Callable, Iterator
import json
import hashlib
import html
import os
import re
import shlex
import subprocess
import sys

import pytest

# 把本目录(共享设施 wire.py 所在处)钉进 sys.path,让各能力子目录里的
# 用例都能 `from wire import ...`。
sys.path.insert(0, str(Path(__file__).resolve().parent))

from wire import DRAIN_TIMEOUT, ZHUYE_BIN, Sandbox, ServeSession  # noqa: E402


@pytest.hookimpl(hookwrapper=True)
def pytest_runtest_makereport(item, call):
    """保存测试结果,供沙箱清理时判断是否收集证据。"""
    outcome = yield
    report = outcome.get_result()
    setattr(item, f"rep_{report.when}", report)


def _redact(text: str) -> str:
    """遮盖环境中已知凭据和常见配置字段。"""
    secrets = [v for k, v in os.environ.items()
               if len(v) >= 6 and any(part in k.upper() for part in ("TOKEN", "SECRET", "PASSWORD", "API_KEY"))]
    for value in secrets:
        for encoded in (value, html.escape(value, quote=True), json.dumps(value, ensure_ascii=False)[1:-1]):
            text = text.replace(encoded, "[已隐藏]")
    return re.sub(r'(?i)((?:api[_-]?key|authorization|password|secret|token)\\?["\']?\s*[:=]\s*\\?["\'])(.*?)(?=\\?["\']|\n)',
                  r'\1[已隐藏]', text)


@pytest.fixture()
def sandbox(tmp_path: Path, request) -> Iterator[Sandbox]:
    """每个用例一份隔离 HOME + cwd。

    HOME 里预写 `~/.zhuye/config.toml`(`[hooks] enabled = []`):serve/CLI
    默认启用 VerificationHook(Stop 时跑 `cargo test`)等重 hook,单轮会拖到
    分钟级;能力用例只验行为语义,不验 hook 计时。config.toml 必须存在 ——
    否则 ConfigWatcher 启动即报错退出。
    """
    home = tmp_path / "home"
    cwd = tmp_path / "cwd"
    (home / ".zhuye").mkdir(parents=True)
    cwd.mkdir(parents=True)
    (home / ".zhuye" / "config.toml").write_text(
        "# 测试用最小配置:关闭全部内置 hook\n[hooks]\nenabled = []\n"
    )
    yield Sandbox(home=home, cwd=cwd)
    report = getattr(request.node, "rep_call", None)
    destination = os.environ.get("ZHUYE_TEST_ARTIFACTS")
    if destination and report is not None and report.failed:
        name = re.sub(r"[^\w.-]+", "_", request.node.nodeid)[:160]
        target = Path(destination) / name
        target.mkdir(parents=True, exist_ok=True)
        for path in tmp_path.rglob("*"):
            if not path.is_file() or path.is_symlink():
                continue
            # 诊断包仅收集文本及 rollout,跳过任意二进制或大文件。
            if path.stat().st_size > 5_000_000 or path.suffix not in {".jsonl", ".stderr", ".toml"}:
                continue
            try:
                content = path.read_text(encoding="utf-8")
            except (UnicodeError, OSError):
                continue
            output = target / path.relative_to(tmp_path)
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_text(_redact(content), encoding="utf-8")
        identity = {"test": request.node.nodeid, "binary": str(ZHUYE_BIN),
                    "seed": os.environ.get("RUNNER_SEED"),
                    "reproduce": f"ZHUYE_BIN={shlex.quote(str(ZHUYE_BIN))} python3 -m pytest {shlex.quote(request.node.nodeid)} -v",
                    "revision": subprocess.run(["git", "rev-parse", "HEAD"], cwd=Path(__file__).resolve().parents[2],
                                               capture_output=True, text=True, check=False).stdout.strip()}
        try:
            identity["binary_sha256"] = hashlib.sha256(ZHUYE_BIN.read_bytes()).hexdigest()
            version = subprocess.run([str(ZHUYE_BIN), "--version"], capture_output=True, text=True, timeout=10, check=False)
            identity["binary_version"] = _redact(version.stdout.strip()) if version.returncode == 0 else None
        except (OSError, subprocess.SubprocessError):
            identity["binary_sha256"] = None
            identity["binary_version"] = None
        (target / "identity.json").write_text(json.dumps(identity, ensure_ascii=False, indent=2), encoding="utf-8")


@pytest.fixture()
def mock_script(tmp_path: Path) -> Callable[[list[str]], Path]:
    """写 mock provider 脚本:JSONL,每行一次模型回复。

    行格式(见 crates/abilities/zhuye-llm/src/providers/mock.rs):
    `{"type":"text","text":"..."}` / `{"type":"tool_call","name":"t","args":{}}`
    """

    def _write(lines: list[str]) -> Path:
        path = tmp_path / "mock-script.jsonl"
        path.write_text("\n".join(lines) + "\n")
        return path

    return _write



@pytest.fixture()
def serve_session(sandbox: Sandbox) -> Callable[..., ServeSession]:
    """起 `zhuye serve` 会话的工厂(自动收尾)。"""

    created: list[ServeSession] = []

    def _make(
        args: list[str] | None = None,
        env: dict[str, str] | None = None,
        timeout: float = DRAIN_TIMEOUT,
    ) -> ServeSession:
        s = ServeSession(sandbox, args=args, env=env, timeout=timeout)
        created.append(s)
        return s

    yield _make
    for s in created:
        try:
            s.close(timeout=3.0)
        except Exception:
            pass


