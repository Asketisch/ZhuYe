//! `ChatRequest` 及其嵌套类型。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use zhuye_protocol::ToolOutput;

// ── 顶层请求 ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    /// 模型名,**不**含 provider 前缀(如 `"gpt-4o"`、`"claude-3-5-sonnet-latest"`)。
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    #[serde(default)]
    pub system: SystemBlocks,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    #[serde(default)]
    pub cache_control: Vec<CacheBreak>,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    #[serde(default)]
    pub stop: Vec<String>,
    /// v1.4 B1:结构化输出格式。`None`(默认)= 纯文本(历史行为);
    /// `Some` = 请求 provider 按 JSON 语义约束输出,各 client 负责映射:
    /// - OpenAI Chat Completions → `response_format` 参数;
    /// - OpenAI Responses → `text.format`;
    /// - Anthropic → 强制 `structured_output` 工具调用,流侧解包参数为
    ///   文本(对上层透明);
    /// - Ollama → `format` 字段("json" 或 schema 对象)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
}

impl Default for ChatRequest {
    fn default() -> Self {
        Self {
            model: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            system: SystemBlocks::default(),
            temperature: None,
            max_tokens: None,
            top_p: None,
            thinking: None,
            cache_control: Vec::new(),
            metadata: HashMap::new(),
            stop: Vec::new(),
            response_format: None,
        }
    }
}

