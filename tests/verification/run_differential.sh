#!/usr/bin/env bash
# 明确启用时执行新旧二进制核心语义差分(agent.contract.differential)。
# 需要提供两个被测版本的 serve 二进制路径;结果写入 target/verification。
# 只跑部分用例时直接调用 differential.py --case(支持重复传参)。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
: "${ZHUYE_DIFF_BASELINE:?需要指定基线二进制}"
: "${ZHUYE_DIFF_CANDIDATE:?需要指定候选二进制}"
exec python3 "$ROOT/tests/verification/differential.py" \
    --baseline "$ZHUYE_DIFF_BASELINE" --candidate "$ZHUYE_DIFF_CANDIDATE" \
    --output "${ZHUYE_DIFF_OUTPUT:-$ROOT/target/verification}"
