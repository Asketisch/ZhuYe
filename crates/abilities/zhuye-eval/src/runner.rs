//! # 批量评测 runner(F20)
//!
//! 对一个 eval set 逐条跑:先断言(确定性),全通过才进 judge(模型)。
//! 汇总成 [`EvalReport`],支持按 tag 分组统计。
//!
//! ## 执行顺序的硬约束
//!
//! `断言失败 → 不进 judge`。这不是优化,是**正确性**:judge 对明显
//! 错误的输出也可能给 pass(模型宽容),让断言兜底才是可靠的。报告里
//! `judge_skipped` 计数让"省了多少次调用"可见。
//!
//! ## 分组统计
//!
//! 每个 case 可带 tags(`["compaction", "regression"]`)。报告按 tag
//! 聚合通过率 —— 回归时能直接看出"这次改动只打坏了 compaction 那组"。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::EvalError;
use crate::assertion::{Assertion, AssertionResult};
use crate::judge::{JudgeOutcome, LlmJudge};

/// 一个评测用例。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalCase {
    /// 用例名(报告里的行标识)。
    pub name: String,
    /// 用户任务(喂给 agent;judge 也用它判断"是否回答了").
    pub task: String,
    /// 期望要点(judge 参考;可为空)。
    #[serde(default)]
    pub expected: Option<String>,
    /// 确定性断言(全部通过才进 judge)。
    #[serde(default)]
    pub assertions: Vec<Assertion>,
    /// 分组标签。
    #[serde(default)]
    pub tags: Vec<String>,
}

impl EvalCase {
    pub fn new(name: impl Into<String>, task: impl Into<String>) -> EvalCase {
        EvalCase {
            name: name.into(),
            task: task.into(),
            expected: None,
            assertions: Vec::new(),
            tags: Vec::new(),
        }
    }

    pub fn with_expected(mut self, expected: impl Into<String>) -> EvalCase {
        self.expected = Some(expected.into());
        self
    }

    pub fn with_assertions(mut self, assertions: Vec<Assertion>) -> EvalCase {
        self.assertions = assertions;
        self
    }

    pub fn with_tags(mut self, tags: Vec<String>) -> EvalCase {
        self.tags = tags;
        self
    }

    /// 构造期校验。
    pub fn validate(&self) -> Result<(), EvalError> {
        if self.name.trim().is_empty() {
            return Err(EvalError::InvalidEvalSet("case name 不能为空".into()));
        }
        for a in &self.assertions {
            a.validate()?;
        }
        Ok(())
    }
}

/// 单条用例的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseOutcome {
    pub name: String,
    pub tags: Vec<String>,
    /// 断言结果(按声明顺序)。
    pub assertions: Vec<AssertionResult>,
    /// judge 结果(`None` = 未跑:断言失败 / 未配置 judge)。
    pub judge: Option<JudgeOutcome>,
    /// 最终是否通过。
    pub passed: bool,
}

impl CaseOutcome {
    /// 断言是否全通过。
    pub fn assertions_passed(&self) -> bool {
        self.assertions.iter().all(|a| a.passed)
    }

    /// 失败的断言(报告明细用)。
    pub fn failed_assertions(&self) -> Vec<&AssertionResult> {
        self.assertions.iter().filter(|a| !a.passed).collect()
    }
}

/// 一次 eval set 的汇总报告。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalReport {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    /// 因断言失败而跳过 judge 的次数。
    pub judge_skipped: usize,
    /// 实际跑 judge 的次数。
    pub judge_calls: usize,
    /// 逐用例结果。
    pub cases: Vec<CaseOutcome>,
    /// 按 tag 的通过率(`tag -> (passed, total)`)。
    pub by_tag: BTreeMap<String, (usize, usize)>,
}

impl EvalReport {
    /// 总通过率(0.0–1.0)。空集视为 1.0(没有失败)。
    pub fn pass_rate(&self) -> f64 {
        if self.total == 0 {
            return 1.0;
        }
        self.passed as f64 / self.total as f64
    }

    /// 是否全绿。
    pub fn all_passed(&self) -> bool {
        self.failed == 0
    }

    /// 某 tag 的通过率。
    pub fn tag_pass_rate(&self, tag: &str) -> Option<f64> {
        self.by_tag
            .get(tag)
            .map(|(p, t)| if *t == 0 { 1.0 } else { *p as f64 / *t as f64 })
    }

    /// 失败用例名列表(CI 输出用)。
    pub fn failed_names(&self) -> Vec<&str> {
        self.cases
            .iter()
            .filter(|c| !c.passed)
            .map(|c| c.name.as_str())
            .collect()
    }

    /// 一条人读摘要。
    pub fn summary(&self) -> String {
        format!(
            "{}/{} 通过({:.1}%)· judge {}/{} · 跳过 {}",
            self.passed,
            self.total,
            self.pass_rate() * 100.0,
            self.judge_calls,
            self.judge_calls + self.judge_skipped,
            self.judge_skipped
        )
    }
}

