#!/usr/bin/env bash
# tests/rust/env_lock_stress.sh —— env 竞态回归压测(zhuye-config)。
#
# 背景(`44ae3da` 修复的回归门禁):
#   zhuye-config 的测试进程里,load.rs 的 zhuye_home_redirects_config_load
#   并发改写 ZHUYE_HOME;paths.rs 的 data_path_appends_subdirectories 要
#   **读两次** env(data_dir + data_path 内部各一次)。改写用的是函数局部
#   锁,锁不住 paths.rs 的读者 —— 两次读取落在并发改写的前后两侧时断言
#   必挂(CI macos 实证:left=临时目录/right=~/.zhuye)。修复 = crate 级
#   paths::ENV_LOCK(#[]cfg(test))串行化,读 env 与改 env 的测试共用。
#
# 竞态无法用单条用例确定性复现(依赖线程调度),本脚本把「跑一次」放大成
# 「连跑 N 次」反复采样调度器:锁纪律一旦回归(锁改回函数局部 / 新增改
# env 测试不持锁 / 新增读 env 测试不持锁),压测会把它打出来,失败即失败。
#
# 用法:
#   由 tests/rust/run.sh 第 9 步自动执行;也可独立跑:
#   bash tests/rust/env_lock_stress.sh            # 默认 20 轮
#   ZHUYE_ENV_STRESS_RUNS=50 bash tests/rust/env_lock_stress.sh
#
# 全程离线,不污染真实 ~/.zhuye(测试自带隔离)。

set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$HERE/../lib.sh"

cd "$ROOT"

RUNS="${ZHUYE_ENV_STRESS_RUNS:-20}"

# ── 负载放大器 ──────────────────────────────────────────────────────────
# 裸跑调度抖动太小,竞速窗口(改 env 的 set_var 落在读者两次读取之间)
# 采样命中率低 —— 本机验证过 40 轮打不出来。压测期间打满 CPU 核数的
# 空转进程,模拟 CI runner 的满载调度,把调度抖动放大到可命中。
# ZHUYE_ENV_STRESS_LOAD=0 可关掉(默认核数)。
LOAD_PIDS=()
start_load() {
    local n="${ZHUYE_ENV_STRESS_LOAD:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)}"
    [ "$n" -le 0 ] && return 0
    for _ in $(seq 1 "$n"); do
        ( while :; do :; done ) &
        LOAD_PIDS+=($!)
    done
}
stop_load() {
    local pid
    for pid in "${LOAD_PIDS[@]:-}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
    LOAD_PIDS=()
}
trap stop_load EXIT

env_lock_stress() {
    local fails=0
    start_load
    for i in $(seq 1 "$RUNS"); do
        if cargo test -p zhuye-config --lib --quiet; then
            # 每 5 轮报一次进度,避免 CI 长时间静默。
            if [ $((i % 5)) -eq 0 ]; then
                echo "  …… $i/$RUNS 轮通过"
            fi
        else
            fails=$((fails + 1))
            echo "  [FAIL] 第 $i/$RUNS 轮失败(env 竞态复现,锁纪律已回归?)"
        fi
    done
    stop_load
    if [ "$fails" -gt 0 ]; then
        echo "  zhuye-config lib 压测:$RUNS 轮中失败 $fails 轮"
        return 1
    fi
}

tcase "zhuye-config env 竞态压测(连跑 ${RUNS} 轮,全过才算过)" env_lock_stress

tfinish "env_lock_stress.sh" || exit 1
exit 0
