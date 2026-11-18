#!/usr/bin/env python3
"""质量门禁策略判定层 —— 纯函数模块,无副作用、无网络、无模型调用。

设计约束(来自工作区 VERIFICATION_ENHANCEMENT_PLAN.md P0):

- 只采信客观验收结果(pass/fail/infra_error 计数),不采信 LLM judge 自述;
- 比较对象不一致(模型 / 任务集 / 重复次数)或样本不足时判 incomplete,
  enforce 模式下不得判通过 —— 「无法证明未退化」不等价于「通过」;
- observe 模式记录 would_block 与触发原因,不改变退出码语义;
- verdict 优先级:有硬证据退化(regression) > 数据不足(incomplete) > 通过(pass)。

verdict 与退出码的映射由 run.py 决定:

- pass        → 0
- regression  → enforce 返回 1
- incomplete  → enforce 返回 1(阻断假绿)
- infra_error → 2(由 run.py 在调用判定前单独识别,优先于一切 verdict)
"""

from __future__ import annotations

import json
from pathlib import Path

DEFAULT_POLICY_PATH = Path(__file__).with_name("policy.json")

VERDICT_PASS = "pass"
VERDICT_REGRESSION = "regression"
VERDICT_INCOMPLETE = "incomplete"


def load_policy(path: Path | None = None) -> dict:
    """读取策略配置并校验;文件缺失或内容非法直接抛异常,由调用方转 usage 错误。"""
    target = Path(path) if path is not None else DEFAULT_POLICY_PATH
    policy = json.loads(target.read_text(encoding="utf-8"))
    validate_policy(policy)
    return policy


def validate_policy(policy: dict) -> None:
    """校验策略字段类型与取值范围,防止静默使用错误阈值。"""
    required_numbers = {
        "max_pass_rate_drop": (0.0, 1.0),
        "max_mean_elapsed_regression_ratio": (0.0, None),
        "max_tokens_regression_ratio": (0.0, None),
    }
    for name, (low, high) in required_numbers.items():
        value = policy.get(name)
        if not isinstance(value, (int, float)) or isinstance(value, bool):
            raise ValueError(f"策略字段 {name} 必须为数字")
        if value < low or (high is not None and value > high):
            raise ValueError(f"策略字段 {name}={value} 超出允许范围 [{low}, {high}]")
    repetitions = policy.get("min_repetitions")
    if not isinstance(repetitions, int) or isinstance(repetitions, bool) or repetitions < 1:
        raise ValueError("策略字段 min_repetitions 必须为正整数")
    completed = policy.get("min_completed_runs")
    if not isinstance(completed, int) or isinstance(completed, bool) or completed < 1:
        raise ValueError("策略字段 min_completed_runs 必须为正整数")
    if completed > repetitions:
        raise ValueError("min_completed_runs 不能大于 min_repetitions")
    if not isinstance(policy.get("forbid_full_case_regression"), bool):
        raise ValueError("策略字段 forbid_full_case_regression 必须为布尔值")
    if not isinstance(policy.get("version"), int):
        raise ValueError("策略字段 version 必须为整数")


def _case_done(summary: dict) -> int:
    """单用例有效样本数:pass 与 fail 之和(infra_error 不算可比较样本)。"""
    return summary.get("pass", 0) + summary.get("fail", 0)


def _ratio_regression(base: float, candidate: float) -> float | None:
    """相对回退比例;(候选-基线)/基线。基线为 0 时无法计算,返回 None。"""
    if base <= 0:
        return None
    return (candidate - base) / base


