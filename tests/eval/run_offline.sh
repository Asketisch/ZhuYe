#!/usr/bin/env bash
# 评测流水线的**离线自检**:用 mock provider 跑通全部固定用例,验证
# 沙箱 / 验收器 / 评分器 / 质量门禁整条链路可用(不需要模型凭据、不产生费用)。
#
# 注意:离线自检里的成功率衡量的是**流水线**,不是模型能力 —— 真正的质量
# 基线必须用 tests/eval/run_quality.sh(真模型)产出。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# mktemp 模板必须自带尾部 X(GNU/BSD 通用);BSD 专有的 `-t 前缀` 语义在
# Linux 上会报 "too few X's" 导致 CI 挂。
OUT="${ZHUYE_EVAL_OFFLINE_OUTPUT:-$(mktemp -d "${TMPDIR:-/tmp}/zhuye-eval-offline.XXXXXX")}"
SCORER="$ROOT/target/debug/zhuye-eval-score"
BIN="${ZHUYE_BIN:-$ROOT/target/release/zhuye}"

cargo build --manifest-path "$ROOT/Cargo.toml" -p zhuye-eval --bin zhuye-eval-score --quiet
if [ ! -x "$BIN" ]; then
    cargo build --manifest-path "$ROOT/Cargo.toml" --release -p zhuye-cli --quiet
fi

python3 "$ROOT/tests/eval/run.py" \
    --candidate "$BIN" --baseline "$BIN" \
    --model mock --base-url http://127.0.0.1:0 \
    --scorer "$SCORER" --output "$OUT" \
    --repetitions 1 --offline --mode observe

python3 - "$OUT/report.json" <<'PY'
import json, sys
report = json.load(open(sys.argv[1], encoding="utf-8"))
if not report.get("offline_self_check"):
    raise SystemExit("报告缺少 offline_self_check 标记")
for role in ("baseline", "candidate"):
    cases = report["versions"][role]["cases"]
    total = sum(g["pass"] + g["fail"] + g["infra_error"] for g in cases.values())
    passed = sum(g["pass"] for g in cases.values())
    failed = [name for name, g in cases.items() if g["pass"] != 1]
    print(f"{role}: {passed}/{total} 通过,平均耗时 "
          f"{sum(g['mean_elapsed_s'] for g in cases.values()) / len(cases):.2f}s")
    if failed:
        raise SystemExit(f"{role} 离线自检未全绿:{failed}")
print("离线自检通过:评测流水线(沙箱 / 验收 / 评分 / 门禁)可用")
PY
