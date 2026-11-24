#!/usr/bin/env bash
# tests/live/cli_e2e.sh —— CLI agent 端到端演练套件(本地专用)。
#
# ## 定位:本地专用,不进 CI
#
# **只用于本地人工触发,不进 CI、不接 run_all.sh 门禁链**:真模型场景
# 依赖本机可达的端点,CI 环境没有。默认跳过(ZHUYE_LIVE=1)是双保险。
# 与桌面端 live/run.sh(桌面 IPC 链路 × 免费模型池)互补:这里走
# **headless `zhuye serve` 链路**,覆盖 agent 的协议面全谱:
#
#   plan 模式(出 plan / 打回修订 / 只读门控)
#   goal 模式(自校验达成 / 未达成续作 / 预算熔断 / goal-queue 插件)
#   上下文压缩(阈值自动 / 手动强制 / LLM 摘要 / rapid-refill 熔断 /
#             窗口占满收口)
#   prompt cache(空闲期保温;命中读取需真模型)
#   交互(中断 / 转向 / 提问 / 审批放行与拒绝)
#   会话(续作回放 / 回退 / 迭代上限 / token 预算)
#   扩展(子代理 / 后台任务 / 客户端远程工具 / 管理面 Op)
#
# 每个场景一个独立进程:崩溃 / 超时 / serve 异常互相隔离;每个检查项
# 折成一条 tcase(逐项独立报 ok / FAIL / skip)。
#
# ## 两种跑法
#
#   # 1) 自检模式(mock provider,离线、不发网络请求):验证场景脚本本身
#   ZHUYE_E2E_MOCK=1 bash tests/live/cli_e2e.sh
#
#   # 2) 真模型模式(需要 cli_e2e.toml 里的端点可达)
#   ZHUYE_LIVE=1 bash tests/live/cli_e2e.sh
#
# 常用环境变量:
#   ZHUYE_E2E_TOML      —— 配置路径(默认 tests/live/cli_e2e.toml)
#   ZHUYE_E2E_BIN       —— zhuye 二进制(默认 target/debug/zhuye,缺失则构建)
#   ZHUYE_E2E_SCENARIOS —— 只跑指定场景(空格分隔,默认全部)
#   ZHUYE_E2E_KEEP=1    —— 保留临时目录(事件 JSONL / serve 日志)供翻查
#   ZHUYE_E2E_TIMEOUT   —— 单场景超时秒数(默认 900)
#
# ## 断言口径
#
# 每个场景是一组**结构化检查项**(协议事件形状 / 产物存在性 / 终态
# 语义),由 cli_e2e_run.py 逐项产出。不断言模型文本内容(模型自由
# 发挥,文本断言不可靠)。
#
# ## 隔离
#
# 每个场景一个独立临时目录:假 HOME(ZHUYE_HOME)+ 独立引擎配置 +
# 独立工作区 —— rollout / session notes / traces 全落临时目录,
# 不读、不写真实 ~/.zhuye。git 身份经假 HOME 的 .gitconfig 提供。

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/../lib.sh"

# ── 0. 二进制 ──────────────────────────────────────────────────────────────
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

# ── 1. 配置与模式 ──────────────────────────────────────────────────────────
TOML="${ZHUYE_E2E_TOML:-$HERE/cli_e2e.toml}"
MOCK="${ZHUYE_E2E_MOCK:-0}"
TIMEOUT="${ZHUYE_E2E_TIMEOUT:-900}"
KEEP="${ZHUYE_E2E_KEEP:-0}"

if [ "$MOCK" != "1" ] && [ "${ZHUYE_LIVE:-}" != "1" ]; then
    echo "  [SKIP] CLI 端到端演练套件 —— 未设 ZHUYE_LIVE=1(真模型)且未设 ZHUYE_E2E_MOCK=1(自检),默认不发网络请求"
    exit 0
fi