def assess(report: dict, policy: dict) -> dict:
    """判定候选版本相对基线是否退化。

    输入为 run.py 的 report(需含 versions.{baseline,candidate}.cases 汇总)与策略字典。
    返回纯数据 verdict 结果,不修改 report,不做任何 I/O。
    """
    violations: list[str] = []
    insufficient: list[str] = []
    notes: list[str] = []

    versions = report.get("versions") or {}
    base_root = versions.get("baseline")
    cand_root = versions.get("candidate")
    if not isinstance(base_root, dict) or not isinstance(cand_root, dict):
        return _verdict(VERDICT_INCOMPLETE, [], [], ["报告缺少 baseline 或 candidate 汇总"])
    base_cases = base_root.get("cases")
    cand_cases = cand_root.get("cases")
    if not isinstance(base_cases, dict) or not isinstance(cand_cases, dict):
        return _verdict(VERDICT_INCOMPLETE, [], [], ["报告缺少逐用例汇总(cases)"])

    repetitions = report.get("repetitions")
    if not isinstance(repetitions, int) or repetitions < policy["min_repetitions"]:
        notes.append(
            f"重复次数 {repetitions} 低于策略要求 {policy['min_repetitions']},统计效力不足")
        insufficient.append("repetitions")

    # 任务集必须一致:一侧缺失用例说明执行不完整,不能对缺失部分判通过。
    base_names, cand_names = set(base_cases), set(cand_cases)
    if not base_names and not cand_names:
        return _verdict(VERDICT_INCOMPLETE, [], [], ["任务集为空,没有任何可比较用例"])
    if base_names != cand_names:
        missing_in_candidate = sorted(base_names - cand_names)
        missing_in_baseline = sorted(cand_names - base_names)
        if missing_in_candidate:
            insufficient.append(f"candidate 缺少用例:{','.join(missing_in_candidate)}")
        if missing_in_baseline:
            insufficient.append(f"baseline 缺少用例:{','.join(missing_in_baseline)}")

    # 逐用例比较:成功率下降、全通过转全失败、时延与 token 预算。
    comparable = sorted(base_names & cand_names)
    base_total_pass = base_total_done = 0
    cand_total_pass = cand_total_done = 0
    for name in comparable:
        base, cand = base_cases[name], cand_cases[name]
        base_done, cand_done = _case_done(base), _case_done(cand)
        base_total_pass += base.get("pass", 0)
        cand_total_pass += cand.get("pass", 0)
        base_total_done += base_done
        cand_total_done += cand_done
        if base_done < policy["min_completed_runs"] or cand_done < policy["min_completed_runs"]:
            insufficient.append(
                f"{name}:有效样本 baseline={base_done} candidate={cand_done}"
                f" < 策略要求 {policy['min_completed_runs']}")
            continue

        base_rate = base.get("pass", 0) / base_done
        cand_rate = cand.get("pass", 0) / cand_done
        # 单用例阈值加统计下限 1/min_completed_runs:重复次数很少时(如 3 次),
        # 任意 1 次翻转就有 0.33 的名义下降,属正常模型噪声,不构成退化证据;
        # 有效样本越多,该下限越小,判定越严格。总体成功率阈值不受此影响。
        case_threshold = max(policy["max_pass_rate_drop"],
                             1.0 / policy["min_completed_runs"])
        if base_rate - cand_rate > case_threshold:
            violations.append(
                f"{name}:成功率 {base_rate:.2f} → {cand_rate:.2f},"
                f"下降超过单用例阈值 {case_threshold:.2f}")

        if policy["forbid_full_case_regression"]:
            all_pass = base.get("pass", 0) == base_done == repetitions
            all_fail = cand.get("pass", 0) == 0 and cand_done == repetitions
            if all_pass and all_fail:
                violations.append(f"{name}:基线全部通过,候选全部失败(整用例回归)")

        for label, key in (("时延", "mean_elapsed_s"), ("token", None)):
            if key is None:
                base_value = base.get("input_tokens", 0) + base.get("output_tokens", 0)
                cand_value = cand.get("input_tokens", 0) + cand.get("output_tokens", 0)
                threshold = policy["max_tokens_regression_ratio"]
            else:
                base_value, cand_value = base.get(key, 0), cand.get(key, 0)
                threshold = policy["max_mean_elapsed_regression_ratio"]
            ratio = _ratio_regression(float(base_value), float(cand_value))
            if ratio is not None and ratio > threshold:
                violations.append(
                    f"{name}:{label}回退 {ratio:.0%},超过预算 {threshold:.0%}"
                    f"({base_value} → {cand_value})")

    # 总体成功率:跨全部可比较用例聚合,比单用例更能反映整体退化。
    if base_total_done > 0 and cand_total_done > 0:
        base_overall = base_total_pass / base_total_done
        cand_overall = cand_total_pass / cand_total_done
        drop = base_overall - cand_overall
        if drop > policy["max_pass_rate_drop"]:
            violations.append(
                f"总体成功率 {base_overall:.2f} → {cand_overall:.2f},"
                f"下降 {drop:.2f} 超过阈值 {policy['max_pass_rate_drop']:.2f}")
    elif comparable:
        notes.append("缺少可聚合的有效样本,总体成功率无法计算")

    if violations:
        # 硬证据退化优先于样本不足:退化本身已是充分证据。
        if insufficient:
            notes.append(f"存在 {len(insufficient)} 项样本不足,但不影响已确认的退化判定")
        return _verdict(VERDICT_REGRESSION, violations, insufficient, notes)
    if insufficient:
        return _verdict(VERDICT_INCOMPLETE, [], insufficient, notes)
    return _verdict(VERDICT_PASS, [], [], notes)


def _verdict(verdict: str, violations: list[str], insufficient: list[str],
             notes: list[str]) -> dict:
    """组装统一的判定结果;would_block 表示 enforce 模式下会阻断。"""
    return {
        "verdict": verdict,
        "would_block": verdict != VERDICT_PASS,
        "violations": violations,
        "insufficient": insufficient,
        "notes": notes,
    }
