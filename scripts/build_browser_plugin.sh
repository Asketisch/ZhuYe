#!/usr/bin/env bash
# 构建 Browser 插件二进制:
#   1. shim(zhuye-browser-server)—— 必建;
#   2. agent-browser(上游)—— third_party/agent-browser submodule 已初始化
#      时一并构建进 bin/ 随插件分发;未初始化则跳过(运行时经 PATH 或
#      ZHUYE_AGENT_BROWSER 兜底)。
#
# 上游 cli/ 是独立单 crate cargo 项目,dashboard 前端缺失时其 build.rs 写
# 占位页兜底 —— 纯 cargo 可构建,不需要 Node/pnpm。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PROFILE="release"
if [[ "${1:-}" == "--fast" ]]; then
  PROFILE="release-fast"
fi

# Windows 下 cargo 产物带 .exe 后缀(git-bash 里 uname 报 MINGW*/MSYS*)。
EXE=""
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) EXE=".exe" ;;
esac

# 原子替换拷贝:先写临时名再 mv(换目录项/新 inode)。
# 修复原因:macOS 对 ad-hoc 签名二进制,若在进程运行期间被 cp 原地覆写,
# 旧文件 inode 的签名判定失效,之后从该路径执行一律 SIGKILL —— 曾导致
# 残留 daemon 未退出时重建插件,新 shim 拉起上游即秒退。mv 换目录项则
# 运行中进程持有旧 inode 不受影响,新执行拿到新签名的完整文件。
install_copy() {
  local src="$1" dst="$2"
  cp "$src" "$dst.tmp.install" && mv -f "$dst.tmp.install" "$dst"
}

cargo build --profile "$PROFILE" -p zhuye-browser --bin zhuye-browser-server
mkdir -p plugins/browser/bin
install_copy "target/$PROFILE/zhuye-browser-server$EXE" "plugins/browser/bin/zhuye-browser-server$EXE"

UPSTREAM_DIR="third_party/agent-browser"
if [ -f "$UPSTREAM_DIR/cli/Cargo.toml" ]; then
  # 上游是独立 crate(自带 Cargo.lock 与 profile 定义),不经 workspace,
  # 锁文件互不污染;--fast 映射为 debug(上游只定义了 release profile)。
  UP_PROFILE="release"
  UP_TARGET="release"
  if [[ "${1:-}" == "--fast" ]]; then
    UP_PROFILE="dev"
    UP_TARGET="debug"
  fi
  PIN="$(git -C "$UPSTREAM_DIR" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  cargo build --profile "$UP_PROFILE" --manifest-path "$UPSTREAM_DIR/cli/Cargo.toml"
  install_copy "$UPSTREAM_DIR/cli/target/$UP_TARGET/agent-browser$EXE" "plugins/browser/bin/agent-browser$EXE"
  # Apache-2.0 分发合规:二进制随插件分发时必须携带许可文本。
  install_copy "$UPSTREAM_DIR/LICENSE" plugins/browser/bin/agent-browser.LICENSE
  echo "已捆绑 agent-browser(pinned ${PIN},Apache-2.0,见 plugins/browser/NOTICE.md)"
else
  echo "跳过 agent-browser 捆绑:submodule 未初始化。运行时兜底任选其一:"
  echo "  ① git submodule update --init --depth 1 third_party/agent-browser 后重跑本脚本"
  echo "  ② PATH 安装(brew install agent-browser / npm i -g agent-browser)"
  echo "  ③ ZHUYE_AGENT_BROWSER=<agent-browser 可执行文件路径>"
fi

echo "构建完成:plugins/browser/bin/"
echo "安装:zhuye plugin install plugins/browser --scope user && zhuye plugin enable browser@inline"
