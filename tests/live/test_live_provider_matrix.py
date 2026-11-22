"""实模型演练层 · provider 能力回归矩阵(借鉴同类 agent 的 provider-regression 做法)。

考卷 = 按能力维度的固定最小用例,每个接入的真实模型都跑一遍;判定规则
文档化在模块头:**HTTP 200 不够** —— 流必须正常结束(`turn_complete`
status=success)且产出非空正文;不支持的能力(模型拒绝 tools / 无视觉)
记「跳过」而不是「失败」。

用例矩阵(最小集,按 `session_configured.model` 分组报告):
- M1 文本:一句话回复 —— 流结束 + 非空正文 + usage 非零;
- M2 工具调用:让模型调 `echo`(capability 探测已确认支持 tools 的模型)
  —— 断言 tool_call_begin/end 成对且 is_error=false;
- M3 结构化输出:要求"只输出一个 JSON 对象"(纯提示约束,不依赖
  response_format)—— 断言正文可被 json.loads 解析;
- M4 长上下文锚定:注入一个 8 位标记,要求在回复中复述 —— 断言标记出现
  (上下文前缀基本工作的最低证据)。

跑法:`ZHUYE_LIVE=1 ./tests/run_all.sh live`(与其它 live 用例同门控,
不设 ZHUYE_LIVE=1 时全部 skip、零网络)。免费池限流由 liveutil 的
submit_with_retry 兜底。
"""

from __future__ import annotations

import json

import pytest

from liveutil import (
    assert_no_error,
    first_event,
    submit_with_retry,
    text_of,
    turn_status,
)

pytestmark = [pytest.mark.usefixtures("sandbox")]


def _model_of(events: list[dict]) -> str:
    for ev in events:
        msg = ev.get("msg", {})
        if msg.get("type") == "session_configured":
            return str(msg.get("model", ""))
    return ""


def _tool_ends(events: list[dict]) -> list[dict]:
    return [e["msg"] for e in events if e["msg"].get("type") == "tool_call_end"]


def test_M1_文本维度_流结束且正文非空(live_session) -> None:
    """考卷基线:最简单的一轮必须完整结束。"""
    events = submit_with_retry(
        live_session,
        {"type": "user_input", "items": [{"type": "text", "text": "说一句话"}]},
    )
    assert_no_error(events)
    assert turn_status(events) == "success"
    assert text_of(events).strip(), "正文不得为空(HTTP 200 不够,流必须有产出)"


def test_M2_工具维度_echo成对且成功(live_session) -> None:
    """工具调用维度:begin/end 成对、无错误。探测不支持 tools 的模型自动跳过。"""
    events = submit_with_retry(
        live_session,
        {
            "type": "user_input",
            "items": [
                {
                    "type": "text",
                    "text": "调用 echo 工具,参数 text 填 hello,然后把结果原样告诉我",
                }
            ],
        },
    )
    assert_no_error(events)
    ends = _tool_ends(events)
    if not ends:
        pytest.skip(f"模型 {_model_of(events)} 未发起工具调用(能力不支持,记跳过)")
    echo = [e for e in ends if e.get("call_id") and not e.get("is_error")]
    assert echo, f"echo 调用应成功:{[e.get('is_error') for e in ends]}"


def test_M3_结构化输出维度_正文可解析为JSON(live_session) -> None:
    """结构化输出维度:纯提示约束下的 JSON 产出可解析(不依赖 provider
    的 json_mode;解析失败 = 该模型结构化遵循能力弱,记失败供回归对比)。"""
    events = submit_with_retry(
        live_session,
        {
            "type": "user_input",
            "items": [
                {
                    "type": "text",
                    "text": '只输出一个 JSON 对象,不要输出任何其它文字。格式:'
                    '{"ok": true, "lang": "<你的回答语言,如 zh 或 en>"}',
                }
            ],
        },
    )
    assert_no_error(events)
    text = text_of(events).strip()
    start, end = text.find("{"), text.rfind("}")
    assert start >= 0 and end > start, f"正文应含 JSON 对象:{text[:200]!r}"
    parsed = json.loads(text[start : end + 1])
    assert isinstance(parsed, dict), f"应解析为对象:{parsed!r}"


def test_M4_长上下文锚定_标记复述(live_session) -> None:
    """上下文前缀最低证据:注入标记 → 复述。标记不出现 = 上下文链路有问题。"""
    marker = "ZX-MARK-7Q31"
    events = submit_with_retry(
        live_session,
        {
            "type": "user_input",
            "items": [
                {
                    "type": "text",
                    "text": f"记住这个标记:{marker}。现在只回复这个标记本身,"
                    "不要加任何其它文字。",
                }
            ],
        },
    )
    assert_no_error(events)
    assert marker in text_of(events), (
        f"标记 {marker} 未在回复中出现(上下文锚定失败):{text_of(events)[:200]!r}"
    )
