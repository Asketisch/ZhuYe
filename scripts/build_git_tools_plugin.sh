#!/usr/bin/env bash
# 构建 Git Tools 插件二进制到 plugins/git-tools/bin/。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PROFILE="release"
if [[ "${1:-}" == "--fast" ]]; then
  PROFILE="release-fast"
fi

cargo build --profile "$PROFILE" -p zhuye-git-tools --bins
mkdir -p plugins/git-tools/bin
cp "target/$PROFILE/zhuye-git-tools-server" plugins/git-tools/bin/
cp "target/$PROFILE/zhuye-cp-hook" plugins/git-tools/bin/

echo "构建完成:plugins/git-tools/bin/{zhuye-git-tools-server,zhuye-cp-hook}"
echo "安装:zhuye plugin install plugins/git-tools --scope user && zhuye plugin enable git-tools@inline"
