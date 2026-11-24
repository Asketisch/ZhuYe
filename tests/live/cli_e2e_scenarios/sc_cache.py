"""cache 组场景:prompt cache 的命中与保温。

引擎事实(与 zhuye-llm / zhuye-core 对齐,改动需同步改本文件):

- 用量口径:`TokenCount.input_tokens` 含 `cached_tokens` /
  `cache_write_tokens` 两个**子片段**(Anthropic 线缆的 input 不含缓存
  分段,provider 适配层上报前折入 `fold_cache_segments`);
- OpenAI 线缆的缓存命中来自 `usage.prompt_tokens_details.cached_tokens`
  —— 由服务端的前缀缓存(vLLM / OpenAI 自动前缀缓存)产生,引擎侧
  不主动插锚点;Anthropic 线缆由引擎插 cache_control 断点,
  `cache_creation_input_tokens` → `cache_write_tokens`;
- 保温(`[cache_warming]`):回合结束后进入空闲期,按 TTL 的 90% 提前
  发一次「合成重放」刷新前缀 TTL,状态经 `EventMsg::CacheWarming`
  可见:`scheduled`(空闲期排程建立)/`executed`(重放完成,带
  warm_cached_tokens)/`skipped`(带 reason)/`cancelled`;
- `force = true` 跳过定价 / 命中观测 / 规模门控(仅测试用),但
  `enabled = false` 时保温恒不生效;
- 保温重放**不消耗 mock 脚本行**(mock 对 warm_replay 标记走合成流),
  故脚本行序不受影响。

`cache_hit` 依赖真实 provider 上报缓存分段(mock 恒为 0),标记
needs_model —— 仅在真模型模式运行。
"""

from __future__ import annotations

import json
import time

import cli_e2e_lib as lib


def _text(value: str) -> dict:
    return {"type": "text", "text": value}


# ── cache_warming:空闲期保温(可离线自检)──────────────────────────────────


CACHE_WARMING_TOML = """
[cache_warming]
# 测试开关:强制保温(跳过定价 / 命中观测 / 规模门控)。
enabled = true
force = true
# 把 TTL 压到 4 秒 —— 默认 1h 的话要等 54 分钟才刷新,测试没法等。
# 下限:refresh_delay 要求 ttl > margin + 1(margin 至少 1 秒)。
ttl_secs = 4
refresh_margin_secs = 0
max_idle_window_secs = 120
"""


def _run_cache_warming(cx: lib.Ctx) -> None:
    env = cx.env(extra_toml=CACHE_WARMING_TOML)
    s = cx.session(env)

    turn = s.turn("请记住:本回合用于建立缓存候选前缀。", timeout=cx.timeout())
    cx.checks.add("回合正常收尾(建立缓存候选)", turn.completed,
                  f"终态={turn.status or turn.aborted_reason}")

    # 回合结束后进入空闲期:先发 scheduled,到刷新点后发 executed。
    deadline = time.monotonic() + 60.0
    events: list[dict] = []
    while time.monotonic() < deadline:
        events = [m for m in _all_cache_warming(s)]
        actions = {str(m.get("action")) for m in events}
        if {"scheduled", "executed"} <= actions:
            break
        time.sleep(0.3)

    actions = [str(m.get("action")) for m in events]
    phases = [str(m.get("phase")) for m in events]
    cx.checks.add("收到保温事件(scheduled + executed)",
                  {"scheduled", "executed"} <= set(actions),
                  f"动作序列={actions},阶段={phases}")
    executed = [m for m in events if str(m.get("action")) == "executed"]
    cx.checks.add("executed 事件带保温用量字段",
                  bool(executed) and executed[0].get("warm_cached_tokens") is not None,
                  f"executed={executed[:1]}")
    cx.checks.add("保温发生在空闲阶段(phase=idle)",
                  any(str(m.get("phase")) == "idle" for m in events),
                  f"阶段={phases}")


def _all_cache_warming(s: lib.Session) -> list[dict]:
    out = []
    for evt in s.events:
        m = lib._msg(evt)
        if m.get("type") == "cache_warming":
            out.append(m)
    return out


def _mock_cache_warming(params: dict) -> list[dict]:
    return [_text("已记住。")]


CACHE_WARMING = lib.Scenario(
    "cache_warming",
    "空闲期 prompt cache 保温:scheduled → executed 重放",
    _run_cache_warming,
    effort="low",
    defaults={},
    mock_script=_mock_cache_warming,
)


# ── cache_hit:真实 provider 的缓存命中(需真模型)──────────────────────────


def _run_cache_hit(cx: lib.Ctx) -> None:
    env = cx.env()
    s = cx.session(env)

    # 同一前缀连跑多轮:真 provider 的前缀缓存应在第 2 轮起命中。
    turns = []
    for i in range(int(cx.p("rounds", 3))):
        turns.append(s.turn(
            f"第 {i + 1} 轮:请复述本会话开头的要求(保持前缀稳定)。",
            timeout=cx.timeout(),
        ))

    counts = [tc for t in turns for tc in t.token_counts]
    cached = [tc for tc in counts if (tc.get("cached_tokens") or 0) > 0]
    cx.checks.add(
        "出现缓存命中(cached_tokens > 0)",
        bool(cached),
        f"调用数={len(counts)},命中数={len(cached)}",
    )
    bad = [tc for tc in counts
           if (tc.get("cached_tokens") or 0) > (tc.get("input_tokens") or 0)]
    cx.checks.add("cached ⊆ input(子片段口径未被破坏)", not bad, f"越界={bad[:3]}")
    cx.checks.add(
        "全程无结构性错误",
        not [c for t in turns for c in t.error_codes if c in lib.FATAL_CODES],
        "",
    )


CACHE_HIT = lib.Scenario(
    "cache_hit",
    "真模型前缀缓存命中:第 2 轮起 cached_tokens > 0",
    _run_cache_hit,
    needs_model=True,
    effort="low",
    defaults={"rounds": 3},
)


# ── cache_write:缓存写入分段(Anthropic 线缆才有)──────────────────────────


def _run_cache_write(cx: lib.Ctx) -> None:
    env = cx.env()
    s = cx.session(env)
    turn = s.turn("请记住本轮内容。", timeout=cx.timeout())
    counts = turn.token_counts
    writes = [tc for tc in counts if (tc.get("cache_write_tokens") or 0) > 0]
    endpoint = (cx.cfg.get("endpoint") or {}).get("endpoint")
    if str(endpoint) == "anthropic":
        cx.checks.add("Anthropic 线缆出现缓存写入(cache_write_tokens > 0)",
                      bool(writes), f"写入段={writes[:3]}")
    else:
        cx.checks.skip("Anthropic 线缆出现缓存写入(cache_write_tokens > 0)",
                       f"当前线格式={endpoint}(非 anthropic 时 provider 不报写入分段)")
    cx.checks.add("回合正常收尾", turn.completed,
                  f"终态={turn.status or turn.aborted_reason}")


CACHE_WRITE = lib.Scenario(
    "cache_write",
    "缓存写入分段(cache_write_tokens)的上报与口径",
    _run_cache_write,
    needs_model=True,
    effort="low",
    defaults={},
)


def _mock_cache_write(params: dict) -> list[dict]:
    return [_text("好的。")]


CACHE_WRITE.mock_script = _mock_cache_write
