#!/usr/bin/env bash
# 拉取 models.dev api.json 并生成 zhuye-llm 内嵌快照。
#
# 产物两个,都落在 crates/abilities/zhuye-llm/assets/:
#   - api.json.gz      原始 api.json 的 gzip(口径与运行时拉取完全一致,
#                      聚合/投票在 Rust 加载期完成,快照与缓存共用
#                      DevCatalog::parse_api 一条解析路径,不存在格式漂移)
#   - api.json.gz.meta 快照抓取时刻(unix 秒),与磁盘缓存的 fetched_at
#                      比新旧,决定懒加载时用哪份数据
#
# 用法: bash scripts/fetch-models-dev.sh   (发版前或目录明显过期时手动跑)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="$ROOT/crates/abilities/zhuye-llm/assets"
URL="${MODELS_DEV_URL:-https://models.dev/api.json}"

mkdir -p "$OUT_DIR"
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

echo "拉取 $URL ..."
curl -fsSL --max-time 120 "$URL" -o "$tmp"
gzip -9 -c "$tmp" > "$OUT_DIR/api.json.gz"
date +%s > "$OUT_DIR/api.json.gz.meta"

size="$(du -h "$OUT_DIR/api.json.gz" | cut -f1)"
count="$(gzip -dc "$OUT_DIR/api.json.gz" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(sum(len(p.get("models", {})) for p in d.values()))')"
echo "models.dev 快照已更新:$size / $count 条模型($(date -r "$OUT_DIR/api.json.gz.meta" '+%Y-%m-%d %H:%M:%S'))"
