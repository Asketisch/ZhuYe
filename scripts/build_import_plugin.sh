#!/usr/bin/env bash
# 构建 harness-import 插件的二进制并装配进插件目录。
#
# 用法:./scripts/build_import_plugin.sh [--fast]
#   --fast  用 release-fast profile(本地快速迭代;分发用默认 release)
#
# 步骤:cargo build -p zhuye-import → 拷贝 target/<profile>/zhuye-import
# 到 plugins/harness-import/bin/(.mcp.json 以 ${PLUGIN_ROOT}/bin/zhuye-import
# 引用)。bin/ 内容是构建产物,不入库(.gitignore 覆盖)。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PLUGIN_BIN_DIR="$ROOT/plugins/harness-import/bin"
PROFILE="release"
PROFILE_FLAG=""
if [[ "${1:-}" == "--fast" ]]; then
    PROFILE="release-fast"
    PROFILE_FLAG="--profile release-fast"
fi

cd "$ROOT"
echo ">> cargo build -p zhuye-import ($PROFILE)"
cargo build $PROFILE_FLAG -p zhuye-import

mkdir -p "$PLUGIN_BIN_DIR"
cp "target/$PROFILE/zhuye-import" "$PLUGIN_BIN_DIR/zhuye-import"
echo ">> 已装配 $PLUGIN_BIN_DIR/zhuye-import"
echo ">> 安装:zhuye plugin install plugins/harness-import --scope user && zhuye plugin enable harness-import@inline"
