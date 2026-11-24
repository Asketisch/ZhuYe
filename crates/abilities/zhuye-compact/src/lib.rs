//! zhuye-compact —— 上下文压缩策略。
//!
//! Zhuye 路线图的 M4 阶段。对应 zhuye 的 `graph.py:330-609`
//! (microcompact + smart_prune)与 `summarizer.py`(LLM-summarize)
//! 的移植版。
//!
//! 三种压缩策略,外加一个 noop:
//!
//! - [`microcompact`] —— 本地启发式,不调用 LLM。固定保留 `System` 与
//!   第一条 `User`;将旧工具结果替换为占位符;剥离 thinking 块。
//! - [`smart_prune`] —— 本地启发式,不调用 LLM。按工具类型应用截断
//!   规则(grep → 头部,bash → 尾部,read → 头尾,其余 → 按字符)。
//!   在低于 `target_tokens` 之前,持续丢弃最旧的非固定保留消息。
//! - [`summarizer::Summarizer`] + [`LlmSummarizer`] —— 异步,LLM 驱动。
//!   生成包含 9 个章节的中文摘要,以 `<summary>` 标签包裹。
//!
//! [`strategy::Compactor`] 将上述三者串联:估算 token,先执行
//! microcompact,再升级到 smart_prune,最后调用 LLM 摘要。

/// F26:上下文治理(可搜索溢出句柄 + 输出预算自适应 + 缓存友好度)。
pub mod context_gov;
pub mod microcompact;
/// v1.x:rapid-refill 熔断器(阻断「压缩 → 立即填满」死循环)。
pub mod refill;
pub mod smart_prune;
pub mod strategy;
pub mod summarizer;
pub mod tokens;
pub mod tool_pair;

pub use refill::{
    RAPID_REFILL_MAX_CONSECUTIVE, RAPID_REFILL_WINDOW_STEPS, RapidRefillGuard, RefillDecision,
};

pub use context_gov::{
    CacheFriendly, SpillHandle, SpillStore, adaptive_output_cap, cache_friendly, stable_prefix_len,
};
pub use microcompact::{
    CompactReport, KEEP_RECENT_DEFAULT, MICROCOMPACT_TRIGGER_RATIO, MicrocompactConfig,
    PRESERVE_TOOL_NAMES, TRUNCATABLE_TOOL_NAMES, microcompact,
};
pub use smart_prune::{
    MAX_ASSISTANT_CHARS, MAX_TOOL_RESULT_CHARS, MAX_TOOL_RESULT_LINES, SmartPruneConfig,
    smart_prune,
};
pub use strategy::{
    CompactionOutcome, Compactor, CompactorConfig, DEFAULT_TRIGGER_TOKENS, MemoryWriteInfo,
    TriggerPolicy,
};
pub use summarizer::{
    LlmSummarizer, SUMMARIZE_PROMPT_FULL, SUMMARIZE_PROMPT_RECENT, SUMMARIZE_TIMEOUT, Summarizer,
    SummarizerError,
};
#[cfg(feature = "tokenizer")]
pub use tokens::tiktoken::{TiktokenEstimator, global_tiktoken_estimator};
pub use tokens::{
    HeuristicEstimator, TokenEstimator, estimate_messages, estimate_text, global_estimator,
    set_global_estimator,
};
