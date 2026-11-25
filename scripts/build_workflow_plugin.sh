#!/usr/bin/env bash
# 构建工作流 MCP 服务并复制到插件目录。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cargo build --manifest-path "$ROOT/Cargo.toml" --release -p zhuye-workflow-server
mkdir -p "$ROOT/plugins/workflow/bin"
cp "$ROOT/target/release/zhuye-workflow-server" "$ROOT/plugins/workflow/bin/"
