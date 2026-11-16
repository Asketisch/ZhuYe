//! # LLM judge(F20)
//!
//! 确定性断言判不了的问题(「这个解释是否真的回答了用户」「语气是否
//! 得体」)交给第二个模型打分。
//!
//! ## 三条纪律
//!
//! 1. **judge 只在断言全通过后跑** —— 断言失败已经能定性,再花一次
//!    模型调用只是浪费。`EvalRunner` 保证这个顺序。
//! 2. **judge 输出结构化**([`JudgeVerdict`])而不是自由文本 —— 自由文本
//!    的"看起来还不错"没法参与聚合统计。
//! 3. **judge 可被 mock** —— [`JudgeFn`] 是闭包,测试塞纯函数。评测框架
//!    自身的测试不能依赖真模型(否则 CI 不稳定)。
//!
//! ## 提示词模板
//!
//! [`JudgePrompt`] 把「任务 / 期望 / 实际输出」拼成固定形状的提示词。
//! 固定形状的价值:judge 的判定标准稳定,不同 case 之间可比。

use serde::{Deserialize, Serialize};

use crate::EvalError;

/// judge 裁决。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgeVerdict {
    /// 通过(输出满足任务要求)。
    Pass,
    /// 不通过。
    Fail,
    /// 说不准 —— 记为 fail 但不计为硬性回归(报告里单列)。
    Uncertain,
}

impl JudgeVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            JudgeVerdict::Pass => "pass",
            JudgeVerdict::Fail => "fail",
            JudgeVerdict::Uncertain => "uncertain",
        }
    }

    /// 是否算作通过(只有 Pass 算)。
    pub fn is_pass(self) -> bool {
        matches!(self, JudgeVerdict::Pass)
    }

    /// 从模型自由文本里解析裁决(容忍 `PASS` / `pass` / 含解释的形态)。
    pub fn parse(s: &str) -> JudgeVerdict {
        let lower = s.trim().to_ascii_lowercase();
        // 先看开头 token:模型常输出 `pass - reason...`
        let head = lower
            .split(|c: char| c.is_whitespace() || c == ':' || c == '-')
            .next()
            .unwrap_or("");
        match head {
            "pass" | "ok" | "yes" | "通过" => JudgeVerdict::Pass,
            "fail" | "no" | "不通过" => JudgeVerdict::Fail,
            _ => {
                // 回退:全文包含判定词。
                if lower.contains("fail") || lower.contains("不通过") {
                    JudgeVerdict::Fail
                } else if lower.contains("pass") || lower.contains("通过") {
                    JudgeVerdict::Pass
                } else {
                    JudgeVerdict::Uncertain
                }
            }
        }
    }
}

/// judge 一次调用的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JudgeOutcome {
    pub verdict: JudgeVerdict,
    /// judge 给的理由(进报告,便于人工复核)。
    pub reason: String,
}

impl JudgeOutcome {
    pub fn new(verdict: JudgeVerdict, reason: impl Into<String>) -> JudgeOutcome {
        JudgeOutcome {
            verdict,
            reason: reason.into(),
        }
    }
}

/// judge 函数签名:输入提示词,输出 `(verdict, reason)`。
///
/// 生产实现接 `zhuye-llm`;测试实现是纯函数。用 `Fn` 而非 async,
/// 调用方在异步上下文里自行 `spawn_blocking`。
pub type JudgeFn = Box<dyn Fn(&str) -> Result<(JudgeVerdict, String), EvalError> + Send + Sync>;

/// judge 提示词构造器。
#[derive(Debug, Clone)]
pub struct JudgePrompt {
    /// 评判标准(系统级要求)。
    pub criteria: String,
}

impl Default for JudgePrompt {
    fn default() -> Self {
        JudgePrompt {
            criteria: "判断 agent 的输出是否真正满足了用户任务。\
                       只看是否解决问题,不苛求措辞。"
                .to_string(),
        }
    }
}

impl JudgePrompt {
    /// 自定义评判标准。
    pub fn with_criteria(criteria: impl Into<String>) -> JudgePrompt {
        JudgePrompt {
            criteria: criteria.into(),
        }
    }

