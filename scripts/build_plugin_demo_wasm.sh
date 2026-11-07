#!/usr/bin/env bash
# 构建 plugin-demo 的 Rust WASM 组件样例(C4)。
# 前置:rustup target add wasm32-wasip2
# 产物:plugins/plugin-demo/bin/zhuye-demo-hook.wasm / zhuye-demo-tool.wasm
#       (样例产物随插件入库;本脚本用于改源码后重建)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEMO="$ROOT/plugins/plugin-demo/wasm/rust"

rustup target list --installed | grep -q wasm32-wasip2 || {
    echo "── 安装 wasm32-wasip2 target ──"
    rustup target add wasm32-wasip2
}

for proj in zhuye-demo-hook zhuye-demo-tool; do
    echo "── 构建 $proj ──"
    (cd "$DEMO/$proj" && cargo build --release --target wasm32-wasip2)
    mkdir -p "$ROOT/plugins/plugin-demo/bin"
    cp "$DEMO/$proj/target/wasm32-wasip2/release/${proj//-/_}.wasm" \
       "$ROOT/plugins/plugin-demo/bin/$proj.wasm"
done
ls -la "$ROOT/plugins/plugin-demo/bin/"
