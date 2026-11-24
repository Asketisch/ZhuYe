#!/usr/bin/env bash
# tests/live/cli_e2e.sh —— CLI 真模型端到端演练(headless 链路全流程)。
#
# ## 定位:本地专用
#
# **只用于本地人工触发的演练,不进 CI、不接 run_all.sh 门禁链**:
# 依赖本机可达的真实模型端点与 cli_e2e.toml 里的模型组,CI 环境两者
# 皆无。默认跳过(ZHUYE_LIVE=1)是双保险 —— 即使误触发也不发网络请求。
# 与桌面端 live/run.sh(桌面 IPC 链路 × 免费模型池)互补:这里走
# **用户配置的真实模型**测完整 agent 工作流(plan → 确认 → 开发 →
# git commit),是沙箱 / 压缩 / plan 门控 / 审批流在真实模型下的
# 端到端回归(桌面 sess_99d9bf0a 实证的两类引擎缺陷即由此发现)。
#
#   实际构建 → 隔离工作区 + plan 模式启动 → agent 出具 plan
#   → 确认 plan(自动批准)→ 正常开发 → git init + commit → 断言产物
#
# 这是沙箱 / 压缩 / plan 门控 / 审批流 / 工具执行**在真实模型下的端到端
# 证据**:桌面 sess_99d9bf0a 实测发现的「沙箱 .git 保护堵死 git init /
# commit」「幽灵压缩误触熔断」两类缺陷,单测各能覆盖一半,只有这个
# 脚本能把整条链一次走通。
#
# ## 配置来源
#
# `tests/live/cli_e2e.toml`(全中文注释,含端点 / 密钥 / 模型 / 思考
# 强度 / 任务提示词 / 验收期望),脚本读取后在隔离 HOME 里生成独立的
# 引擎配置 —— 不读、不写真实 ~/.zhuye。字段级环境变量覆盖(优先于
# toml),名字 = `ZHUYE_E2E_` + 大写字段名:
#
#   ZHUYE_E2E_BASE_URL / ZHUYE_E2E_API_KEY / ZHUYE_E2E_MODEL /
#   ZHUYE_E2E_ENDPOINT / ZHUYE_E2E_CONTEXT_WINDOW / ZHUYE_E2E_EFFORT /
#   ZHUYE_E2E_EXPECT_ARTIFACT / ZHUYE_E2E_EXPECT_COMMIT /
#   ZHUYE_E2E_TASK_FILE / ZHUYE_E2E_CONTINUE_FILE(prompt 整文件覆盖)
#
# ## 跑法与跳过
#
#   ZHUYE_LIVE=1 bash tests/live/cli_e2e.sh
#   ZHUYE_E2E_TOML=/path/to/my-e2e.toml ZHUYE_LIVE=1 bash tests/live/cli_e2e.sh
#   # 验证桌面端 pin 态:ZHUYE_E2E_BIN 指向桌面构建的二进制
#   ZHUYE_E2E_BIN=../ZhuYe-Desktop/zhuye-agent/target/debug/zhuye \
#     ZHUYE_LIVE=1 bash tests/live/cli_e2e.sh
#
# 未设 `ZHUYE_LIVE=1` 时整层 SKIP —— 不发任何网络请求;端点不可达判
# **SKIP** 而非失败 —— 测的是代码,不是网络。事件 / 结果 JSON 落临时
# 目录,默认随清理消失;ZHUYE_E2E_KEEP=1 保留供事后翻查。
#
# ## 断言口径
#
# 只断言**协议形状 + 产物存在性**(与 toml 的 expect_* 配对):plan
# markdown 非空、审批后权限切换、全程无结构性错误(尤其
# compact_rapid_refill)、工作区产物齐、git 提交由 agent 在沙箱内完成。
# 不断言模型文本。
#
# ## 隔离
#
# mktemp 假 HOME + ZHUYE_HOME:引擎配置由脚本生成,rollout / session
# notes 全落临时目录,不污染真实 ~/.zhuye。git 身份经假 HOME 的
# .gitconfig 提供(沙箱内可读)。

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/../lib.sh"

if [ "${ZHUYE_LIVE:-}" != "1" ]; then
    echo "  [SKIP] CLI 真模型端到端 —— 未设 ZHUYE_LIVE=1,默认不发网络请求"
    exit 0
fi