    /// 渲染完整提示词。
    pub fn render(&self, task: &str, expected: Option<&str>, actual: &str) -> String {
        let mut out = String::new();
        out.push_str("你是一个严格的评测员。\n\n");
        out.push_str("## 评判标准\n");
        out.push_str(&self.criteria);
        out.push_str("\n\n## 用户任务\n");
        out.push_str(task);
        out.push('\n');
        if let Some(exp) = expected {
            out.push_str("\n## 期望要点\n");
            out.push_str(exp);
            out.push('\n');
        }
        out.push_str("\n## agent 实际输出\n");
        out.push_str(actual);
        out.push_str(
            "\n\n## 输出格式\n\
             只输出一行:`pass` 或 `fail`,后接冒号与一句话理由。\n\
             例:`pass: 完整回答了问题并给出了可运行的代码`\n",
        );
        out
    }
}

/// judge 包装器。
pub struct LlmJudge {
    judge: JudgeFn,
    prompt: JudgePrompt,
}

impl LlmJudge {
    pub fn new(judge: JudgeFn) -> LlmJudge {
        LlmJudge {
            judge,
            prompt: JudgePrompt::default(),
        }
    }

    pub fn with_prompt(mut self, prompt: JudgePrompt) -> LlmJudge {
        self.prompt = prompt;
        self
    }

    /// 评判一次输出。
    pub fn evaluate(
        &self,
        task: &str,
        expected: Option<&str>,
        actual: &str,
    ) -> Result<JudgeOutcome, EvalError> {
        let prompt = self.prompt.render(task, expected, actual);
        let (verdict, reason) = (self.judge)(&prompt)?;
        Ok(JudgeOutcome { verdict, reason })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_parse_tolerant() {
        assert_eq!(JudgeVerdict::parse("pass"), JudgeVerdict::Pass);
        assert_eq!(JudgeVerdict::parse("PASS: good"), JudgeVerdict::Pass);
        assert_eq!(
            JudgeVerdict::parse("fail - wrong answer"),
            JudgeVerdict::Fail
        );
        assert_eq!(JudgeVerdict::parse("通过"), JudgeVerdict::Pass);
        assert_eq!(JudgeVerdict::parse("不通过"), JudgeVerdict::Fail);
        assert_eq!(JudgeVerdict::parse("hmm"), JudgeVerdict::Uncertain);
        // 含解释的形态:开头 token 决定。
        assert_eq!(
            JudgeVerdict::parse("fail: 没回答核心问题"),
            JudgeVerdict::Fail
        );
    }

    #[test]
    fn verdict_is_pass_only_for_pass() {
        assert!(JudgeVerdict::Pass.is_pass());
        assert!(!JudgeVerdict::Fail.is_pass());
        assert!(!JudgeVerdict::Uncertain.is_pass());
    }

    #[test]
    fn prompt_includes_all_sections() {
        let p = JudgePrompt::default();
        let rendered = p.render("修复登录 bug", Some("给出根因"), "改了 cookie 过期时间");
        assert!(rendered.contains("评判标准"));
        assert!(rendered.contains("修复登录 bug"));
        assert!(rendered.contains("给出根因"));
        assert!(rendered.contains("改了 cookie 过期时间"));
        assert!(rendered.contains("只输出一行"));
    }

    #[test]
    fn prompt_without_expected_omits_section() {
        let p = JudgePrompt::default();
        let rendered = p.render("task", None, "out");
        assert!(!rendered.contains("期望要点"));
    }

    #[test]
    fn custom_criteria() {
        let p = JudgePrompt::with_criteria("必须提到性能");
        assert!(p.render("t", None, "o").contains("必须提到性能"));
    }

    #[test]
    fn judge_wrapper_calls_fn_and_returns_outcome() {
        let judge = LlmJudge::new(Box::new(|prompt| {
            assert!(prompt.contains("task-x"));
            Ok((JudgeVerdict::Pass, "looks good".to_string()))
        }));
        let out = judge.evaluate("task-x", None, "answer").expect("ok");
        assert_eq!(out.verdict, JudgeVerdict::Pass);
        assert_eq!(out.reason, "looks good");
    }

    #[test]
    fn judge_error_propagates() {
        let judge = LlmJudge::new(Box::new(|_| Err(EvalError::JudgeFailed("boom".into()))));
        assert!(judge.evaluate("t", None, "o").is_err());
    }

    #[test]
    fn outcome_serializes() {
        let o = JudgeOutcome::new(JudgeVerdict::Fail, "wrong");
        let json = serde_json::to_string(&o).expect("serialize");
        assert!(json.contains("\"verdict\":\"fail\""), "{json}");
        let back: JudgeOutcome = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, o);
    }
}
