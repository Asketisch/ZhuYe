#!/usr/bin/env bash
# tests/rust/workspace_tests.sh —— workspace 级测试:单元 + 集成 + lint + 文档。
#
# 覆盖:
#   1. cargo test --workspace --all-targets 与 --doc 工作区测试和文档用例
#   2. cargo clippy -D warnings CI 门槛等价物
#   3. cargo fmt --check        格式检查
#   4. cargo doc --no-deps      文档构建
#
# 用法:由 tests/rust/run.sh source;也可独立执行。

source "$(dirname "${BASH_SOURCE[0]}")/../lib.sh"

cd "$ROOT"

# ── 1. 全 workspace 测试(核心覆盖面)────────────────────────────────────
echo "── workspace 测试(cargo test --workspace --all-targets)──"
cargo_test_workspace() {
    cargo test --workspace --all-targets --quiet
}
tcase "cargo test --workspace --all-targets 全绿" cargo_test_workspace

echo "── workspace 文档测试 ──"
doc_test_workspace() {
    cargo test --workspace --doc --quiet
}
tcase "cargo test --workspace --doc 全绿" doc_test_workspace

# ── 2. clippy 严格检查 ────────────────────────────────────────────────────
echo "── clippy(-D warnings)──"
clippy_strict() {
    cargo clippy --workspace --all-targets -- -D warnings
}
tcase "clippy --workspace --all-targets -D warnings" clippy_strict

# ── 3. 格式检查 ────────────────────────────────────────────────────────────
echo "── fmt 检查 ──"
fmt_check() {
    cargo fmt --all -- --check
}
tcase "cargo fmt --all --check" fmt_check

# ── 4. 文档构建 ────────────────────────────────────────────────────────────
echo "── 文档构建 ──"
doc_build() {
    cargo doc --no-deps --quiet
}
tcase "cargo doc --no-deps" doc_build

tfinish "workspace_tests.sh" || exit 1
exit 0
