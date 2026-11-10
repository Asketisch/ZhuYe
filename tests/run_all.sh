#!/usr/bin/env bash
# tests/run_all.sh —— 全量测试入口:能力覆盖层 + Rust 侧 + SDK 和原生 Python 绑定。
#
# 用法:
#   ./tests/run_all.sh            # 全部
#   ./tests/run_all.sh functional # 功能覆盖(能力层 + Rust 参数面 + 双 SDK + 原生 Python)
#   ./tests/run_all.sh functional-layer  # 只跑能力覆盖层(Python 打真二进制)
#   ./tests/run_all.sh live       # 实模型演练层(真实模型;默认 SKIP,需 ZHUYE_LIVE=1)
#   ./tests/run_all.sh rust       # 只跑 Rust 侧
#   ./tests/run_all.sh sdk-python # 只跑 Python SDK 绑定层
#   ./tests/run_all.sh sdk-ts     # 只跑 TS SDK 绑定层
#   ./tests/run_all.sh native-python # 构建 wheel 并验证原生 Python 绑定
#
# `functional` 是 CI 的门禁入口:跳过 cargo test / clippy / fmt / doc
# (那部分由既有 CI job 覆盖),跑真正的功能覆盖。
#
# `live` 是**实模型演练层**:用真实模型跑同一条引擎链路,断言协议形状。
# 默认 SKIP(不发起网络请求)—— 它需要外网与免费额度,不适合当 CI 门禁。

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CUSTOM_ZHUYE_BIN="${ZHUYE_BIN:-}"
source "$HERE/lib.sh"

SUITES=("functional-layer" "rust" "sdk-python" "sdk-ts" "native-python")
if [ $# -gt 0 ]; then
    SUITES=("$@")
fi

cd "$ROOT"

# 先校验套件名,避免拼错参数时仍触发构建。
for s in "${SUITES[@]}"; do
    case "$s" in
        functional|functional-layer|live|rust|sdk-python|sdk-ts|native-python) ;;
        *) echo "未知套件:$s(可选:functional / functional-layer / live / rust / sdk-python / sdk-ts / native-python)" >&2; exit 2 ;;
    esac
done

# 默认产物交给 Cargo 判断是否过期,不能仅凭文件存在就测试旧代码。
# 显式指定 ZHUYE_BIN 时尊重调用者的二进制,不覆盖其产物。
NEED_ZHUYE_BIN=0
for s in "${SUITES[@]}"; do
    if [ "$s" != "native-python" ]; then
        NEED_ZHUYE_BIN=1
        break
    fi
done
if [ -z "$CUSTOM_ZHUYE_BIN" ] && [ "$NEED_ZHUYE_BIN" -eq 1 ]; then
    echo "── 构建当前源码的 release 二进制 ──"
    cargo build --release -p zhuye-cli -p zhuye-import --quiet || exit 2
    for s in "${SUITES[@]}"; do
        if [ "$s" = "functional" ] || [ "$s" = "functional-layer" ]; then
            cargo build --release -p zhuye-kms -p zhuye-computer-use -p zhuye-shell -p zhuye-git-tools --bins --quiet || exit 2
            cargo build -p zhuye-mcp --quiet || exit 2
            break
        fi
    done
fi
export ZHUYE_BIN

OVERALL=0
for s in "${SUITES[@]}"; do
    case "$s" in
        functional)  RUNNERS=("$HERE/functional/run.sh" "$HERE/rust/run.sh" "$HERE/sdk-python/run.sh" "$HERE/sdk-ts/run.sh" "$HERE/native-python/run.sh") ;;
        functional-layer) RUNNERS=("$HERE/functional/run.sh") ;;
        live)        RUNNERS=("$HERE/live/run.sh") ;;
        rust)        RUNNERS=("$HERE/rust/run.sh") ;;
        sdk-python)  RUNNERS=("$HERE/sdk-python/run.sh") ;;
        sdk-ts)      RUNNERS=("$HERE/sdk-ts/run.sh") ;;
        native-python) RUNNERS=("$HERE/native-python/run.sh") ;;
    esac
    echo
    echo "╔══════════════════════════════════════════════════╗"
    echo "  套件:$s"
    echo "╚══════════════════════════════════════════════════╝"
    for runner in "${RUNNERS[@]}"; do
        # 仅 functional 中的 Rust 参数面跳过 workspace;不污染后续套件。
        if [ "$s" = "functional" ] && [ "$runner" = "$HERE/rust/run.sh" ]; then
            ZHUYE_SKIP_WORKSPACE=1 bash "$runner"
            runner_rc=$?
        else
            bash "$runner"
            runner_rc=$?
        fi
        if [ "$runner_rc" -eq 0 ]; then
            echo "  ✅ $(basename "$(dirname "$runner")") 通过"
        else
            echo "  ❌ $(basename "$(dirname "$runner")") 失败"
            OVERALL=1
        fi
    done
done

echo
if [ "$OVERALL" -eq 0 ]; then
    echo "══════════════ 全部测试套件通过 ══════════════"
else
    echo "══════════════ 存在失败套件(见上)══════════════"
fi
exit "$OVERALL"
