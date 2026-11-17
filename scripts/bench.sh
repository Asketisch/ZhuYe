#!/usr/bin/env bash
# 性能基线:各指标独立采样,失败或缺少事件时返回非零。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
exec python3 "$ROOT/scripts/bench.py" "$@"
