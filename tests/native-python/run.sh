#!/usr/bin/env bash
# 构建并安装真实 wheel,在隔离 Python 中验证 PyO3 入口。
set -eu

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
python3 -m maturin --version >/dev/null || {
    echo "缺少 maturin:请安装 python3 -m pip install maturin" >&2
    exit 2
}
TEMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TEMP_DIR"' EXIT
python3 -m venv "$TEMP_DIR/venv"
python3 -m maturin build --manifest-path "$ROOT/crates/runtime/zhuye-py/Cargo.toml" \
    --interpreter "$TEMP_DIR/venv/bin/python" --out "$TEMP_DIR/wheels"
"$TEMP_DIR/venv/bin/python" -m pip install --no-index --no-deps "$TEMP_DIR"/wheels/*.whl
unset PYTHONPATH
python3 - "$TEMP_DIR/venv/bin/python" "$HERE/test_native.py" <<'PY'
import subprocess
import sys

try:
    result = subprocess.run(sys.argv[1:], timeout=90, check=False)
except subprocess.TimeoutExpired:
    print("原生 Python 测试超过 90 秒", file=sys.stderr)
    raise SystemExit(1)
raise SystemExit(result.returncode)
PY
