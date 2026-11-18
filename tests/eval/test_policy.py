#!/usr/bin/env python3
"""质量门禁策略判定器自测 —— 全部使用合成报告,不调用真实模型、不消耗额度。

覆盖口径(对应 VERIFICATION_ENHANCEMENT_PLAN.md P0 验收):

- 改善 / 不变 / 允许范围内波动 → pass;
- 成功率越阈、单用例全通过转全失败、时延与 token 越预算 → regression;
- 样本不足、任务集不一致、重复次数不够、报告缺块 → incomplete;
- 有硬证据退化时,regression 优先于 incomplete;
- enforce 退出码映射:pass→0,regression/incomplete→1;
  observe 恒 0(仅记录 would_block);infra_error→2 由 run.py 侧处理。

运行方式:`python3 tests/eval/test_policy.py`(仅依赖标准库)。
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import eval_policy  # noqa: E402


def make_case(pass_n: int, fail_n: int = 0, infra_n: int = 0, mean_elapsed: float = 10.0,
              input_tokens: int = 1000, output_tokens: int = 500) -> dict:
    """构造单个用例的汇总条目,字段与 run.py summarize() 输出对齐。"""
    return {"pass": pass_n, "fail": fail_n, "infra_error": infra_n,
            "mean_elapsed_s": mean_elapsed,
            "input_tokens": input_tokens * (pass_n + fail_n + infra_n),
            "output_tokens": output_tokens * (pass_n + fail_n + infra_n),
            "tool_calls": 3 * (pass_n + fail_n + infra_n)}


def make_report(base: dict, cand: dict, repetitions: int = 3) -> dict:
    """构造与 run.py report 相同结构的最小报告。"""
    return {
        "model": "test-model",
        "repetitions": repetitions,
        "case_sha256": "fake",
        "versions": {
            "baseline": {"cases": base},
            "candidate": {"cases": cand},
        },
    }


def default_policy() -> dict:
    """与 policy.json 同口径的测试策略,避免测试依赖磁盘文件内容漂移。"""
    return {
        "version": 1,
        "min_repetitions": 3,
        "min_completed_runs": 2,
        "max_pass_rate_drop": 0.15,
        "forbid_full_case_regression": True,
        "max_mean_elapsed_regression_ratio": 0.5,
        "max_tokens_regression_ratio": 0.5,
    }


class PolicyValidationTest(unittest.TestCase):
    """策略配置校验:非法配置必须显式失败,不允许静默使用错误阈值。"""

    def test_default_policy_file_valid(self):
        # 仓库自带 policy.json 必须始终可加载,否则 run_quality.sh 无法启动。
        policy = eval_policy.load_policy()
        self.assertEqual(policy["version"], 1)

    def test_negative_threshold_rejected(self):
        policy = default_policy()
        policy["max_pass_rate_drop"] = -0.1
        with self.assertRaises(ValueError):
            eval_policy.validate_policy(policy)

    def test_pass_rate_drop_above_one_rejected(self):
        policy = default_policy()
        policy["max_pass_rate_drop"] = 1.5
        with self.assertRaises(ValueError):
            eval_policy.validate_policy(policy)

    def test_completed_runs_above_repetitions_rejected(self):
        policy = default_policy()
        policy["min_completed_runs"] = 5
        with self.assertRaises(ValueError):
            eval_policy.validate_policy(policy)

    def test_non_bool_flag_rejected(self):
        policy = default_policy()
        policy["forbid_full_case_regression"] = "yes"
        with self.assertRaises(ValueError):
            eval_policy.validate_policy(policy)


class AssessPassTest(unittest.TestCase):
    """正常与允许波动场景必须判 pass,不得误报。"""

    def test_identical_results_pass(self):
        report = make_report({"case1": make_case(3)}, {"case1": make_case(3)})
        result = eval_policy.assess(report, default_policy())
        self.assertEqual(result["verdict"], "pass")
        self.assertFalse(result["would_block"])

    def test_improvement_pass(self):
        report = make_report({"case1": make_case(2, fail_n=1)},
                             {"case1": make_case(3)})
        result = eval_policy.assess(report, default_policy())
        self.assertEqual(result["verdict"], "pass")

    def test_small_fluctuation_within_threshold_pass(self):
        # 单用例 3 次里掉 1 次:名义下降 0.33,但单用例阈值含统计下限
        # 1/min_completed_runs = 0.5,属正常噪声不判退化;
        # 总体成功率 1.0 → 35/36(约 0.028 下降)也远低于 0.15 → pass。
        base = {f"case{i}": make_case(3) for i in range(12)}
        cand = {f"case{i}": make_case(3) for i in range(12)}
        cand["case0"] = make_case(2, fail_n=1)
        report = make_report(base, cand)
        result = eval_policy.assess(report, default_policy())
        self.assertEqual(result["verdict"], "pass")

    def test_elapsed_within_budget_pass(self):
        # 10s → 14s:回退 40%,在 50% 预算内。
        report = make_report({"case1": make_case(3, mean_elapsed=10.0)},
                             {"case1": make_case(3, mean_elapsed=14.0)})
        result = eval_policy.assess(report, default_policy())
        self.assertEqual(result["verdict"], "pass")


class AssessRegressionTest(unittest.TestCase):
    """硬证据退化必须被判 regression 并给出可定位的 violation。"""

    def test_single_case_pass_rate_drop(self):
        # 单用例 3/3 → 1/3:下降 0.67,远超 0.15。
        report = make_report({"case1": make_case(3)}, {"case1": make_case(1, fail_n=2)})
        result = eval_policy.assess(report, default_policy())
        self.assertEqual(result["verdict"], "regression")
        self.assertTrue(result["would_block"])
        self.assertTrue(any("case1" in v and "成功率" in v for v in result["violations"]))

    def test_overall_pass_rate_drop(self):
        # 12 用例中 6 个各掉 1 次:总体 1.0 → 30/36 ≈ 0.833,下降 0.167 > 0.15;
        # 单用例每次翻转 0.33 < 统计下限 0.5,不触发单用例 violation。
        base = {f"case{i}": make_case(3) for i in range(12)}
        cand = {f"case{i}": make_case(3) for i in range(12)}
        for i in range(6):
            cand[f"case{i}"] = make_case(2, fail_n=1)
        report = make_report(base, cand)
        result = eval_policy.assess(report, default_policy())
        self.assertEqual(result["verdict"], "regression")
        self.assertTrue(any("总体成功率" in v for v in result["violations"]))

    def test_full_case_regression(self):
        # 基线全通过 → 候选全失败:即使总体下降只来自一个用例也必须阻断。
        base = {f"case{i}": make_case(3) for i in range(12)}
        cand = {f"case{i}": make_case(3) for i in range(12)}
        cand["case0"] = make_case(0, fail_n=3)
        report = make_report(base, cand)
        result = eval_policy.assess(report, default_policy())
        self.assertEqual(result["verdict"], "regression")
        self.assertTrue(any("整用例回归" in v for v in result["violations"]))

    def test_full_case_regression_can_be_disabled(self):
        policy = default_policy()
        policy["forbid_full_case_regression"] = False
        base = {f"case{i}": make_case(3) for i in range(12)}
        cand = {f"case{i}": make_case(3) for i in range(12)}
        cand["case0"] = make_case(0, fail_n=3)
        # 关闭整用例回归检查后,总体下降 0.25 仍超过 0.15 → 仍判 regression,
        # 但 violation 不再含"整用例回归"字样。
        report = make_report(base, cand)
        result = eval_policy.assess(report, policy)
        self.assertEqual(result["verdict"], "regression")
        self.assertFalse(any("整用例回归" in v for v in result["violations"]))

    def test_elapsed_over_budget(self):
        # 10s → 16s:回退 60% > 50% 预算。
        report = make_report({"case1": make_case(3, mean_elapsed=10.0)},
                             {"case1": make_case(3, mean_elapsed=16.0)})
        result = eval_policy.assess(report, default_policy())
        self.assertEqual(result["verdict"], "regression")
        self.assertTrue(any("时延回退" in v for v in result["violations"]))

    def test_tokens_over_budget(self):
        # 输入+输出 4500 → 9000:回退 100% > 50% 预算。
        report = make_report({"case1": make_case(3, input_tokens=1000, output_tokens=500)},
                             {"case1": make_case(3, input_tokens=2000, output_tokens=1000)})
        result = eval_policy.assess(report, default_policy())
        self.assertEqual(result["verdict"], "regression")
        self.assertTrue(any("token回退" in v for v in result["violations"]))

    def test_regression_takes_precedence_over_insufficient(self):
        # 一个用例退化、另一个用例样本不足:退化证据充分,必须判 regression。
        report = make_report(
            {"case1": make_case(3), "case2": make_case(3)},
            {"case1": make_case(0, fail_n=3), "case2": make_case(1, infra_n=2)})
        result = eval_policy.assess(report, default_policy())
        self.assertEqual(result["verdict"], "regression")
        self.assertTrue(result["insufficient"])  # 不足项记录为 note,不掩盖退化


class AssessIncompleteTest(unittest.TestCase):
    """数据不足 / 比较对象不一致必须判 incomplete,enforce 下阻断假绿。"""

    def test_missing_versions_incomplete(self):
        report = {"versions": {"baseline": {"cases": {}}}}
        result = eval_policy.assess(report, default_policy())
        self.assertEqual(result["verdict"], "incomplete")
        self.assertTrue(result["would_block"])

    def test_missing_cases_incomplete(self):
        report = make_report({}, {})
        result = eval_policy.assess(report, default_policy())
        self.assertEqual(result["verdict"], "incomplete")

    def test_task_set_mismatch_incomplete(self):
        report = make_report({"case1": make_case(3), "case2": make_case(3)},
                             {"case1": make_case(3)})
        result = eval_policy.assess(report, default_policy())
        self.assertEqual(result["verdict"], "incomplete")
        self.assertTrue(any("candidate 缺少用例:case2" in s for s in result["insufficient"]))

    def test_insufficient_samples_incomplete(self):
        # 双侧各只有 1 个有效样本(其余 infra_error):低于 min_completed_runs=2。
        report = make_report({"case1": make_case(1, infra_n=2)},
                             {"case1": make_case(1, infra_n=2)})
        result = eval_policy.assess(report, default_policy())
        self.assertEqual(result["verdict"], "incomplete")
        self.assertTrue(any("有效样本" in s for s in result["insufficient"]))

    def test_repetitions_below_policy_incomplete(self):
        # 报告只跑了 1 次,策略要求至少 3 次:统计效力不足。
        report = make_report({"case1": make_case(1)}, {"case1": make_case(1)}, repetitions=1)
        result = eval_policy.assess(report, default_policy())
        self.assertEqual(result["verdict"], "incomplete")
        self.assertTrue(any("重复次数" in n for n in result["notes"]))

    def test_all_infra_error_incomplete(self):
        # 双侧全部 infra_error:无有效样本,不得判 pass 也不得判 regression。
        report = make_report({"case1": make_case(0, infra_n=3)},
                             {"case1": make_case(0, infra_n=3)})
        result = eval_policy.assess(report, default_policy())
        self.assertEqual(result["verdict"], "incomplete")


class VerdictShapeTest(unittest.TestCase):
    """判定结果结构契约:run.py 与 CI 依赖这些字段,变更需同步。"""

    def test_result_fields_present(self):
        report = make_report({"case1": make_case(3)}, {"case1": make_case(3)})
        result = eval_policy.assess(report, default_policy())
        for field in ("verdict", "would_block", "violations", "insufficient", "notes"):
            self.assertIn(field, result)
        self.assertIn(result["verdict"],
                      (eval_policy.VERDICT_PASS, eval_policy.VERDICT_REGRESSION,
                       eval_policy.VERDICT_INCOMPLETE))

    def test_would_block_matches_verdict(self):
        # would_block 必须与 verdict 严格联动,observe/enforce 都依赖它。
        cases = [
            (make_report({"case1": make_case(3)}, {"case1": make_case(3)}), False),
            (make_report({"case1": make_case(3)}, {"case1": make_case(0, fail_n=3)}), True),
        ]
        for report, expected_block in cases:
            result = eval_policy.assess(report, default_policy())
            self.assertEqual(result["would_block"], expected_block)


if __name__ == "__main__":
    unittest.main(verbosity=2)
