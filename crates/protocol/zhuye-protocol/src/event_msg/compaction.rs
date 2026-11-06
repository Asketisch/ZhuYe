//! 压缩 + 错误载荷。

use serde::{Deserialize, Serialize};

/// 上下文压缩结果:pre_loop 实际执行压缩后发出;`Op::Compact` 受理时
/// 先发一条全零的 `Noop` 回执(真正压缩在下一轮 pre_loop 进行)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextCompactedEvent {
    /// 实际生效的压缩策略(`Noop` = 未压缩)。
    pub strategy: ContextCompactedStrategy,
    /// 被移除的消息条数。
    pub removed_messages: usize,
    /// 压缩前上下文 token 数(最近一次 LLM 上报的输入 token 数与本地估算取大)。
    pub before_tokens: u32,
    /// 压缩后上下文 token 数(本地估算)。
    pub after_tokens: u32,
}

/// 错误事件载荷:turn 内或会话级错误,不终止会话(客户端决定重试或关闭)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorEvent {
    /// 机器可读的错误码(如 `"invalid_submission"` / `"prompt_rejected"` /
    /// `"RATE_LIMITED"`),客户端据此分支处理。
    pub code: String,
    /// 人类可读的错误描述。
    pub message: String,
    /// 可选的结构化附加信息,形状随 `code` 而定(如 `invalid_submission`
    /// 携带截断的原始行 `{"line": ...}`)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// 临时性 LLM 调用错误:core 随后原地重试或 failover 到其它 credential。
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct StreamErrorEvent {
    /// 大写错误码,与 `LlmError` 变体对应(如 `"RATE_LIMITED"` /
    /// `"OVERLOADED"` / `"NETWORK_ERROR"`)。
    pub code: String,
    /// 人类可读的错误描述。
    pub message: String,
    /// 重试等待时长(毫秒):原地重试为退避延迟,进 cooldown 时为该
    /// credential 的冷却时长,直接 failover 时为 0。
    pub retry_in_ms: u64,
    /// v1.0 多 Provider 路由:失败 credential 的 provider 名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// v1.0 多 Provider 路由:失败 credential 的 label。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_label: Option<String>,
    /// v1.0 多 Provider 路由:全部 candidate 试完仍失败时,记录每个
    /// 候选的结果。便于 TUI 渲染"5 个 key 都试过:work=429,
    /// personal=auth, ..."诊断表。`None` 表示未填(单 credential 失败
    /// 或仍在重试中)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tried: Option<Vec<TriedCredential>>,
}

/// v1.0 多 Provider 路由:单个 credential 失败时记录在 `StreamError.tried`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TriedCredential {
    /// 被尝试的 credential label(如 `"work"` / `"default"`)。
    pub label: String,
    /// 失败原因码(同 `LlmError` 变体名):
    /// `"rate_limited"` / `"auth"` / `"overloaded"` /
    /// `"provider_5xx"` / `"network"` / `"context_too_long"` / ...
    pub outcome: String,
}

/// 上下文压缩策略(按 escalation 顺序逐级升级:microcompact →
/// smart_prune → LLM 摘要)。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextCompactedStrategy {
    /// M1 stub 使用的 no-op。
    #[default]
    Noop,
    /// 本地启发式压缩(M4)。
    Microcompact,
    /// 阈值触发的智能剪枝(M4)。
    SmartPrune,
    /// LLM 摘要压缩(M4)。
    LlMSummarize,
}
