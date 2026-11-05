#!/usr/bin/env bash
# 构建 KMS 插件二进制到 plugins/kms/bin/(装插件前必须先跑本脚本)。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PROFILE="release"
if [[ "${1:-}" == "--fast" ]]; then
  PROFILE="release-fast"
fi

cargo build --profile "$PROFILE" -p zhuye-kms --bin zhuye-kms-server
mkdir -p plugins/kms/bin
cp "target/$PROFILE/zhuye-kms-server" plugins/kms/bin/zhuye-kms-server

echo "构建完成:plugins/kms/bin/zhuye-kms-server"
echo "安装:zhuye plugin install plugins/kms --scope user && zhuye plugin enable kms@inline"
