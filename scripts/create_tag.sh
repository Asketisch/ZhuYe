#!/usr/bin/env bash
# scripts/create_tag.sh —— 一站式发版 tag:版本 bump + 提交 + 打 tag + 推送。
#
# 用法:
#   ./scripts/create_tag.sh             # 默认:当前四处版本末位 +1(0.1.8 → 0.1.9)
#   ./scripts/create_tag.sh 0.2.0       # 自定义版本(带 v 前缀亦可)
#   ./scripts/create_tag.sh --dry-run   # 只打印计划,不改动任何东西
#   ./scripts/create_tag.sh --no-push   # 本地提交 + 打 tag,但不推送
#   ./scripts/create_tag.sh --yes 0.2.0 # 跳过推送前确认
#
# 单条命令完成的事(与 release-sdks.yml 的 gate 口径一致,杜绝
# 「tag 版本 != 仓库版本」这一类发布失败):
#   1. 前置校验:main 分支 + 工作区干净 + 不落后远端 + 四处版本一致
#   2. 计算目标版本(无参数时当前版本末位 +1;预发布后缀版本必须显式指定)
#   3. 四处版本同步 bump + cargo check 刷新 Cargo.lock + 事后复核
#   4. 提交 build(release): 版本 bump <ver>
#   5. 打 annotated tag v<ver> 并连同 main 一起推送
#
# ⚠️ 推送 v* tag 会同时触发两条发布流水线(同 release_sdk.sh tag):
#   - release.yml      → CLI 二进制发到 GitHub Release(公开)
#   - release-sdks.yml → Python SDK 发 PyPI、TS SDK 发 npm(公开且不可撤回)

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PY_PROJECT="$ROOT/sdks/python/pyproject.toml"
PY_INIT="$ROOT/sdks/python/zhuye/__init__.py"
TS_PACKAGE="$ROOT/sdks/typescript/package.json"
CARGO_TOML="$ROOT/Cargo.toml"

die() { echo "FAIL: $*" >&2; exit 1; }

# ── 四处版本号读取(与流水线 gate / release_sdk.sh 保持同一口径)──────────
cargo_version() {
  awk '/^\[workspace\.package\]/{f=1;next} /^\[/{f=0} f && /^version = /{print; exit}' \
    "$CARGO_TOML" | cut -d'"' -f2
}
python_version() {
  sed -n 's/^version = "\(.*\)"/\1/p' "$PY_PROJECT" | head -n1
}
py_init_version() {
  sed -n 's/^__version__ = "\(.*\)"/\1/p' "$PY_INIT" | head -n1
}
ts_version() {
  node -p "require('$TS_PACKAGE').version"
}
versions_consistent() {
  local c p i t
  c="$(cargo_version)"; p="$(python_version)"; i="$(py_init_version)"; t="$(ts_version)"
  [ -n "$c" ] && [ "$c" = "$p" ] && [ "$p" = "$i" ] && [ "$i" = "$t" ]
}

is_semver() {
  printf '%s' "$1" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'
}

# ── 参数解析 -------------------------------------------------------------
DRY_RUN=false NO_PUSH=false ASSUME_YES=false VER_ARG=""
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=true ;;
    --no-push) NO_PUSH=true ;;
    --yes|-y)  ASSUME_YES=true ;;
    -h|--help) sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) [ -z "$VER_ARG" ] || die "只能指定一个版本,已收到:$VER_ARG 与 $arg"
       VER_ARG="$arg" ;;
  esac
done

# ── 1. 前置校验 ----------------------------------------------------------
BRANCH="$(git -C "$ROOT" rev-parse --abbrev-ref HEAD)"
# 分支约束属于安全边界(不可绕过):发版 tag 必须打在 main 上,
# 否则版本提交与 tag 会落在未合并的分支上,流水线 gate 拿到的代码与 main 不符。
[ "$BRANCH" = "main" ] || die "当前分支是 $BRANCH —— 本脚本只能在 main 分支使用。"

[ -z "$(git -C "$ROOT" status --porcelain)" ] \
  || die "工作区不干净 —— 版本提交不能混入无关改动,请先提交或暂存(stash)。"

# 本地不得落后远端(领先没关系,版本提交马上会推上去);fetch 失败仅警告(离线可用)。
if git -C "$ROOT" fetch origin main --quiet 2>/dev/null; then
  BEHIND="$(git -C "$ROOT" rev-list --count main..origin/main)"
  [ "$BEHIND" = "0" ] || die "本地 main 落后 origin/main $BEHIND 个提交 —— 先 git pull。"
else
  echo "WARN: 无法 fetch origin(离线?)—— 跳过远端同步校验。"
fi

versions_consistent || die "四处版本不一致 —— 先修复后再发版:
  Rust workspace: $(cargo_version)
  Python SDK:     $(python_version)
  Python __version__: $(py_init_version)
  TS SDK:         $(ts_version)"

CUR="$(cargo_version)"

# ── 2. 计算目标版本 ------------------------------------------------------
if [ -n "$VER_ARG" ]; then
  NEW="${VER_ARG#v}"                       # 兼容 v0.2.0 写法
  is_semver "$NEW" || die "非法版本号:$VER_ARG(应为 0.2.0 这样的三段式)"
else
  # 末位 +1:最后一段必须是纯数字;预发布版本(0.2.0-rc.1)语义上
  # 「+1」不明确,必须显式指定目标版本。
  LAST="${CUR##*.}"
  printf '%s' "$LAST" | grep -Eq '^[0-9]+$' \
    || die "当前版本 $CUR 带预发布后缀,无法自动 +1 —— 请显式指定目标版本。"
  NEW="${CUR%.*}.$((LAST + 1))"
