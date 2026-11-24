"""compact 组场景:上下文压缩的机制回归。

引擎事实(与 zhuye-compact / zhuye-core 对齐,改动需同步改本文件):

- 触发阈值 = 模型上下文窗口 × `[compact].trigger_ratio`(默认 0.85);
  比较量 = max(最近一次 LLM 上报 input_tokens, 本地估算);
- 本地估算器:exec bootstrap 注册了 tiktoken(cl100k)全局估算器 ——
  英文 ≈ 4.16 字符/token(实测),中文 ≈ 1 字符/token 量级。本模块的
  夹具一律用 ASCII 填充,以便按字符数反推 token 数;
- 压缩管线:microcompact(消息数 > keep_recent(30) 时清掉旧工具结果)→
  smart_prune(工具结果截断到 300 字符 / 删旧工具对,目标 trigger×0.75)
  → LLM 摘要(前两阶段都压不下来且估算 ≥ trigger 时);
- 判定门槛分两层:**管线门槛**用 `before_tokens`(force 时 = u32::MAX,
  否则 = max(上报, 本地估算)),**裁剪门槛**用本地估算 —— 所以
  `Op::Compact` 的 force 语义在 mock 下只体现为「阈值被抬到最大」,
  真正裁不裁得动仍取决于本地估算是否够大;
- rapid-refill 熔断:压缩实际发生后 ≤3 步内再次越阈 = refill,第 1/2 次
  跳过压缩并注入收敛提醒(若同时 ≥ 窗口 95% 则直接以
  `compact_rapid_refill_window_full` 收口),连续第 3 次以
  `compact_rapid_refill` 终止当前 turn(sess_77cd626f 事故的收口路径);
- 上下文已达窗口且没有可压内容 → `compact_unavailable_window_full`;
- `Op::Compact` 手动压缩:受理即回一条 Noop 回执,真正压缩发生在下一
  回合的 pre_loop(force 路径绕过阈值与 rapid-refill 守卫)。

mock 下 LLM 上报恒为 0,真实模型上报会顶穿小窗口,故本组场景标记
mock_only —— 仅在自检模式运行。
"""

from __future__ import annotations

import json

import cli_e2e_lib as lib

REFILL_CODES = {"compact_rapid_refill", "compact_rapid_refill_window_full"}
# force 路径把 before_tokens 抬到 u32::MAX(「无视阈值」的实现标志)。
FORCE_BEFORE_TOKENS = 4294967295
# tiktoken(cl100k)对英文的实测字符/token。
CHARS_PER_TOKEN = 4.16


def _read(path: str) -> dict:
    return {"type": "tool_call", "name": "read", "args": {"path": path}}


def _text(value: str) -> dict:
    return {"type": "text", "text": value}