/// 批量 runner。
pub struct EvalRunner {
    /// 可选的 judge(`None` = 只跑确定性断言)。
    judge: Option<LlmJudge>,
}

impl EvalRunner {
    /// 只跑断言,不跑 judge。
    pub fn deterministic_only() -> EvalRunner {
        EvalRunner { judge: None }
    }

    /// 断言 + judge。
    pub fn with_judge(judge: LlmJudge) -> EvalRunner {
        EvalRunner { judge: Some(judge) }
    }

    /// 跑一个用例。
    ///
    /// `actual` 是 agent 的输出,`tools` 是工具调用序列。
    pub fn run_case(
        &self,
        case: &EvalCase,
        actual: &str,
        tools: &[String],
    ) -> Result<CaseOutcome, EvalError> {
        case.validate()?;
        let assertions: Vec<AssertionResult> = case
            .assertions
            .iter()
            .map(|a| a.evaluate(actual, tools))
            .collect();
        let assertions_ok = assertions.iter().all(|a| a.passed);

        let mut judge_outcome = None;
        let mut passed = assertions_ok;
        if assertions_ok && let Some(judge) = &self.judge {
            let out = judge.evaluate(&case.task, case.expected.as_deref(), actual)?;
            passed = out.verdict.is_pass();
            judge_outcome = Some(out);
        }

        Ok(CaseOutcome {
            name: case.name.clone(),
            tags: case.tags.clone(),
            assertions,
            judge: judge_outcome,
            passed,
        })
    }