fi
[ "$NEW" != "$CUR" ] || die "目标版本 $NEW 与当前版本相同,无事可做。"

git -C "$ROOT" rev-parse -q --verify "refs/tags/v$NEW" > /dev/null \
  && die "tag v$NEW 已存在 —— 换一个版本,或先清理旧 tag。"

echo "发版计划:$CUR → $NEW,tag v$NEW(分支 $BRANCH)"

if $DRY_RUN; then
  echo
  echo "── dry-run:将执行以下动作 ──"
  echo "  1. bump 四处版本:$CARGO_TOML / $PY_PROJECT / $PY_INIT / $TS_PACKAGE"
  echo "  2. cargo check --workspace 刷新 Cargo.lock"
  echo "  3. git commit -m 'build(release): 版本 bump $NEW'"
  echo "  4. git tag -a v$NEW"
  $NO_PUSH && echo "  5. (--no-push)不推送" \
            || echo "  5. git push origin main v$NEW(触发 release + release-sdks 流水线)"
  echo "dry-run 结束,未改动任何东西。"
  exit 0
fi

# ── 3. 四处版本同步写入 + Cargo.lock 刷新 --------------------------------
# 与 release_sdk.sh bump 同款锚点替换:只动四处版本锚点行,不碰依赖版本。
python3 - "$CARGO_TOML" "$PY_PROJECT" "$PY_INIT" "$TS_PACKAGE" "$CUR" "$NEW" <<'PYEOF'
import pathlib, sys

cargo, pyproject, pyinit, ts, old, new = sys.argv[1:7]

def replace_line(path, old_line, new_line, anchor=None):
    p = pathlib.Path(path)
    text = p.read_text(encoding="utf-8")
    if anchor:  # 仅限 TOML 指定段内(如 [workspace.package])
        lines, inside, done, out = text.splitlines(keepends=True), False, False, []
        for ln in lines:
            if ln.startswith("["):
                inside = ln.startswith(anchor)
            if inside and not done and ln.rstrip("\n") == old_line:
                ln, done = new_line + "\n", True
            out.append(ln)
        assert done, f"{path} 的 {anchor} 段中未找到 {old_line!r}"
        p.write_text("".join(out), encoding="utf-8")
    else:
        assert text.count(old_line) == 1, f"{path} 中 {old_line!r} 不唯一或缺失"
        p.write_text(text.replace(old_line, new_line), encoding="utf-8")

replace_line(cargo,    f'version = "{old}"',      f'version = "{new}"',      "[workspace.package]")
replace_line(pyproject, f'version = "{old}"',      f'version = "{new}"')
replace_line(pyinit,   f'__version__ = "{old}"',   f'__version__ = "{new}"')
replace_line(ts,       f'"version": "{old}"',      f'"version": "{new}"')
PYEOF

# cargo check 全量通过顺便验证编译;lock 的版本记录随之刷新。
cargo check --workspace --quiet 2>/dev/null

# 复核:四处 == NEW,且 Cargo.lock 确实被刷新(版本变化必然产生 diff)。
CUR2="$(cargo_version)"
[ "$CUR2" = "$NEW" ] && versions_consistent \
  || die "bump 后四处版本复核失败(当前 Rust workspace:$CUR2,期望 $NEW)。"
git -C "$ROOT" diff --quiet -- Cargo.lock \
  && die "Cargo.lock 未被刷新 —— 请手动执行 cargo check --workspace 后重试。"
# lock 里所有 zhuye* 包的版本必须都已到 NEW(第三方依赖恰好同名版本不算残留)。
STALE_LOCK="$(awk -v want="\"$NEW\"" '
  /^name = "zhuye/ { n = $3 }
  /^version = /   { if (n != "" && $3 != want) print n, $3; n = "" }
' "$ROOT/Cargo.lock")"
[ -z "$STALE_LOCK" ] || die "Cargo.lock 中 zhuye 包仍有旧版本:$STALE_LOCK"

# ── 4. 提交版本改动 -------------------------------------------------------
git -C "$ROOT" add "$CARGO_TOML" Cargo.lock "$PY_PROJECT" "$PY_INIT" "$TS_PACKAGE"
git -C "$ROOT" commit -m "build(release): 版本 bump $NEW"

# ── 5. 打 tag + 推送 ------------------------------------------------------
git -C "$ROOT" tag -a "v$NEW" -m "zhuye v$NEW(CLI + Python/TS SDK 统一版本)"
echo
echo "✓ 版本已提交,tag v$NEW 已创建。"

if $NO_PUSH; then
  echo "(--no-push)未推送。后续手动执行:"
  echo "  git push origin main v$NEW"
  exit 0
fi

echo
echo "⚠️  推送 v$NEW 将触发正式发布:"
echo "   - release.yml      → CLI 二进制发到 GitHub Release(公开)"
echo "   - release-sdks.yml → Python SDK 发 PyPI、TS SDK 发 npm(公开且不可撤回)"
if ! $ASSUME_YES; then
  read -r -p "确认推送?输入 yes 继续:" ans
  [ "$ans" = "yes" ] || { echo "已取消(tag 与版本提交保留在本地,未推送)。"; exit 1; }
fi
git -C "$ROOT" push origin main "v$NEW"
echo "tag v$NEW 已推送,发布流水线已触发(可在 Actions 页查看进度)。"