# ── 0. 定位 / 构建二进制 ────────────────────────────────────────────────────
BIN="${ZHUYE_E2E_BIN:-$ROOT/target/debug/zhuye}"
if [ ! -x "$BIN" ]; then
    echo "  未找到 $BIN,执行 cargo build -p zhuye-cli …"
    if ! (cd "$ROOT" && cargo build -p zhuye-cli) > "$WORK/cli-e2e-build.log" 2>&1; then
        tail -30 "$WORK/cli-e2e-build.log" | sed 's/^/         │ /'
        tcase "构建 zhuye-cli" false
        tfinish
        exit 1
    fi
fi
[ -x "$BIN" ] || { tcase "zhuye 二进制可执行" false; tfinish; exit 1; }

# ── 1. 读取测试配置(cli_e2e.toml;环境变量字段级覆盖)─────────────────────
TOML="${ZHUYE_E2E_TOML:-$HERE/cli_e2e.toml}"
if [ ! -f "$TOML" ] && [ -f "$HERE/cli_e2e.example.toml" ]; then
    # 首次使用:从模板落一份本地配置(gitignore,真实端点 / 密钥
    # 不入库),按注释填好即用。
    cp "$HERE/cli_e2e.example.toml" "$TOML"
    echo "  已从模板生成本地配置:$TOML(填写真实端点后重跑)"
fi
if [ ! -f "$TOML" ]; then
    echo "  [SKIP] CLI 真模型端到端 —— 找不到测试配置 $TOML"
    exit 0
fi

WORK="$(mktemp -d /tmp/zhuye-cli-e2e.XXXXXX)"
KEEP="${ZHUYE_E2E_KEEP:-0}"
cleanup() {
    if [ "$KEEP" = "1" ]; then
        echo "  [keep] 临时目录保留:$WORK"
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

# 解析 toml:标量按 KEY<TAB>VALUE 打到 stdout(多行任务文本直接落文件)。
# 需要 python ≥ 3.11(tomllib)。
if ! python3 - "$TOML" "$WORK" > "$WORK/config.env" << 'PYEOF'
import sys
import tomllib

toml_path, work = sys.argv[1], sys.argv[2]
with open(toml_path, "rb") as f:
    cfg = tomllib.load(f)
ep = cfg.get("endpoint") or {}
run = cfg.get("run") or {}
task = cfg.get("task") or {}

scalars = {
    "BASE_URL": ep.get("base_url") or "",
    "API_KEY": ep.get("api_key") or "",
    "MODEL": ep.get("model") or "",
    "ENDPOINT": ep.get("endpoint") or "openai",
    "CONTEXT_WINDOW": str(ep.get("context_window") or ""),
    "EFFORT": run.get("effort") or "xhigh",
    "PHASE_TIMEOUT": str(run.get("phase_timeout") or 1200),
    "EXPECT_ARTIFACT": task.get("expect_artifact") or "",
    "EXPECT_COMMIT": task.get("expect_commit_subject") or "",
}
for key, value in scalars.items():
    print(f"{key}\t{value}")

# 多行任务文本直接落文件(bash 侧不再经手,避免转义损耗)。
with open(f"{work}/task.txt", "w", encoding="utf-8") as f:
    f.write(str(task.get("prompt") or "").strip() + "\n")
with open(f"{work}/continue.txt", "w", encoding="utf-8") as f:
    f.write(str(task.get("continue_prompt") or "").strip() + "\n")
PYEOF
then
    tail -10 "$WORK/config.env" | sed 's/^/         │ /'
    tcase "解析测试配置(需要 python ≥ 3.11)" false
    tfinish
    exit 1
fi

# KEY<TAB>VALUE → 普通变量(macOS 自带 bash 3.2 无 declare -A);
# 环境变量(同名加 ZHUYE_E2E_ 前缀)逐字段覆盖。
CFG_BASE_URL=""; CFG_API_KEY=""; CFG_MODEL=""; CFG_ENDPOINT="openai"
CFG_CONTEXT_WINDOW=""; CFG_EFFORT="xhigh"; CFG_PHASE_TIMEOUT="1200"
CFG_EXPECT_ARTIFACT=""; CFG_EXPECT_COMMIT=""
while IFS=$'\t' read -r key value; do
    case "$key" in
        BASE_URL)         CFG_BASE_URL="$value" ;;
        API_KEY)          CFG_API_KEY="$value" ;;
        MODEL)            CFG_MODEL="$value" ;;
        ENDPOINT)         CFG_ENDPOINT="$value" ;;
        CONTEXT_WINDOW)   CFG_CONTEXT_WINDOW="$value" ;;
        EFFORT)           CFG_EFFORT="$value" ;;
        PHASE_TIMEOUT)    CFG_PHASE_TIMEOUT="$value" ;;
        EXPECT_ARTIFACT)  CFG_EXPECT_ARTIFACT="$value" ;;
        EXPECT_COMMIT)    CFG_EXPECT_COMMIT="$value" ;;
    esac
