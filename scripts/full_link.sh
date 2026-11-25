#!/usr/bin/env bash
# 全链路集成测试 —— 模拟 LLM(script 回放),贯通引擎完整链路。
#
# 链路:Submission(Op)→ submission_loop → 4 节点 StateGraph →
# 工具执行队列(真实 bash/echo/call_<role>)→ 协议事件流断言 →
# rollout 持久化与回放。唯一被 mock 的是 LLM。
#
# 覆盖矩阵见 crates/runtime/zhuye-core/tests/full_link.rs 头注:
# 生命周期/工具环/用量、多轮×持久化回填、Rewind、真中断+恢复、
# Steer 边界合并、工具输出流式增量、子代理状态查询、子代理端到端
# (进度推送+结果回流+状态中心)、压缩升级。
#
# 用法:./scripts/full_link.sh
set -euo pipefail
cd "$(dirname "$0")/.."

# 引擎进程沙箱:当前用例全用内存 rollout 录制器(不落盘),但任何用例一旦
# 挂上默认 recorder,就会把 mock 会话写进真实 `$HOME/.zhuye/sessions/`,
# 留下污染桌面端会话列表的壳会话(实测 2026-11)。`ZHUYE_HOME` 优先级高于
# `$HOME/.zhuye`(zhuye-config::paths),整脚本统一隔离进一次性目录;本脚本
# 只跑 cargo test,无依赖真实数据目录的断言。
SANDBOX_HOME="$(mktemp -d)"
trap 'rm -rf "$SANDBOX_HOME"' EXIT
export ZHUYE_HOME="$SANDBOX_HOME"

echo "── zhuye 全链路测试(模拟 LLM,离线)"
ZHUYE_MODEL=mock cargo test -p zhuye-core --test full_link -- --test-threads=4

echo "── 配套引擎切片(中断 / 转向 / 会话事件 / 状态查询)"
ZHUYE_MODEL=mock cargo test -p zhuye-core \
    --test interrupt --test steer_mid_turn --test session_events \
    --test subagent_status -- --test-threads=4

echo "[ok] 全链路测试全部通过"
