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

/// 本 turn 当前流式 assistant 输出被重置(additive 新变体,v1.10)。
///
/// 模型流式调用中途失败且已有可见增量时,引擎将**自动重试**该次调用;
/// 重试会从头重新生成,客户端应清空本 turn **正在流式生成**的 assistant
/// 消息缓冲(文本 / thinking / 尚未 `ToolCallEnd` 的半成品工具调用),
/// 等待重试的全新输出 —— 否则两次输出会拼接污染。
/// 已完成的工具调用(`ToolCallEnd` 已发)不受影响。
/// 旧客户端按未知变体降级忽略(此时会看到两次输出拼接,属已知降级)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessageResetEvent {
    /// 触发重置的错误码(如 `"SSE_PARSE_ERROR"`),供客户端展示重试原因。
    pub reason_code: String,
    /// 本次重试的退避等待(ms);客户端可显示「N ms 后重试」。
    pub retry_in_ms: u64,
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
/// 为 `None`,且 `turn_total = true`)。各计数口径同 `TokenUsage`。
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
    /// v1.x:本条计量是否为**回合累计**。
    ///
    /// 引擎在回合收尾时会补发一条累计 `TokenCount`(数字 = 本回合逐次
    /// 调用的累计,`provider` / `credential_label` / `model` 均为 `None`)。
    /// 它与逐次调用的快照语义不同 —— 消费方若把两者**一并累计**,会话
    /// 总量与费用会翻倍(per-call 之和 + 回合累计)。本字段给出显式判据。
    ///
    /// `false`(缺省)= 单次调用;旧引擎不发该字段 → 按单次调用处理,
    /// 线缆双向兼容。单次调用时序列化省略本字段(逐次调用事件的 wire
    /// 与本字段引入前完全一致)。
    #[serde(default, skip_serializing_if = "is_false")]
    pub turn_total: bool,
}

/// serde 的 `skip_serializing_if` 谓词:布尔为 `false` 时省略该字段。
fn is_false(value: &bool) -> bool {
    !*value
}

/// v1.12:上下文组成类别的稳定 id。六类覆盖「这个 turn 的 input 上下文
/// 由什么构成」;对齐 context composition 形态(上游 11 细类在此
/// 归并为 6 粗类:提示词库主体 / 记忆注入 / 技能与临时层 / 内置工具
/// schema / 外部(MCP·LSP·插件)工具 schema / 对话消息)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CompositionCategoryId {
    /// 系统提示词主体(提示词库 core 段 + agent 定义 + mode 段,不含
    /// 记忆注入与 checklist)。
    #[default]
    SystemPrompt,
    /// 记忆检索注入(pre_loop 拼进 system prompt 的相关记忆文本)。
    Memory,
    /// 技能与临时层(`<system-reminder>` 块:技能目录 + 工具目录文本 +
    /// 迭代提醒 + mode hint)。
    Skills,
    /// 内置工具 JSON schema(`ToolSource::Builtin` 的工具参数定义)。
    ToolSchemas,
    /// 外部工具 JSON schema(MCP / LSP / 插件 / 远程注册,即非 Builtin 源)。
    McpTools,
    /// 对话消息历史(含内联 system-reminder 注入与压缩摘要)。
    Messages,
    /// 前向兼容:未知类别(旧 consumer 容错,仅反序列化路径可出现)。
    #[serde(other)]
    Unknown,
}

/// 一个类别的估算 token 量。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionCategory {
    /// 类别 id。
    pub id: CompositionCategoryId,
    /// 估算 token 数。**估算值,非精确计量**:复用 compact 的启发式 /
    /// tiktoken 估计器,各类别独立计量,不保证求和等于 provider 上报的
    /// `input_tokens`。
    pub tokens: u32,
}

/// v1.12:上下文组成快照(additive 新变体)。
///
/// 每次 LLM 请求组装后计量一次;组成与上次相比无变化时**省略不发**
/// (按类别序列指纹去重),避免高频 delta 流淹没事件通道。客户端据此
/// 渲染「上下文容量分解」条形(系统提示词 / 记忆 / 技能 / 工具 schema /
/// 外部工具 / 消息各占多少)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextCompositionEvent {
    /// 六类分解(顺序固定为 id 枚举声明序,客户端按 id 取值)。
    pub categories: Vec<CompositionCategory>,
}
