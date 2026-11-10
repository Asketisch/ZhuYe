#!/usr/bin/env bash
# tests/functional/run.sh —— 能力覆盖层入口(Python 驱动真二进制,原始 wire)。
#
# 这一层测的是 **Rust 代码在真产物上的行为**:起 `zhuye serve` 子进程、
# 走原始 JSONL wire、断言事件字段 —— **不经过任何 SDK**(SDK 的 bug 会
# 把 Rust 的 bug 遮住)。
#
# 覆盖:基础通信 / 并发路由 / 工具调用 / 审批交互 / 会话状态 op /
#      重连恢复 / 容错 / 持久化 / 编排 / CLI 能力面。
#
# 全程离线(ZHUYE_MODEL=mock)+ 隔离 HOME(mktemp),不污染真实 ~/.zhuye。
# 退出码 0 = 全部通过;缺少 pytest 时失败,不能以跳过冒充覆盖。

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/../lib.sh"

require_binary
cd "$ROOT"

RC=0

echo "── 能力覆盖层:Python 驱动真二进制(原始 wire)──"
if require_cmd python3 && python3 -m pytest --version >/dev/null 2>&1; then
    # 这些前置条件缺失时,用例自身允许按需跳过;全量门禁必须明确失败。
    BIN_DIR="$(dirname "$ZHUYE_BIN")"
    for bin in zhuye-import zhuye-kms-server zhuye-computer-use-server zhuye-shell-server zhuye-git-tools-server zhuye-cp-hook; do
        if [ ! -x "$BIN_DIR/$bin" ]; then
            echo "  [FAIL] 缺少能力测试二进制:$BIN_DIR/$bin" >&2
            exit 1
        fi
    done
    if [ ! -x "$ROOT/target/debug/mock_mcp_server" ]; then
        echo "  [FAIL] 缺少可执行的 mock MCP server:$ROOT/target/debug/mock_mcp_server" >&2
        exit 1
    fi
    for path in "$ROOT/plugins/plugin-demo/bin/zhuye-demo-tool.wasm" \
        "$ROOT/plugins/plugin-demo/bin/zhuye-demo-hook.wasm" \
        "$ROOT/plugins/harness-import" "$ROOT/plugins/kms" \
        "$ROOT/plugins/computer-use" "$ROOT/plugins/shell" "$ROOT/plugins/git-tools"; do
        if [ ! -e "$path" ]; then
            echo "  [FAIL] 缺少能力测试资源:$path" >&2
            exit 1
        fi
    done
    if ! require_cmd git; then
        echo "  [FAIL] 能力测试需要 git" >&2
        exit 1
    fi
    if HOME="$(mktemp -d)" ZHUYE_BIN="$ZHUYE_BIN" \
        python3 -m pytest "$HERE" -v --tb=short; then
        echo "  [ok]   能力覆盖层全绿"
    else
        echo "  [FAIL] 能力覆盖层存在失败"
        RC=1
    fi
else
    echo "  [FAIL] 未安装 python3/pytest" >&2
    RC=1
fi

exit "$RC"
