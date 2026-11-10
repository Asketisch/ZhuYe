#!/usr/bin/env bash
# tests/rust/run.sh —— Rust 侧测试总入口。
#
# 依次执行:
#   1. workspace_tests.sh   cargo test / clippy / fmt / doc
#   2. cli_core.sh          核心 CLI 端到端(exec/serve/session/traces/login/config)
#   3. cli_modules.sh       模块 CLI 端到端(mcp/lsp/plugin/task/pipeline/discussion/...)
#   4. examples_e2e.sh      zhuye 库 examples 冒烟
#   5. env_lock_stress.sh   zhuye-config env 竞态回归压测(44ae3da 门禁)
#  (tools_drive.sh 已迁入 functional/test_tools.py T30–T38,见 §9 迁移路线)
#
# `ZHUYE_SKIP_WORKSPACE=1` 时跳过第 1 步(cargo test / clippy / fmt / doc)
# —— CI 的 `functional` 门禁入口用此模式:那部分由既有 CI job 覆盖。
#
# 全程离线(ZHUYE_MODEL=mock)+ 隔离 HOME,不污染真实 ~/.zhuye。
# 退出码 0 = 全部通过。

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/../lib.sh"

cd "$ROOT"

# release 二进制不存在则构建(幂等)
if [ ! -x "$ZHUYE_BIN" ]; then
    echo "── 构建 release 二进制 ──"
    cargo build --release -p zhuye-cli --quiet || exit 2
fi

TOTAL_RC=0

if [ "${ZHUYE_SKIP_WORKSPACE:-0}" = "1" ]; then
    echo
    echo "════════════════════════════════════════════════"
    echo "  1/5 workspace 测试 —— 跳过(ZHUYE_SKIP_WORKSPACE=1)"
    echo "════════════════════════════════════════════════"
else
    echo
    echo "════════════════════════════════════════════════"
    echo "  1/5 workspace 测试(cargo test + lint + doc)"
    echo "════════════════════════════════════════════════"
    bash "$HERE/workspace_tests.sh" || TOTAL_RC=1
fi

echo
echo "════════════════════════════════════════════════"
echo "  2/5 核心 CLI 端到端"
echo "════════════════════════════════════════════════"
bash "$HERE/cli_core.sh" || TOTAL_RC=1

echo
echo "════════════════════════════════════════════════"
echo "  3/5 模块 CLI 端到端"
echo "════════════════════════════════════════════════"
bash "$HERE/cli_modules.sh" || TOTAL_RC=1

echo
echo "════════════════════════════════════════════════"
echo "  4/5 examples 冒烟"
echo "════════════════════════════════════════════════"
bash "$HERE/examples_e2e.sh" || TOTAL_RC=1

echo
echo "════════════════════════════════════════════════"
echo "  5/5 zhuye-config env 竞态回归压测"
echo "════════════════════════════════════════════════"
bash "$HERE/env_lock_stress.sh" || TOTAL_RC=1

echo
if [ "$TOTAL_RC" -eq 0 ]; then
    echo "  ✅ Rust 测试套件全部通过"
else
    echo "  ❌ Rust 测试套件存在失败(见上方 [FAIL] 行)"
fi
exit "$TOTAL_RC"
