#!/usr/bin/env python3
"""在临时环境中安装指定发布产物,验证二进制和三个语言绑定。"""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def one(directory: Path, pattern: str) -> Path:
    """缺少或存在多个候选均视为基础设施错误。"""
    matches = sorted(directory.glob(pattern))
    if len(matches) != 1 or not matches[0].is_file():
        raise RuntimeError(f"产物 {pattern} 必须恰有一个,实际:{matches}")
    return matches[0].resolve()


def run(command: list[str], *, env: dict[str, str], cwd: Path) -> None:
    print("执行:", " ".join(command), flush=True)
    subprocess.run(command, env=env, cwd=cwd, check=True, timeout=600)


def main() -> int:
    artifact = os.environ.get("RUNNER_ARTIFACT")
    if not artifact:
        raise RuntimeError("需要 RUNNER_ARTIFACT 产物目录")
    source = Path(artifact).resolve()
    binary = one(source, "zhuye.exe" if os.name == "nt" else "zhuye")
    python_sdk = one(source, "zhuye-*.whl")
    native = one(source, "zhuye_py-*.whl")
    npm = one(source, "*.tgz")
    if not os.access(binary, os.X_OK):
        raise RuntimeError(f"二进制不可执行:{binary}")

    with tempfile.TemporaryDirectory(prefix="zhuye-artifacts-") as tmp:
        work = Path(tmp)
        env = {k: v for k, v in os.environ.items() if not k.startswith(("ZHUYE_", "PYTHONPATH", "PYTHONHOME"))}
        env.update(ZHUYE_BIN=str(binary), HOME=str(work), ZHUYE_HOME=str(work / ".zhuye"))
        (work / ".zhuye").mkdir()
        (work / ".zhuye/config.toml").write_text("[hooks]\nenabled = []\n")
        version = subprocess.check_output([str(binary), "--version"], env=env, cwd=work, text=True, timeout=30).strip().split()[-1]
        print(f"二进制版本:{version}", flush=True)

        sdk_venv = work / "sdk-venv"
        native_venv = work / "native-venv"
        for venv, wheel in ((sdk_venv, python_sdk), (native_venv, native)):
            run([sys.executable, "-m", "venv", "--system-site-packages", str(venv)], env=env, cwd=work)
            python = venv / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
            run([str(python), "-m", "pip", "install", "--ignore-installed", "--no-index", "--no-deps", str(wheel)], env=env, cwd=work)
        python = sdk_venv / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
        version_check = "import importlib.metadata as m,sys; assert m.version(sys.argv[1]) == sys.argv[2]"
        run([str(python), "-c", version_check, "zhuye", version], env=env, cwd=work)
        run([str(python), "-c", "import pathlib,sys,zhuye; assert pathlib.Path(zhuye.__file__).resolve().is_relative_to(pathlib.Path(sys.prefix).resolve())"], env=env, cwd=work)
        run([str(python), "-m", "pytest", str(ROOT / "tests/sdk-python"), "-q", "-p", "no:cacheprovider"], env=env, cwd=work)
        native_python = native_venv / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
        run([str(native_python), "-c", version_check, "zhuye-py", version], env=env, cwd=work)
        run([str(native_python), str(ROOT / "tests/native-python/test_native.py")], env=env, cwd=work)

        node_dir = work / "node"
        node_dir.mkdir()
        run(["npm", "install", "--offline", "--ignore-scripts", "--no-audit", "--no-fund", "--prefix", str(node_dir), str(npm)], env=env, cwd=work)
        package = node_dir / "node_modules/zhuye"
        if json.loads((package / "package.json").read_text())["version"] != version:
            raise RuntimeError("npm 包与二进制版本不一致")
        env["ZHUYE_SDK_ENTRY"] = (package / "dist/index.js").as_uri()
        run(["node", "--test", str(ROOT / "tests/sdk-ts/real-bin.test.mjs")], env=env, cwd=work)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as exc:
        print(f"产物验证失败:{exc}", file=sys.stderr)
        sys.exit(2)