    /// 跑整个 eval set。
    ///
    /// `outputs` 提供每条用例的实际输出(`name -> (text, tools)`)。缺失的
    /// 用例记为失败(不能因为"没跑"而算通过)。
    pub fn run_set(
        &self,
        cases: &[EvalCase],
        outputs: &BTreeMap<String, (String, Vec<String>)>,
    ) -> Result<EvalReport, EvalError> {
        let mut report = EvalReport::default();
        for case in cases {
            let Some((text, tools)) = outputs.get(&case.name) else {
                // 缺输出 = 没跑 = 失败(缺省通过会让 CI 假绿)。
                report.total += 1;
                report.failed += 1;
                report.cases.push(CaseOutcome {
                    name: case.name.clone(),
                    tags: case.tags.clone(),
                    assertions: vec![AssertionResult {
                        kind: "output_present".to_string(),
                        passed: false,
                        detail: "该用例没有产生输出(未执行)".to_string(),
                    }],
                    judge: None,
                    passed: false,
                });
                for tag in &case.tags {
                    let e = report.by_tag.entry(tag.clone()).or_insert((0, 0));
                    e.1 += 1;
                }
                continue;
            };
            let outcome = self.run_case(case, text, tools)?;
            report.total += 1;
            if outcome.passed {
                report.passed += 1;
            } else {
                report.failed += 1;
            }
            if outcome.judge.is_some() {
                report.judge_calls += 1;
            } else if !outcome.assertions_passed() {
                report.judge_skipped += 1;
            }
            for tag in &case.tags {
                let e = report.by_tag.entry(tag.clone()).or_insert((0, 0));
                e.1 += 1;
                if outcome.passed {
                    e.0 += 1;
                }
            }
            report.cases.push(outcome);
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::{JudgePrompt, JudgeVerdict};

    fn map(entries: &[(&str, &str, &[&str])]) -> BTreeMap<String, (String, Vec<String>)> {
        entries
            .iter()
            .map(|(name, text, tools)| {
                (
                    name.to_string(),
                    (
                        text.to_string(),
                        tools.iter().map(|s| s.to_string()).collect(),
                    ),
                )
            })
            .collect()
    }

    #[test]
    fn deterministic_pass_and_fail() {
        let cases = vec![
            EvalCase::new("c1", "t")
                .with_assertions(vec![Assertion::Contains { text: "Ok".into() }]),
            EvalCase::new("c2", "t").with_assertions(vec![Assertion::Contains {
                text: "missing".into(),
            }]),
        ];
        let outputs = map(&[("c1", "Result::Ok", &[]), ("c2", "nope", &[])]);
        let report = EvalRunner::deterministic_only()
            .run_set(&cases, &outputs)
            .expect("run");
        assert_eq!(report.total, 2);
        assert_eq!(report.passed, 1);
        assert_eq!(report.failed, 1);
        assert_eq!(report.failed_names(), vec!["c2"]);
        assert_eq!(report.judge_calls, 0);
    }

    #[test]
    fn missing_output_counts_as_failure() {
        let cases = vec![EvalCase::new("c1", "t")];
        let report = EvalRunner::deterministic_only()
            .run_set(&cases, &BTreeMap::new())
            .expect("run");
        assert_eq!(report.failed, 1, "缺输出必须算失败,不能假绿");
        assert!(!report.all_passed());
    }

    #[test]
    fn judge_skipped_when_assertions_fail() {
        let cases = vec![
            EvalCase::new("c1", "t").with_assertions(vec![Assertion::Contains {
                text: "required".into(),
            }]),
        ];
        let outputs = map(&[("c1", "missing it", &[])]);
        let judge = LlmJudge::new(Box::new(|_| panic!("断言失败时不应调用 judge")));
        let report = EvalRunner::with_judge(judge)
            .run_set(&cases, &outputs)
            .expect("run");
        assert_eq!(report.judge_skipped, 1);
        assert_eq!(report.judge_calls, 0);
    }

    #[test]
    fn judge_runs_when_assertions_pass() {
        let cases = vec![
            EvalCase::new("c1", "explain X")
                .with_expected("mentions Y")
                .with_assertions(vec![Assertion::Contains { text: "X".into() }]),
        ];
        let outputs = map(&[("c1", "X is because Y", &[])]);
        let judge = LlmJudge::new(Box::new(|prompt| {
            assert!(prompt.contains("explain X"));
            Ok((JudgeVerdict::Pass, "answered".to_string()))
        }));
        let report = EvalRunner::with_judge(judge)
            .run_set(&cases, &outputs)
            .expect("run");
        assert_eq!(report.judge_calls, 1);
        assert_eq!(report.passed, 1);
    }

    #[test]
    fn judge_fail_marks_case_failed() {
        let cases = vec![EvalCase::new("c1", "t")];
        let outputs = map(&[("c1", "some answer", &[])]);
        let judge = LlmJudge::new(Box::new(|_| {
            Ok((JudgeVerdict::Fail, "did not answer".to_string()))
        }));
        let report = EvalRunner::with_judge(judge)
            .run_set(&cases, &outputs)
            .expect("run");
        assert_eq!(report.failed, 1);
        assert_eq!(
            report.cases[0].judge.as_ref().map(|j| j.verdict),
            Some(JudgeVerdict::Fail)
        );
    }

    #[test]
    fn by_tag_aggregation() {
        let cases = vec![
            EvalCase::new("c1", "t")
                .with_tags(vec!["compaction".into()])
                .with_assertions(vec![Assertion::Contains { text: "ok".into() }]),
            EvalCase::new("c2", "t")
                .with_tags(vec!["compaction".into()])
                .with_assertions(vec![Assertion::Contains {
                    text: "missing".into(),
                }]),
            EvalCase::new("c3", "t")
                .with_tags(vec!["other".into()])
                .with_assertions(vec![Assertion::Contains { text: "ok".into() }]),
        ];
        let outputs = map(&[("c1", "ok", &[]), ("c2", "nope", &[]), ("c3", "ok", &[])]);
        let report = EvalRunner::deterministic_only()
            .run_set(&cases, &outputs)
            .expect("run");
        assert_eq!(report.by_tag["compaction"], (1, 2));
        assert_eq!(report.by_tag["other"], (1, 1));
        assert!((report.tag_pass_rate("compaction").unwrap() - 0.5).abs() < 1e-9);
        assert!((report.tag_pass_rate("other").unwrap() - 1.0).abs() < 1e-9);
        assert!(report.tag_pass_rate("nope").is_none());
    }

    #[test]
    fn empty_set_is_all_passed() {
        let report = EvalRunner::deterministic_only()
            .run_set(&[], &BTreeMap::new())
            .expect("run");
        assert!(report.all_passed());
        assert_eq!(report.pass_rate(), 1.0);
    }

    #[test]
    fn case_validation_rejects_empty_name() {
        let bad = EvalCase::new("", "t");
        assert!(bad.validate().is_err());
    }

    #[test]
    fn case_validation_propagates_assertion_errors() {
        let bad = EvalCase::new("c", "t").with_assertions(vec![Assertion::Matches {
            pattern: String::new(),
        }]);
        assert!(bad.validate().is_err());
    }

    #[test]
    fn report_summary_is_readable() {
        let cases = vec![EvalCase::new("c1", "t")];
        let outputs = map(&[("c1", "anything", &[])]);
        let report = EvalRunner::deterministic_only()
            .run_set(&cases, &outputs)
            .expect("run");
        let s = report.summary();
        assert!(s.contains("1/1"), "{s}");
        assert!(s.contains("100.0%"), "{s}");
    }

    #[test]
    fn custom_judge_prompt_is_used() {
        let cases = vec![EvalCase::new("c1", "t")];
        let outputs = map(&[("c1", "ans", &[])]);
        let judge = LlmJudge::new(Box::new(|prompt| {
            assert!(prompt.contains("必须提到性能"), "自定义标准未生效:{prompt}");
            Ok((JudgeVerdict::Pass, "ok".to_string()))
        }))
        .with_prompt(JudgePrompt::with_criteria("必须提到性能"));
        let report = EvalRunner::with_judge(judge)
            .run_set(&cases, &outputs)
            .expect("run");
        assert!(report.all_passed());
    }
}