impl ChatRequest {
    /// 把 `messages` 里内联的 `System` 消息折叠进 `system` 块。
    ///
    /// OpenAI 兼容(Chat Completions)/ Ollama / OpenAI Responses 的线上形态
    /// 都要求 `system` 只能出现在数组**开头**,严格的网关对「用户消息之后
    /// 再出现 system」直接返回 HTTP 400
    /// (`System message must be at the beginning.`);Anthropic 的 system
    /// 是顶层字段,消息流里的 `System` 会被静默丢弃(内容直接丢失)。
    ///
    /// 折叠后请求恰好只有一条首位 system,语义不变(压缩摘要 / hook 注入
    /// 内容仍在上下文里),并且 `inject_cache_control` 按**消息下标**写入的
    /// 缓存断点不会因为 provider 丢弃消息而漂移。
    ///
    /// 幂等;若 `messages` 全是 `System`(折叠后为空),保持原样 —— 上游
    /// 对空 `messages` 会返回 `messages must not be empty`。
    pub fn fold_system_messages(&mut self) {
        if !self
            .messages
            .iter()
            .any(|m| matches!(m, ChatMessage::System(_)))
        {
            return;
        }
        let inline: Vec<String> = self
            .messages
            .iter()
            .filter_map(|m| match m {
                ChatMessage::System(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        // 全 System 时不折叠:折叠后 messages 为空会被 provider 拒绝。
        if inline.len() == self.messages.len() {
            return;
        }
        self.messages
            .retain(|m| !matches!(m, ChatMessage::System(_)));
        for text in inline {
            // 空 system 块只占位置,不产生任何语义。
            if text.trim().is_empty() {
                continue;
            }
            self.system.0.push(SystemBlock {
                text,
                cache_control: None,
                ephemeral: false,
            });
        }
    }
}

/// v1.4 B1:结构化输出格式声明。
///
/// `JsonSchema` 是强约束(provider 按 schema 严格生成);`JsonObject`
/// 是弱约束(只保证输出是合法 JSON 对象,不保证字段结构)。`Text`
/// 显式声明纯文本(等价于 `None`,供需要显式表达的调用方使用)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResponseFormat {
    Text,
    /// 仅保证输出为合法 JSON 对象。
    JsonObject,
    /// 按 JSON Schema 严格生成。
    JsonSchema {
        /// schema 名称(OpenAI `json_schema.name` 必填;Anthropic 侧作
        /// 为强制工具的说明文本)。
        name: String,
        /// JSON Schema 定义(原样透传给 provider)。
        schema: Value,
        /// 是否启用严格模式(不允许额外字段;provider 不支持时忽略)。
        #[serde(default = "default_strict")]
        strict: bool,
    },
}

fn default_strict() -> bool {
    true
}

// ── 消息 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ChatMessage {
    System(String),
    User(UserContent),
    Assistant(AssistantContent),
    Tool(ToolResult),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserContent {
    pub blocks: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AssistantContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub id: String,
    pub name: String,
    /// 完整缓冲的 JSON 参数(由多个 delta 拼接而成)。
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    /// 工具输出的类型化内容块。承载 `Text` 与 `Image`(多模态)块,
    /// 让支持图片工具结果(Anthropic)的 provider 能以可查看图片的形式
    /// 暴露给模型,而不是压平成字节数组字符串。此前本字段是 `String`,
    /// 通过 JSON 序列化整个 content 数组得到,这会破坏图片内容
    /// (`[{"type":"image","data":[137,80,...]}]` 文本)—— 是所有"图片查看"
    /// 类问题失败的根因(GAIA chess 等)。
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub is_error: bool,
}

impl ToolResult {
    pub fn from_output(call_id: impl Into<String>, output: &ToolOutput) -> Self {
        // 将协议层 ContentBlock 映射到 LLM 层 ContentBlock,
        // 保留 Image 块(多模态),而非压平成 JSON 字符串。
        // 非多模态的协议变体(Diff / ToolUse / 嵌套 ToolResult)降级为文本表示。
        let content = output
            .content
            .iter()
            .map(|b| match b {
                zhuye_protocol::ContentBlock::Text { text } => {
                    ContentBlock::Text { text: text.clone() }
                }
                zhuye_protocol::ContentBlock::Image { data, mime_type } => ContentBlock::Image {
                    data: data.clone(),
                    mime_type: mime_type.clone(),
                },
                // 非多模态 / 结构化变体:序列化为字符串,让模型仍能看到内容
                // (这些变体从不承载可查看图片)。
                other => ContentBlock::Text {
                    text: serde_json::to_string(other).unwrap_or_default(),
                },
            })
            .collect();
        Self {
            call_id: call_id.into(),
            content,
            is_error: output.is_error,
        }
    }

    /// 将内容块压平为纯文本字符串,供需要单字符串的 provider / 消费者使用
    /// (token 估算、summarizer、OpenAI / Ollama 只接受纯文本的
    /// tool-result `content` 字段)。Image 块渲染为短占位(绝不输出原始字节)。
    pub fn content_as_text(&self) -> String {
        let mut out = String::new();
        for b in &self.content {
            match b {
                ContentBlock::Text { text } => {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
                ContentBlock::Image { data, mime_type } => {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&format!("[image: {mime_type}, {} bytes]", data.len()));
                }
            }
        }
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image { data: Vec<u8>, mime_type: String },
}

// 部分 provider 的工具结果只能放文本。保持整批工具结果相邻,然后
// 以用户图片消息提供实际图像,避免把图片丢成占位文本。
pub(crate) fn lift_tool_images(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    fn flush(pending: &mut Vec<ContentBlock>, output: &mut Vec<ChatMessage>) {
        if !pending.is_empty() {
            output.push(ChatMessage::User(UserContent {
                blocks: std::mem::take(pending),
            }));
        }
    }
    let mut output = Vec::with_capacity(messages.len());
    let mut pending = Vec::new();
    for mut message in messages {
        if let ChatMessage::Tool(result) = &mut message {
            for block in &mut result.content {
                if let ContentBlock::Image { data, mime_type } = block {
                    let placeholder = format!("[image: {mime_type}, {} bytes]", data.len());
                    let image = std::mem::replace(block, ContentBlock::Text { text: placeholder });
                    pending.push(ContentBlock::Text {
                        text: format!("[工具调用 {} 返回的图片]", result.call_id),
                    });
                    pending.push(image);
                }
            }
        } else {
            flush(&mut pending, &mut output);
        }
        output.push(message);
    }
    flush(&mut pending, &mut output);
    output
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        ContentBlock::Text { text: s.into() }
    }
}

// ── system 块 + 缓存控制 ────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemBlocks(pub Vec<SystemBlock>);

impl SystemBlocks {
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|b| b.text.is_empty())
    }
    /// 将非空 system 合并为单字符串(OpenAI 没有 system blocks;Anthropic 有)。
    pub fn as_single_string(&self) -> Option<String> {
        let parts: Vec<&str> = self.0.iter().map(|b| b.text.as_str()).collect();
        let joined = parts.join("\n");
        if joined.is_empty() {
            None
        } else {
            Some(joined)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemBlock {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
    #[serde(default)]
    pub ephemeral: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheControlKind {
    Ephemeral,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheTtl {
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub kind: CacheControlKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<CacheTtl>,
}

impl Default for CacheControl {
    fn default() -> Self {
        Self {
            kind: CacheControlKind::Ephemeral,
            ttl: Some(CacheTtl::FiveMinutes),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheBreak {
    pub after_message_index: usize,
    pub ttl: CacheTtl,
}

// ── 思考配置 ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingConfig {
    Enabled { budget_tokens: u32 },
    Disabled,
    OpenAIReasoning { effort: ReasoningEffort },
}

/// 思考强度档位。v1.10 功能4:在既有 Low/Medium/High 之上新增 `Max` /
/// `Ultra`,构成完整阶梯 `Low < Medium < High < Max < Ultra`
/// (`PartialOrd` 按变体声明序比较,供能力表钳制取 `min`)。
///
/// xhigh 批次:在 High 与 Max 之间插入 `XHigh`(wire 词 `xhigh`,显式
/// rename 避开 snake_case 的 `x_high`),阶梯变为
/// `Low < Medium < High < XHigh < Max < Ultra`。qwen3.8 系等新厂商词表
/// 的顶档 `xhigh` 由此获得真实档位,且线性阶梯语义不变。
///
/// 不同模型族支持的最高档不同(qwen 系到 High、qwen3.8 系到 XHigh、
/// deepseek 到 Max、OpenAI 到 Ultra),由
/// [`crate::reasoning_effort::clamp_effort_for_model`] 在发 wire 前按
/// 目标模型钳制 —— 本枚举只表达「调用方想要的强度」。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    /// 高于 `high`、低于 `max` 的厂商档(qwen3.8 系顶档)。
    #[serde(rename = "xhigh")]
    XHigh,
    Max,
    Ultra,
}

/// v1.x S4:把协议层 `ReasoningEffortMirror` 桥接到 LLM 层 `ReasoningEffort`。
///
/// 协议层独立枚举是为了避免 `protocol → llm` 反向依赖;`submission_loop`
/// / `model_call` 是**唯一**需要该转换的地方,集中在 `From` impl 便于
/// 协议字段扩展(加 `XHigh` / `Max` / `Ultra` 等)时单点同步。
impl From<zhuye_protocol::ReasoningEffortMirror> for ReasoningEffort {
    fn from(m: zhuye_protocol::ReasoningEffortMirror) -> Self {
        match m {
            zhuye_protocol::ReasoningEffortMirror::Low => ReasoningEffort::Low,
            zhuye_protocol::ReasoningEffortMirror::Medium => ReasoningEffort::Medium,
            zhuye_protocol::ReasoningEffortMirror::High => ReasoningEffort::High,
            zhuye_protocol::ReasoningEffortMirror::XHigh => ReasoningEffort::XHigh,
            zhuye_protocol::ReasoningEffortMirror::Max => ReasoningEffort::Max,
            zhuye_protocol::ReasoningEffortMirror::Ultra => ReasoningEffort::Ultra,
        }
    }
}

// ── Tool spec(供 LLM 层使用的 zhuye_tools::ToolSpec 镜像)─────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolSpec {
    Function {
        name: String,
        description: String,
        parameters: Value,
    },
}

impl ToolSpec {
    pub fn name(&self) -> &str {
        match self {
            ToolSpec::Function { name, .. } => name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_request_serde_roundtrip() {
        let req = ChatRequest {
            response_format: None,
            model: "gpt-4o".into(),
            messages: vec![ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::text("hi")],
            })],
            tools: vec![],
            system: SystemBlocks::default(),
            temperature: Some(0.7),
            max_tokens: Some(1024),
            top_p: None,
            thinking: None,
            cache_control: vec![],
            metadata: HashMap::new(),
            stop: vec![],
        };
        let j = serde_json::to_string(&req).unwrap();
        let back: ChatRequest = serde_json::from_str(&j).unwrap();
        assert_eq!(back.model, "gpt-4o");
        assert_eq!(back.messages.len(), 1);
    }

    #[test]
    fn system_blocks_as_single_string_skips_empty() {
        let blocks = SystemBlocks(vec![SystemBlock {
            text: "x".into(),
            cache_control: None,
            ephemeral: false,
        }]);
        assert_eq!(blocks.as_single_string().as_deref(), Some("x"));
        assert!(SystemBlocks::default().as_single_string().is_none());
    }

    /// GAIA-fix:`from_output` 必须把 protocol 层的 `Image` 块**保留**为 LLM 层的
    /// `ContentBlock::Image`(而非 JSON 序列化成字节数组字符串),这样 provider
    /// 才能把图片以 base64 形式发给模型。
    #[test]
    fn from_output_preserves_image_block() {
        let output = zhuye_protocol::ToolOutput {
            content: vec![zhuye_protocol::ContentBlock::Image {
                data: vec![0x89, 0x50, 0x4e, 0x47],
                mime_type: "image/png".into(),
            }],
            is_error: false,
            metadata: serde_json::json!({}),
            elapsed_ms: 0,
        };
        let r = ToolResult::from_output("c1", &output);
        assert_eq!(r.content.len(), 1);
        match &r.content[0] {
            ContentBlock::Image { data, mime_type } => {
                assert_eq!(data, &vec![0x89, 0x50, 0x4e, 0x47]);
                assert_eq!(mime_type, "image/png");
            }
            other => panic!("expected Image block, got {other:?}"),
        }
    }

    /// `from_output` 文本块映射为 LLM 层 Text 块(回归保护)。
    #[test]
    fn from_output_maps_text_block() {
        let output = zhuye_protocol::ToolOutput {
            content: vec![zhuye_protocol::ContentBlock::Text {
                text: "hello".into(),
            }],
            is_error: false,
            metadata: serde_json::json!({}),
            elapsed_ms: 0,
        };
        let r = ToolResult::from_output("c1", &output);
        assert_eq!(r.content_as_text(), "hello");
    }

    /// `content_as_text`:混合 Text+Image 块时,Image 渲染为短占位(非原始字节)。
    #[test]
    fn content_as_text_image_placeholder() {
        let r = ToolResult {
            call_id: "c1".into(),
            content: vec![
                ContentBlock::text("a board state"),
                ContentBlock::Image {
                    data: vec![1; 5000],
                    mime_type: "image/png".into(),
                },
            ],
            is_error: false,
        };
        let text = r.content_as_text();
        assert!(text.contains("a board state"));
        assert!(text.contains("[image: image/png, 5000 bytes]"));
        // 关键:绝不能把字节数组灌进文本。
        assert!(!text.contains("[1, 1, 1"));
    }

    /// 构造一条纯文本 User 消息(测试辅助)。
    fn user(text: &str) -> ChatMessage {
        ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::text(text)],
        })
    }

    /// 折叠内联 System:末尾注入的 system(PreCompact hook / 压缩摘要)与
    /// 中途 system 都必须上移到 `system` 块,`messages` 只留对话轮次。
    #[test]
    fn fold_system_messages_moves_inline_system_into_system_blocks() {
        let mut req = ChatRequest {
            model: "deepseek-chat".into(),
            messages: vec![
                ChatMessage::System("<summary>\n旧摘要\n</summary>".into()),
                user("你好"),
                ChatMessage::Assistant(AssistantContent::default()),
                ChatMessage::System("hook 注入的提醒".into()),
            ],
            system: SystemBlocks(vec![SystemBlock {
                text: "核心 system prompt".into(),
                cache_control: None,
                ephemeral: false,
            }]),
            ..Default::default()
        };

        req.fold_system_messages();

        // messages 里不再有任何 System,顺序保持。
        assert!(
            !req.messages
                .iter()
                .any(|m| matches!(m, ChatMessage::System(_))),
            "折叠后不应残留内联 System: {:?}",
            req.messages
        );
        assert!(matches!(&req.messages[0], ChatMessage::User(_)));
        assert_eq!(req.messages.len(), 2);

        // system 块按原出现顺序追加在核心 prompt 之后。
        let texts: Vec<&str> = req.system.0.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "核心 system prompt",
                "<summary>\n旧摘要\n</summary>",
                "hook 注入的提醒"
            ]
        );
    }

    /// 幂等:重复折叠不改变结果。
    #[test]
    fn fold_system_messages_is_idempotent() {
        let mut req = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![user("hi"), ChatMessage::System("note".into())],
            ..Default::default()
        };
        req.fold_system_messages();
        let once = (req.messages.len(), req.system.0.len());
        req.fold_system_messages();
        assert_eq!(once, (req.messages.len(), req.system.0.len()));
        assert_eq!(req.system.0[0].text, "note");
    }

    /// 守卫:全部是 System 的消息列表不折叠 —— 折叠后 messages 为空,
    /// 上游会返回 `messages must not be empty`。
    #[test]
    fn fold_system_messages_keeps_all_system_list_untouched() {
        let mut req = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![ChatMessage::System("only".into())],
            ..Default::default()
        };
        req.fold_system_messages();
        assert!(req.system.0.is_empty());
        assert_eq!(req.messages.len(), 1);
    }

    /// 空白 System 内容直接丢弃(不产生空的 system 块)。
    #[test]
    fn fold_system_messages_drops_blank_system() {
        let mut req = ChatRequest {
            model: "gpt-4o".into(),
            messages: vec![ChatMessage::System("   ".into()), user("hi")],
            ..Default::default()
        };
        req.fold_system_messages();
        assert!(req.system.0.is_empty());
        assert_eq!(req.messages.len(), 1);
    }
}
