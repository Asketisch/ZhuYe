#!/usr/bin/env bash
# 构建 Browser 插件二进制(默认开 real feature:CDP over loopback)。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PROFILE="release"
if [[ "${1:-}" == "--fast" ]]; then
  PROFILE="release-fast"
fi

cargo build --profile "$PROFILE" -p zhuye-browser --features real --bin zhuye-browser-server
mkdir -p plugins/browser/bin
cp "target/$PROFILE/zhuye-browser-server" plugins/browser/bin/zhuye-browser-server

echo "构建完成:plugins/browser/bin/zhuye-browser-server(real feature)"
echo "安装:zhuye plugin install plugins/browser --scope user && zhuye plugin enable browser@inline"