done < "$WORK/config.env"
[ -n "${ZHUYE_E2E_BASE_URL:-}" ]          && CFG_BASE_URL="$ZHUYE_E2E_BASE_URL"
[ -n "${ZHUYE_E2E_API_KEY:-}" ]           && CFG_API_KEY="$ZHUYE_E2E_API_KEY"
[ -n "${ZHUYE_E2E_MODEL:-}" ]             && CFG_MODEL="$ZHUYE_E2E_MODEL"
[ -n "${ZHUYE_E2E_ENDPOINT:-}" ]          && CFG_ENDPOINT="$ZHUYE_E2E_ENDPOINT"
[ -n "${ZHUYE_E2E_CONTEXT_WINDOW:-}" ]    && CFG_CONTEXT_WINDOW="$ZHUYE_E2E_CONTEXT_WINDOW"
[ -n "${ZHUYE_E2E_EFFORT:-}" ]            && CFG_EFFORT="$ZHUYE_E2E_EFFORT"
[ -n "${ZHUYE_E2E_PHASE_TIMEOUT:-}" ]     && CFG_PHASE_TIMEOUT="$ZHUYE_E2E_PHASE_TIMEOUT"
[ -n "${ZHUYE_E2E_EXPECT_ARTIFACT:-}" ]   && CFG_EXPECT_ARTIFACT="$ZHUYE_E2E_EXPECT_ARTIFACT"
[ -n "${ZHUYE_E2E_EXPECT_COMMIT:-}" ]     && CFG_EXPECT_COMMIT="$ZHUYE_E2E_EXPECT_COMMIT"

if [ -z "$CFG_BASE_URL" ] || [ -z "$CFG_MODEL" ]; then
    echo "  [SKIP] CLI 真模型端到端 —— 测试配置缺 base_url / model($TOML)"
    exit 0
fi
PHASE_TIMEOUT="$CFG_PHASE_TIMEOUT"
echo "  目标:$CFG_MODEL @ $CFG_BASE_URL(effort=$CFG_EFFORT)"

# ── 2. 生成隔离引擎配置 + git 身份 ─────────────────────────────────────────
mkdir -p "$WORK/home/.zhuye"
cat > "$WORK/home/.gitconfig" << 'EOF'
[user]
    name = zhuye-e2e
    email = zhuye-e2e@example.com
[commit]
    gpgsign = false
EOF
# 引擎配置:最小独立面(不继承真实 ~/.zhuye 的插件 / MCP / hooks,
# 演练结果只取决于本 toml + 引擎默认)。allow_network = true:任务夹具
# 虽然纯本地,但不人为限制 agent 的合法依赖(如文档查询)。
python3 - "$WORK" > "$WORK/home/.zhuye/config.toml" << 'PYEOF'
import json
import sys

work = sys.argv[1]
scalars = {}
for line in open(f"{work}/config.env", encoding="utf-8"):
    key, _, value = line.rstrip("\n").partition("\t")
    scalars[key] = value
window = scalars.get("CONTEXT_WINDOW") or ""

lines = [
    "# 由 tests/live/cli_e2e.sh 生成 —— CLI 真模型端到端演练的独立引擎配置。",
    "[active]",
    'group = "e2e"',
    "",
    "[[model_group]]",
    'id = "e2e"',
    f'endpoint = "{scalars.get("ENDPOINT", "openai")}"',
    f"api_key = {json.dumps(scalars.get('API_KEY', ''))}",
    f"base_url = {json.dumps(scalars.get('BASE_URL', ''))}",
    f"model = {json.dumps(scalars.get('MODEL', ''))}",
    "responses_api = false",
    "credentials = []",
    "",
]
if window.isdigit():
    lines += [
        "[context_windows.entries]",
        f'{json.dumps(scalars["MODEL"])} = {int(window)}',
        "",
    ]
lines += [
    "[sandbox]",
    "os_level = true",
    "writable_dirs = []",
    "allow_network = true",
    "",
    "[plugins]",
    "enabled_plugins = []",
    "disabled_plugins = []",
]
print("\n".join(lines))
PYEOF
WS="$WORK/ws"
mkdir -p "$WS"

