#!/usr/bin/env bash
# 构建 Shell 插件二进制到 plugins/shell/bin/(装插件前必须先跑本脚本)。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PROFILE="release"
if [[ "${1:-}" == "--fast" ]]; then
  PROFILE="release-fast"
fi

cargo build --profile "$PROFILE" -p zhuye-shell --bin zhuye-shell-server
mkdir -p plugins/shell/bin
cp "target/$PROFILE/zhuye-shell-server" plugins/shell/bin/zhuye-shell-server

echo "构建完成:plugins/shell/bin/zhuye-shell-server"
echo "安装:zhuye plugin install plugins/shell --scope user && zhuye plugin enable shell@inline"