def _ascii(chunk: int) -> str:
    """生成 `chunk` 个字符的 ASCII 填充(内容不重复,便于 tiktoken 稳定计)。"""
    unit = "abcdefghij"
    return (unit * (chunk // len(unit) + 1))[:chunk]


def _data_file(rel: str, chars: int):
    """生成一个 `rel` 夹具写入函数(ASCII 填充,单行)。"""

    def setup(ws: str) -> None:
        with open(f"{ws}/{rel}", "w", encoding="utf-8") as f:
            f.write(_ascii(chars))

    return setup


def _strategies(comp: list[dict]) -> list[str]:
    return [str(c.get("strategy")) for c in comp]


# ── compact_auto:阈值触发的自动压缩 ────────────────────────────────────────


def _run_compact_auto(cx: lib.Ctx) -> None:
    window = int(cx.p("window", 4500))
    rounds = int(cx.p("rounds", 30))
    file_chars = int(cx.p("file_chars", 420))
    env = cx.env(
        model_windows={"mock/mock-1": window},
        setup=_data_file("data.txt", file_chars),
    )
    s = cx.session(env)

    turn = s.turn("逐段读取 data.txt 并汇报进度。", timeout=cx.timeout())
    comp = turn.compactions
    cx.checks.add(
        "自动压缩发生(strategy ≠ noop)",
        bool(comp),
        json.dumps([{"s": c.get("strategy"), "rm": c.get("removed_messages"),
                     "before": c.get("before_tokens"), "after": c.get("after_tokens")}
                    for c in comp], ensure_ascii=False) if comp else "无压缩事件",
    )
    cx.checks.add(
        "压缩确实减少了消息/字节",
        any(c.get("removed_messages", 0) > 0
            or (c.get("after_tokens") or 0) < (c.get("before_tokens") or 0)
            for c in comp),
        f"策略={_strategies(comp)}",
    )
    codes = sorted(set(turn.error_codes) & (lib.FATAL_CODES | REFILL_CODES))
    cx.checks.add("未触发熔断/收口错误", not codes, f"错误码={codes}" if codes else "无")
    cx.checks.add("回合正常收尾", turn.completed, f"终态={turn.status or turn.aborted_reason}")
    cx.metrics.update(compactions=len(comp))


def _mock_compact_auto(params: dict) -> list[dict]:
    rounds = int(params.get("rounds", 30))
    return [_read("data.txt") for _ in range(rounds)] + [_text("读取完毕。")]


COMPACT_AUTO = lib.Scenario(
    "compact_auto",
    "阈值触发的自动压缩(microcompact/smart_prune 落地)",
    _run_compact_auto,
    mock_only=True,
    effort="low",
    defaults={"window": 4500, "rounds": 30, "file_chars": 420},
    mock_script=_mock_compact_auto,
)


# ── compact_manual:Op::Compact 强制路径 ────────────────────────────────────


def _run_compact_manual(cx: lib.Ctx) -> None:
    """手动强制压缩:force 路径**无视阈值、也绕过 rapid-refill 守卫**。

    断言口径刻意避开「上下文恰好落在阈值附近的窄带」—— 那条带的
    位置随并行工具执行时序漂移,不可做确定性断言。改为在**守卫已上膛**
    的状态(压缩后又立刻填满)验证 force 的语义:

      回合 1 —— 上下文撑到触发阈值之上 → 压缩 → 守卫连击 → 回合被熔断;
      回合 2 —— Op::Compact 强制压缩:守卫被 force 路径绕过,压缩照常
                 执行,before_tokens 被抬到 u32::MAX(无视阈值)。

    守卫是**会话级**的:熔断后仍处上膛状态,所以回合 2 之后仍可能
    再次被熔断 —— 本场景只断言「强制压缩确实执行了」。
    """
    window = int(cx.p("window", 8000))
    rounds = int(cx.p("rounds", 120))
    user_chars = int(cx.p("user_chars", 20800))
    file_chars = int(cx.p("file_chars", 420))
    env = cx.env(model_windows={"mock/mock-1": window},
                 setup=_data_file("data.txt", file_chars))
    s = cx.session(env)

    # 回合 1:把上下文顶到触发阈值之上(首条用户消息是「固定保留」消息,
    # 压不动 —— 只能靠 tool 结果堆积)。守卫随后上膛并熔断本回合。
    head = "background: " + _ascii(user_chars)
    first = s.turn(head + "\n持续读取 data.txt 直到收尾。", timeout=cx.timeout())
    cx.checks.add(
        "回合 1 触发过压缩(守卫上膛)",
        bool(first.compactions),
        f"压缩策略={_strategies(first.compactions)}",
    )
    cx.checks.add(
        "回合 1 被熔断收口(守卫已上膛)",
        bool(set(first.error_codes) & REFILL_CODES),
        f"错误码={sorted(set(first.error_codes))}",
    )

    # Op::Compact 受理:立即回一条 Noop 回执(不等回合)。
    s.op({"type": "compact"})
    receipt = s.wait(
        lambda e: lib.etype(e) == "context_compacted"
        and (lib._msg(e).get("strategy") or "") == "noop",
        30, what="compact Noop 回执",
    )
    cx.checks.add("Op::Compact 立即回 Noop 回执", receipt is not None,
                  "收到 context_compacted(strategy=noop)")

    # 回合 2:pre_loop 以 force 语义执行压缩 —— 阈值抬到 u32::MAX,
    # 且**绕过 rapid-refill 守卫**(force 路径不进 pre_check)。
    turn = s.turn("继续。", timeout=cx.timeout())
    events = turn._msgs("context_compacted")
    forced = [c for c in events if (c.get("before_tokens") or 0) == FORCE_BEFORE_TOKENS]
    cx.checks.add(
        "守卫已上膛时强制压缩仍然执行(before_tokens=u32::MAX)",
        bool(forced),
        f"压缩事件={[(c.get('strategy'), c.get('before_tokens')) for c in events][:4]}",
    )
    cx.checks.add(
        "强制压缩实际裁剪了内容(策略 ≠ noop)",
        any(c.get("strategy") not in (None, "noop") for c in events),
        f"策略={[c.get('strategy') for c in events][:6]}",
    )


def _mock_compact_manual(params: dict) -> list[dict]:
    rounds = int(params.get("rounds", 40))
    return ([_read("data.txt") for _ in range(rounds)]
            + [_text("读取完毕。"), _text("好的。")])


COMPACT_MANUAL = lib.Scenario(
    "compact_manual",
    "Op::Compact 强制压缩:受理回执 + 下一回合无视阈值执行压缩",
    _run_compact_manual,
    mock_only=True,
    effort="low",
    defaults={"window": 8000, "rounds": 120, "user_chars": 20800, "file_chars": 420},
    mock_script=_mock_compact_manual,
)


# ── compact_summarize:单条超大消息走 LLM 摘要 ─────────────────────────────


def _run_compact_summarize(cx: lib.Ctx) -> None:
    window = int(cx.p("window", 4500))
    big_chars = int(cx.p("big_chars", 17000))
    env = cx.env(model_windows={"mock/mock-1": window})
    s = cx.session(env)
    big = "history: " + _ascii(big_chars)
    turn = s.turn(big + "\n读到后请说 OK。", timeout=cx.timeout())
    comp = turn.compactions
    cx.checks.add(
        "超大上下文走 LLM 摘要压缩(strategy=ll_m_summarize)",
        any(c.get("strategy") == "ll_m_summarize" for c in comp),
        json.dumps(_strategies(comp), ensure_ascii=False),
    )
    codes = sorted(set(turn.error_codes) & (lib.FATAL_CODES | REFILL_CODES))
    cx.checks.add("未触发熔断/收口错误", not codes, f"错误码={codes}" if codes else "无")
    cx.checks.add("回合正常收尾", turn.completed, f"终态={turn.status or turn.aborted_reason}")
    cx.metrics.update(compactions=len(comp))


def _mock_compact_summarize(params: dict) -> list[dict]:
    # 第 1 行被摘要器消耗(pre_loop 的摘要调用先于本回合的模型调用);
    # 摘要器要求回复带 <summary> 标签,否则报 NoSummaryTag 并降级为
    # 「best-effort smart_prune」(空压 → Noop)。
    return [
        _text("<summary>用户给出了一大段历史资料,已按要点归并。</summary>"),
        _text("OK。"),
    ]


COMPACT_SUMMARIZE = lib.Scenario(
    "compact_summarize",
    "单条超大历史走 LLM 摘要压缩",
    _run_compact_summarize,
    mock_only=True,
    effort="low",
    defaults={"window": 4500, "big_chars": 17000},
    mock_script=_mock_compact_summarize,
)


# ── compact_refill_guard:rapid-refill 熔断回归(sess_77cd626f)─────────────


def _run_compact_refill_guard(cx: lib.Ctx) -> None:
    window = int(cx.p("window", 4500))
    file_chars = int(cx.p("file_chars", 420))
    user_chars = int(cx.p("user_chars", 9800))
    env = cx.env(model_windows={"mock/mock-1": window},
                 setup=_data_file("small.txt", file_chars))
    s = cx.session(env)

    # 首条用户消息很大(固定保留,压不动)→ 上下文长期停在「触发阈值之上」,
    # 压缩只能清掉旧工具结果、无法把估算压回阈值以下 —— 压缩后立刻又被
    # 填满,正是 sess_77cd626f 的 rapid refill 形态。
    head = "background: " + _ascii(user_chars)
    turn = s.turn(head + "\n持续读取 small.txt 直到收尾。", timeout=cx.timeout())
    fired = sorted(set(turn.error_codes) & REFILL_CODES)
    cx.checks.add(
        "rapid-refill 熔断收口(compact_rapid_refill*)",
        bool(fired),
        f"错误码={fired}" if fired else f"error_codes={sorted(set(turn.error_codes))}",
    )
    cx.checks.add(
        "熔断以 turn_aborted 收口(非静默死循环)",
        bool(turn.aborted_reason),
        f"aborted={turn.aborted_reason}",
    )
    msg = json.dumps(turn.aborted_reason or {}, ensure_ascii=False)
    cx.checks.add(
        "熔断文案给出可操作建议(开启新会话 / 拆小任务)",
        ("新会话" in msg) or ("拆小" in msg),
        msg[:160],
    )
    cx.metrics.update(compactions=len(turn.compactions))


def _mock_compact_refill_guard(params: dict) -> list[dict]:
    rounds = int(params.get("rounds", 40))
    return ([_read("small.txt") for _ in range(rounds)]
            + [_text("读取完毕。"), _text("好。")])


COMPACT_REFILL_GUARD = lib.Scenario(
    "compact_refill_guard",
    "rapid-refill 熔断:压缩后又秒填满 → 熔断收口而非死循环",
    _run_compact_refill_guard,
    mock_only=True,
    effort="low",
    defaults={"window": 4500, "file_chars": 420, "user_chars": 9800, "rounds": 40},
    mock_script=_mock_compact_refill_guard,
)


# ── compact_window_full:窗口已满且压不动 ──────────────────────────────────


def _run_compact_window_full(cx: lib.Ctx) -> None:
    window = int(cx.p("window", 4500))
    env = cx.env(model_windows={"mock/mock-1": window})
    s = cx.session(env)
    huge = "history: " + _ascii(int(cx.p("huge_chars", 20000)))
    turn = s.turn(huge, timeout=cx.timeout())
    codes = sorted(set(turn.error_codes))
    fired = "compact_unavailable_window_full" in codes
    cx.checks.add(
        "窗口已满收口(compact_unavailable_window_full)",
        fired,
        f"错误码={codes},压缩={_strategies(turn.compactions)}",
    )
    bad = sorted(set(turn.error_codes) & {"compact_rapid_refill"})
    cx.checks.add("未误入 rapid-refill 死循环", not bad, f"错误码={bad}" if bad else "无")


def _mock_compact_window_full(params: dict) -> list[dict]:
    return [_text("好的。")]


COMPACT_WINDOW_FULL = lib.Scenario(
    "compact_window_full",
    "上下文占满单条不可压消息:收口码,绝不循环",
    _run_compact_window_full,
    mock_only=True,
    effort="low",
    defaults={"window": 4500, "huge_chars": 20000},
    mock_script=_mock_compact_window_full,
)