# ── 3. 端点预检:不可达 → SKIP(不烧单阶段超时)───────────────────────────
# 可达性 = 有任何 HTTP 响应(鉴权 401 也算可达 —— 只关心连通,不关心配额)。
HTTP_CODE=$(curl -s -m 10 -o /dev/null -w "%{http_code}" \
    -H "Authorization: Bearer $CFG_API_KEY" \
    "$CFG_BASE_URL/models" 2>/dev/null)
if [ -z "$HTTP_CODE" ] || [ "$HTTP_CODE" = "000" ]; then
    echo "  [SKIP] CLI 真模型端到端 —— 模型端点不可达($CFG_BASE_URL),环境问题不判失败"
    exit 0
fi
echo "  端点可达:$CFG_BASE_URL (http=$HTTP_CODE)"

# ── 4. 驱动(阶段超时来自 toml 的 run.phase_timeout)───────────────────────
EVENTS="$WORK/cli-e2e-events.jsonl"
RESULT_JSON="$WORK/cli-e2e-result.json"
python3 "$HERE/cli_e2e_driver.py" \
    --bin "$BIN" \
    --workspace "$WS" \
    --home "$WORK/home" \
    --task-file "$WORK/task.txt" \
    --continue-file "$WORK/continue.txt" \
    --effort "$CFG_EFFORT" \
    --events "$EVENTS" \
    --result "$RESULT_JSON" \
    --phase-timeout "$PHASE_TIMEOUT" \
    > "$WORK/cli-e2e-summary.json" 2> "$WORK/cli-e2e-driver.err"
DRIVER_STATUS=$?

# ── 5. 断言(结构化;产物与 git 状态直接对工作区验)────────────────────────
if [ "$DRIVER_STATUS" -eq 2 ]; then
    # 环境性跳过(端点不可达):不算失败。
    echo "  [SKIP] CLI 真模型端到端 —— 模型端点不可达(见 $RESULT_JSON)"
    exit 0
fi

if [ ! -s "$RESULT_JSON" ]; then
    echo "  驱动器没有产出结果 JSON"
    tail -30 "$WORK/cli-e2e-driver.err" | sed 's/^/         │ /'
    tcase "结果 JSON 产出" false
    tfinish
    exit 1
fi

# 分项断言,各读结果 JSON 的一个键。
EXPECT_ARTIFACT="$CFG_EXPECT_ARTIFACT"
EXPECT_COMMIT="$CFG_EXPECT_COMMIT"
tcase "会话配置(模型解析成功)" python3 -c "
import json, sys
r = json.load(open('$RESULT_JSON'))
sys.exit(0 if r.get('model') else 1)"
tcase "plan 正式产出(markdown 非空)" python3 -c "
import json, sys
r = json.load(open('$RESULT_JSON'))
sys.exit(0 if r.get('plan_ready') and r.get('plan_markdown_chars', 0) > 0 else 1)"
tcase "plan 审批生效(进入开发模式)" python3 -c "
import json, sys
r = json.load(open('$RESULT_JSON'))
sys.exit(0 if r.get('plan_approved') and any('accept_edits' in p for p in r.get('permission_flow', [])) else 1)"
tcase "全程无结构性错误(含 compact_rapid_refill)" python3 -c "
import json, sys
r = json.load(open('$RESULT_JSON'))
bad = {'compact_rapid_refill', 'invalid_submission', 'prompt_rejected'}
sys.exit(0 if not (set(r.get('error_codes', [])) & bad) else 1)"
tcase "回合终态 success" python3 -c "
import json, sys
r = json.load(open('$RESULT_JSON'))
sys.exit(0 if r.get('status') == 'PASS' else 1)"
tcase "产物存在($EXPECT_ARTIFACT)" test -s "$WS/$EXPECT_ARTIFACT"
tcase "git 仓库含 agent 提交(沙箱回归核心断言)" bash -c '
    git -C "'"$WS"'" rev-parse --verify HEAD >/dev/null 2>&1 &&
    git -C "'"$WS"'" log --format=%s | grep -q "'"$EXPECT_COMMIT"'"'

echo
echo "  事件留档:$EVENTS"
echo "  结果 JSON:$RESULT_JSON($([ "$KEEP" = 1 ] && echo "临时目录保留:$WORK" || echo "临时目录已清理,ZHUYE_E2E_KEEP=1 可保留"))"
tfinish
