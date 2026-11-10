#!/usr/bin/env bash
# tests/sdk-ts/run.sh —— TypeScript SDK 绑定层测试入口。
#
# 两批用例:
#   1. tests/sdk-ts/real-bin.test.mjs     —— 真二进制 + mock provider,
#      断言 **TS SDK 客户端自身的行为**(握手/单轮 prompt/远程工具本地执行/
#      非 turn op 收尾/close 幂等);
#   2. sdks/typescript/tests/*.test.ts    —— mock serve 协议单测
#      (vitest,仓库原有,一并纳入统一入口)。
#
# 服务端语义(19 Op / 事件流 / 重连 / 容错)由能力覆盖层承担:
#   tests/functional/(原始 wire,不经 SDK)。
#
# 退出码 0 = 全部通过;缺少 node / vitest 依赖时失败。

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/../lib.sh"

require_binary
cd "$ROOT"

RC=0

echo "── TS SDK 绑定层:真二进制端到端(node --test)──"
if require_cmd node; then
    # 测试导入 dist;每次构建以覆盖源码、编译配置与依赖变化。
    (cd "$ROOT/sdks/typescript" && npm run build --silent) || {
        echo "  [FAIL] dist 构建失败"
        exit 1
    }
    if HOME="$(mktemp -d)" ZHUYE_BIN="$ZHUYE_BIN" \
        node --test "$HERE/real-bin.test.mjs"; then
        echo "  [ok]   SDK 绑定层全绿"
    else
        echo "  [FAIL] SDK 绑定层存在失败"
        RC=1
    fi
else
    echo "  [FAIL] 未安装 node" >&2
    RC=1
fi

echo
echo "── TS SDK:协议单测(vitest,mock serve)──"
if [ -d "$ROOT/sdks/typescript/node_modules/vitest" ]; then
    if ! (cd "$ROOT/sdks/typescript" && npm run typecheck); then
        echo "  [FAIL] SDK 类型契约检查失败"
        RC=1
    fi
    if (cd "$ROOT/sdks/typescript" && npx vitest run); then
        echo "  [ok]   协议单测全绿"
    else
        echo "  [FAIL] 协议单测存在失败"
        RC=1
    fi
else
    echo "  [FAIL] sdks/typescript 未安装 vitest 依赖" >&2
    RC=1
fi

exit "$RC"