if [ ! -f "$TOML" ]; then
    if [ -f "$HERE/cli_e2e.example.toml" ]; then
        cp "$HERE/cli_e2e.example.toml" "$TOML"
        echo "  已从模板生成本地配置:$TOML(填写真实端点后重跑)"
    else
        echo "  [SKIP] CLI 端到端演练套件 —— 找不到测试配置 $TOML"
        exit 0
    fi
fi

# ── 2. 场景清单(来自注册表;可用 ZHUYE_E2E_SCENARIOS 过滤)──────────────
SCENARIOS=$(python3 - "$HERE" << 'PYEOF'
import sys
sys.path.insert(0, sys.argv[1])
from cli_e2e_scenarios import SCENARIOS
print(" ".join(sorted(SCENARIOS)))
PYEOF
)
if [ -n "${ZHUYE_E2E_SCENARIOS:-}" ]; then
    SCENARIOS="$ZHUYE_E2E_SCENARIOS"
fi
if [ -z "$SCENARIOS" ]; then
    tcase "场景注册表可读" false
    tfinish
    exit 1
fi
echo "  模式:$([ "$MOCK" = "1" ] && echo '自检(mock provider,离线)' || echo '真模型')"
echo "  场景:$SCENARIOS"

# ── 3. 逐场景执行(一个进程一场景:崩溃 / 超时互相隔离)────────────────────
WORK="$(mktemp -d /tmp/zhuye-cli-e2e-suite.XXXXXX)"
cleanup() {
    if [ "$KEEP" = "1" ]; then
        echo "  [keep] 临时目录保留:$WORK"
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

for scenario in $SCENARIOS; do
    echo
    echo "  ── 场景:$scenario ──"
    SC_DIR="$WORK/$scenario"
    RESULT="$SC_DIR/result.json"
    mkdir -p "$SC_DIR"

    ARGS=(--scenario "$scenario" --bin "$BIN" --work "$SC_DIR/ws-root"
          --toml "$TOML" --result "$RESULT")
    [ "$MOCK" = "1" ] && ARGS+=(--mock)

    timeout "$TIMEOUT" python3 "$HERE/cli_e2e_run.py" "${ARGS[@]}" \
        > "$SC_DIR/run.out" 2> "$SC_DIR/run.err"
    rc=$?
    if [ "$rc" = "124" ]; then
        # 超时既可能是端点排队(27B 模型 xhigh 思考 + 全套连跑),也可能是
        # 引擎挂死 —— 都该被看见,所以判 FAIL;日志尾巴给出定位线索。
        tcase "$scenario · 场景执行未超时(${TIMEOUT}s)" bash -c \
            'printf "超时 ${TIMEOUT}s:可能是端点排队/慢;也查一遍 serve 日志确认不是引擎挂死\n"; exit 1'
        tail -5 "$SC_DIR/run.err" 2>/dev/null | sed 's/^/         │ /'
        continue
    fi
    if [ "$rc" != "0" ] && [ ! -s "$RESULT" ]; then
        tcase "$scenario · 场景执行器正常产出结果" false
        tail -20 "$SC_DIR/run.err" | sed 's/^/         │ /'
        continue
    fi

    # 逐个检查项折成一条 tcase(名称带场景前缀,便于定位)。
    while IFS=$'\t' read -r status name detail; do
        [ -z "$name" ] && continue
        case "$status" in
            ok)
                tcase "$scenario · $name" true
                ;;
            FAIL)
                tcase "$scenario · $name" bash -c 'printf "%s\n" "$1"; exit 1' _ "$detail"
                ;;
            *)
                echo "  [skip] $scenario · $name —— $detail"
                ;;
        esac
    done < <(python3 - "$RESULT" << 'PYEOF'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as f:
    result = json.load(f)
for item in result.get("checks", []):
    status = item.get("status")
    tag = "ok" if status == "pass" else ("FAIL" if status == "fail" else "SKIP")
    detail = (item.get("detail") or "").replace("\n", " ⏎ ")
    print(f"{tag}\t{item.get('name')}\t{detail}")
PYEOF
    )
done

echo
echo "  结果 JSON 目录:$WORK"
tfinish
