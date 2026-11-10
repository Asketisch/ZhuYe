"""测试统一入口的套件路由与构建策略,防止门禁因跳过而假绿。"""

from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]


@pytest.fixture
def runner_tree(tmp_path: Path) -> tuple[Path, dict[str, str]]:
    tests = tmp_path / "tests"
    tests.mkdir()
    shutil.copy2(ROOT / "tests/run_all.sh", tests / "run_all.sh")
    shutil.copy2(ROOT / "tests/lib.sh", tests / "lib.sh")
    for suite in ("functional", "rust", "sdk-python", "sdk-ts", "native-python", "live"):
        target = tests / suite / "run.sh"
        target.parent.mkdir()
        target.write_text(
            '#!/usr/bin/env bash\n'
            'printf "%s|%s\\n" "$(basename "$(dirname "$0")")" '
            '"${ZHUYE_SKIP_WORKSPACE:-0}" >> "$RUNNER_TEST_LOG"\n'
        )
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    cargo = bin_dir / "cargo"
    cargo.write_text('#!/usr/bin/env bash\nprintf "%s\\n" "$*" >> "$RUNNER_CARGO_LOG"\n')
    cargo.chmod(0o755)
    env = dict(os.environ)
    env.pop("ZHUYE_BIN", None)
    env.pop("ZHUYE_SKIP_WORKSPACE", None)
    env.update({
        "PATH": str(bin_dir) + os.pathsep + env.get("PATH", ""),
        "RUNNER_TEST_LOG": str(tmp_path / "runners.log"),
        "RUNNER_CARGO_LOG": str(tmp_path / "cargo.log"),
    })
    return tmp_path, env


def _run(root: Path, env: dict[str, str], *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(["bash", str(root / "tests/run_all.sh"), *args], env=env,
                          text=True, capture_output=True, timeout=10)


def test_默认入口各层只运行一次且不跳过工作区(runner_tree) -> None:
    root, env = runner_tree
    result = _run(root, env)
    assert result.returncode == 0, result.stderr + result.stdout
    assert (root / "runners.log").read_text().splitlines() == [
        "functional|0", "rust|0", "sdk-python|0", "sdk-ts|0", "native-python|0",
    ]
    builds = (root / "cargo.log").read_text().splitlines()
    assert any("-p zhuye-cli" in line and "-p zhuye-import" in line for line in builds)
    assert any("-p zhuye-kms" in line and "-p zhuye-shell" in line for line in builds)
    assert any("-p zhuye-mcp" in line for line in builds)


def test_功能门禁跳过标志不泄漏到后续rust套件(runner_tree) -> None:
    root, env = runner_tree
    result = _run(root, env, "functional", "rust")
    assert result.returncode == 0, result.stderr + result.stdout
    assert (root / "runners.log").read_text().splitlines() == [
        "functional|0", "rust|1", "sdk-python|0", "sdk-ts|0", "native-python|0", "rust|0",
    ]


def test_显式二进制不被重建且未知套件不触发构建(runner_tree) -> None:
    root, env = runner_tree
    env["ZHUYE_BIN"] = str(root / "external-zhuye")
    result = _run(root, env, "sdk-python")
    assert result.returncode == 0, result.stderr + result.stdout
    assert not (root / "cargo.log").exists()
    env.pop("ZHUYE_BIN")
    result = _run(root, env, "不存在")
    assert result.returncode == 2
    assert "未知套件" in result.stderr
    assert not (root / "cargo.log").exists()


def test_原生Python单套件不构建CLI二进制(runner_tree) -> None:
    root, env = runner_tree
    result = _run(root, env, "native-python")
    assert result.returncode == 0, result.stderr + result.stdout
    assert (root / "runners.log").read_text().splitlines() == ["native-python|0"]
    assert not (root / "cargo.log").exists()


def test_能力层缺少pytest时失败(runner_tree) -> None:
    root, env = runner_tree
    runner = root / "tests/functional/run.sh"
    shutil.copy2(ROOT / "tests/functional/run.sh", runner)
    binary = root / "target/release/zhuye"
    binary.parent.mkdir(parents=True)
    binary.write_text("#!/usr/bin/env bash\nexit 0\n")
    binary.chmod(0o755)
    python = root / "bin/python3"
    python.write_text("#!/usr/bin/env bash\nexit 1\n")
    python.chmod(0o755)
    result = subprocess.run(["bash", str(runner)], env=env, text=True,
                            capture_output=True, timeout=10)
    assert result.returncode != 0
    assert "未安装 python3/pytest" in result.stderr


def test_能力层缺少插件二进制时失败(runner_tree) -> None:
    root, env = runner_tree
    runner = root / "tests/functional/run.sh"
    shutil.copy2(ROOT / "tests/functional/run.sh", runner)
    binary = root / "target/release/zhuye"
    binary.parent.mkdir(parents=True)
    binary.write_text("#!/usr/bin/env bash\nexit 0\n")
    binary.chmod(0o755)
    result = subprocess.run(["bash", str(runner)], env=env, text=True,
                            capture_output=True, timeout=10)
    assert result.returncode != 0
    assert "缺少能力测试二进制" in result.stderr
    assert "zhuye-import" in result.stderr


def test_examples隔离HOME时沿用已有工具链(runner_tree) -> None:
    root, env = runner_tree
    runner = root / "tests/rust/examples_e2e.sh"
    shutil.copy2(ROOT / "tests/rust/examples_e2e.sh", runner)
    original_home = root / "original-home"
    original_home.mkdir()
    env["HOME"] = str(original_home)
    env.pop("RUSTUP_HOME", None)
    env.pop("CARGO_HOME", None)
    cargo = root / "bin/cargo"
    cargo.write_text('#!/usr/bin/env bash\nprintf "%s\\n%s\\n%s\\n" '
                     '"$HOME" "$RUSTUP_HOME" "$CARGO_HOME" > "$RUNNER_CARGO_LOG"\nexit 1\n')
    result = subprocess.run(["bash", str(runner)], env=env, text=True,
                            capture_output=True, timeout=10)
    assert result.returncode != 0
    build_home, rustup_home, cargo_home = (root / "cargo.log").read_text().splitlines()
    assert build_home != str(original_home)
    assert rustup_home == str(original_home / ".rustup")
    assert cargo_home == str(original_home / ".cargo")
