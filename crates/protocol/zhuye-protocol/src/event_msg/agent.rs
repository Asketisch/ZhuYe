//! LLM 输出载荷:助手消息、thinking delta、token 用量。

use serde::{Deserialize, Serialize};

/// 完整(非增量)助手消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    /// 助手消息全文。
    pub text: String,
}

/// 流式助手消息分片。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessageDelta {
    /// 本次追加的文本增量(按到达顺序拼接即得完整消息)。
    pub delta: String,
}

/// thinking delta 的语义类型。此前是自由 `String`(构造端每片
/// `"raw".to_string()` 分配一次),类型化为枚举:wire 形状不变
/// (`"raw"` / `"summary"` 小写),未知值容错为 `Unknown` 不崩。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingDeltaKind {
    /// token 级流式(默认)。
    #[default]
    Raw,
    /// 推理摘要:TUI reducer 路由到 `MessageRow::ReasoningSummary`
    /// (区别于原始 `Thinking`)。
    Summary,
    /// 前向兼容:未知语义值(仅反序列化路径可出现)。
    #[serde(other)]
    Unknown,
}

/// 模型思考(thinking / reasoning)内容的流式分片。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingDelta {
    /// 本次追加的思考文本增量。
    pub delta: String,
    /// P4:thinking delta 的语义类型(默认 [`ThinkingDeltaKind::Raw`])。
    /// `#[serde(default)]` 保持与旧事件的线缆兼容。
    #[serde(default)]
    pub kind: ThinkingDeltaKind,
}

impl Default for ThinkingDelta {
    fn default() -> Self {
        Self {
            delta: String::new(),
            kind: ThinkingDeltaKind::Raw,
        }
    }
}

/// Token 用量快照:每次模型调用成功后发出该次调用的用量(附 provider /
/// credential / model);turn 收尾时再发一次本 turn 累计用量(此时这三者
/// 为 `None`)。各计数口径同 `TokenUsage`。
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TokenCountEvent {
    /// 输入 token 数(含 `cached_tokens` / `cache_write_tokens` 子片段)。
    pub input_tokens: u32,
    /// 输出 token 数。
    pub output_tokens: u32,
    /// 缓存读取命中的 token 数,是 `input_tokens` 的子集。
    pub cached_tokens: u32,
    /// M8:cache_creation 段(已折入 `input_tokens`);不支持 prompt caching
    /// 的 provider 上为 0。`#[serde(default)]` 保证事件与 M7 consumer 的
    /// 线缆兼容。
    #[serde(default)]
    pub cache_write_tokens: u32,
    /// 可计费 token 总数(`input_tokens + output_tokens`)。
    pub total_tokens: u32,
    /// M8 P1a:本轮 USD 费用,由 `zhuye_llm::providers::pricing` 算出。
    /// 模型未知或定价表为空时为 `None`。`#[serde(default)]` 保证事件
    /// 与 M7 consumer 的线缆兼容。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// v1.0 多 Provider 路由:本轮实际命中的 provider 名(如 `"anthropic"`)。
    /// `#[serde(default, skip_serializing_if = "Option::is_none")]` 双向
    /// 兼容:旧 consumer 解析时填 `None`,序列化时缺省。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// v1.0 多 Provider 路由:命中的 credential label(如 `"work"` /
    /// `"default"`)。同 `provider` 字段的兼容策略。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_label: Option<String>,
    /// v1.x 模型级 token 统计:本轮实际命中的模型 spec(如 `"openai/gpt-4o"`)。
    /// 与 `provider` 同兼容策略:`#[serde(default, skip_serializing_if)]`
    /// 双向兼容 —— 旧 consumer 解析时填 `None`,序列化时缺省。TUI / GUI
    /// 据此实时按模型展示用量。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}
