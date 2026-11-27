#!/usr/bin/env bash
# 明确启用时构建评分器并执行真实模型质量比较。
# 门禁模式:ZHUYE_EVAL_MODE=observe(默认,只记录 would_block)/ enforce(退化返回 1)。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
: "${ZHUYE_EVAL_BASELINE:?需要指定基线二进制}"
: "${ZHUYE_EVAL_CANDIDATE:?需要指定候选二进制}"
: "${ZHUYE_EVAL_MODEL:?需要指定固定模型}"
: "${ZHUYE_EVAL_BASE_URL:?需要指定 provider 地址}"
: "${ZHUYE_EVAL_API_KEY:?需要设置真实模型凭据}"
: "${ZHUYE_EVAL_MODE:=observe}"
: "${ZHUYE_EVAL_POLICY:=$ROOT/tests/eval/policy.json}"
cargo build --manifest-path "$ROOT/Cargo.toml" -p zhuye-eval --bin zhuye-eval-score
EVAL_ARGS=(--policy "$ZHUYE_EVAL_POLICY")
if [[ -n "${ZHUYE_EVAL_INPUT_PRICE:-}" ]]; then
    EVAL_ARGS+=(--input-price "$ZHUYE_EVAL_INPUT_PRICE")
fi
if [[ -n "${ZHUYE_EVAL_OUTPUT_PRICE:-}" ]]; then
    EVAL_ARGS+=(--output-price "$ZHUYE_EVAL_OUTPUT_PRICE")
fi
exec python3 "$ROOT/tests/eval/run.py" \
    --candidate "$ZHUYE_EVAL_CANDIDATE" --baseline "$ZHUYE_EVAL_BASELINE" \
    --candidate-profile "${ZHUYE_EVAL_CANDIDATE_PROFILE:-default}" \
    --baseline-profile "${ZHUYE_EVAL_BASELINE_PROFILE:-default}" \
    --model "$ZHUYE_EVAL_MODEL" --base-url "$ZHUYE_EVAL_BASE_URL" \
    --scorer "$ROOT/target/debug/zhuye-eval-score" \
    --output "${ZHUYE_EVAL_OUTPUT:-$ROOT/target/eval-quality}" \
    --repetitions "${ZHUYE_EVAL_REPETITIONS:-3}" \
    --mode "$ZHUYE_EVAL_MODE" \
    "${EVAL_ARGS[@]}"
