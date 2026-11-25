#!/usr/bin/env bash
# 本地工作流演练:默认跳过;显式开启真模型或离线自检。
set -eu
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
if [ "${ZHUYE_LIVE:-0}" != 1 ] && [ "${ZHUYE_E2E_MOCK:-0}" != 1 ]; then
    echo "[SKIP] 设置 ZHUYE_LIVE=1 或 ZHUYE_E2E_MOCK=1 后运行工作流演练"
    exit 0
fi
BIN="${ZHUYE_WORKFLOW_BIN:-$ROOT/target/debug/zhuye-workflow-server}"
if [ ! -x "$BIN" ]; then
    (cd "$ROOT" && cargo build -p zhuye-workflow-server)
fi
export ZHUYE_WORKFLOW_BIN="$BIN"
exec python3 "$HERE/workflow_e2e.py"
