//! # zhuye-eval —— 评测体系(F20)
//!
//! 给 agent 出「考试卷」:固定评测集 + 自动打分。改提示词 / 换模型 / 动
//! 压缩策略之后跑一遍,就知道有没有变差。
//!
//! ## 三层结构
//!
//! 1. **断言库**([`assertion`])—— 确定性检查(包含 / 正则 / 相等 / 长度 /
//!    工具调用序列)。**先跑这一层**:便宜、稳定、无模型依赖。
//! 2. **LLM judge**([`judge`])—— 确定性断言判不了的质量问题(语气、是否
//!    真正回答了问题)。judge 走注入的闭包,测试塞 mock。
//! 3. **批量 runner**([`runner`])—— 对一个 eval set 逐条跑,汇总分数,
//!    支持按 case 打标签分组统计(用于"这次改动只影响 X 类"的定位)。
//!
//! ## 为什么 deterministic 先行
//!
//! LLM judge 有噪声、有成本、结果不稳定。能用规则判的绝不喂给 judge ——
//! 否则一次回归评估的结论可能只是 judge 那天心情不好。`EvalCase` 的
//! 设计强制:断言全部通过**才**进 judge;断言失败直接记 fail,省一次调用。
//!
//! ## 与 `zhuye-core::eval` 的关系
//!
//! `zhuye-core::eval` 是**场景回放**(mock 模型 + 事件断言),偏引擎集成
//! 测试。本 crate 是**质量评测**(回答好不好),偏产品回归。两者互补:
//! 前者验"引擎行为对不对",后者验"输出质量够不够"。

#![allow(clippy::module_name_repetitions)]

pub mod assertion;
pub mod judge;
pub mod runner;

pub use assertion::{Assertion, AssertionResult};
pub use judge::{JudgeFn, JudgeOutcome, JudgeVerdict, LlmJudge};
pub use runner::{CaseOutcome, EvalCase, EvalReport, EvalRunner};

use thiserror::Error;

/// 评测错误。
#[derive(Debug, Error)]
pub enum EvalError {
    /// 断言 DSL 非法(未知类型 / 缺字段)。
    #[error("invalid assertion: {0}")]
    InvalidAssertion(String),
    /// eval set 解析失败。
    #[error("invalid eval set: {0}")]
    InvalidEvalSet(String),
    /// judge 调用失败(网络 / provider)。
    #[error("judge failed: {0}")]
    JudgeFailed(String),
}
