#!/usr/bin/env bash
# 构建 Computer Use 插件二进制(默认开 real feature:xcap/enigo)。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PROFILE="release"
if [[ "${1:-}" == "--fast" ]]; then
  PROFILE="release-fast"
fi

cargo build --profile "$PROFILE" -p zhuye-computer-use --features real --bin zhuye-computer-use-server
mkdir -p plugins/computer-use/bin
cp "target/$PROFILE/zhuye-computer-use-server" plugins/computer-use/bin/zhuye-computer-use-server

echo "构建完成:plugins/computer-use/bin/zhuye-computer-use-server(real feature)"
echo "安装:zhuye plugin install plugins/computer-use --scope user && zhuye plugin enable computer-use@inline"
