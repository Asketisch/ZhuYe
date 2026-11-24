"""cli_e2e 场景注册表。

每个场景模块导出若干 `lib.Scenario`,这里汇总成 `SCENARIOS`。新增场景:
写模块 → 在此登记 → 在 cli_e2e.example.toml 的 [scenarios.<name>] 补
带中文注释的参数说明。

场景分组:
- plan   —— plan 模式三条路径(出 plan / 打回修订 / 只读门控);
- goal   —— 目标模式(自校验续作 / 预算熔断 / goal-queue 插件);
- compact—— 上下文压缩(阈值自动 / 手动强制 / 摘要 / 熔断 / 窗口占满);
- cache  —— prompt cache(命中读取 / 写入 / 保温);
- interact —— 交互(中断 / 转向 / 提问 / 审批);
- session—— 会话(续作 / 回退 / 限额 / 工具集);
- ext    —— 扩展(子代理 / 后台任务 / 远程工具 / 管理面 Op)。
"""

from __future__ import annotations

import cli_e2e_lib as lib
from . import sc_cache, sc_compact, sc_ext, sc_goal, sc_interact, sc_plan, sc_session

_ALL = (
    # plan
    sc_plan.PLAN_DEV_COMMIT,
    sc_plan.PLAN_REVISE,
    sc_plan.PLAN_READONLY,
    # compact
    sc_compact.COMPACT_AUTO,
    sc_compact.COMPACT_MANUAL,
    sc_compact.COMPACT_SUMMARIZE,
    sc_compact.COMPACT_REFILL_GUARD,
    sc_compact.COMPACT_WINDOW_FULL,
    # goal
    sc_goal.GOAL_MET,
    sc_goal.GOAL_UNMET,
    sc_goal.GOAL_BUDGET,
    sc_goal.GOAL_PLUGIN,
    # cache
    sc_cache.CACHE_WARMING,
    sc_cache.CACHE_HIT,
    sc_cache.CACHE_WRITE,
    # interact
    sc_interact.INTERRUPT,
    sc_interact.STEER,
    sc_interact.ASK_USER,
    sc_interact.APPROVAL,
    sc_interact.APPROVAL_DENY,
    # session
    sc_session.RESUME,
    sc_session.REWIND,
    sc_session.MAX_ITERATIONS,
    sc_session.TOKEN_BUDGET,
    # ext
    sc_ext.SUBAGENT,
    sc_ext.BACKGROUND,
    sc_ext.REMOTE_TOOL,
    sc_ext.ADMIN_OPS,
)

SCENARIOS: dict[str, lib.Scenario] = {s.name: s for s in _ALL}
