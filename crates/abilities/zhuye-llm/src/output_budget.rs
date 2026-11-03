//! # 输出上限自适应拟合(v1.9 C8)
//!
//! 与 oh-my-pi `output-budget.ts` 对齐:请求未显式指定 `max_tokens`
//! 时,按**模型上下文窗口**自适应推一个安全输出上限,而不是把决定
//! 交给 provider 服务端默认值(默认值偏小,长答案被截断)。
//!
//! ## 两个修正项(报告 1.2-C8)
//!
//! 1. **tokenizer 误差余量**:输入 token 是估算值(字符/4 级别的
//!    近似),真实分词可能多出 10%+。预算按
//!    `输入估算 × (1 + 误差余量)` 预留,保证
//!    `真实输入 + 输出上限 ≤ 窗口` 以高概率成立。
//! 2. **chat-template framing 计费**:每条消息的框架 token
//!    (role 标记、分隔符、tool schema 包裹)也占窗口,按
//!    `消息数 × 每条 overhead + 工具数 × tool overhead` 计入。
//!
//! ## 接线
//!
//! `providers::apply_request_param_defaults`(每个 provider client 的
//! `stream()` 入口)在「请求侧与 provider 段配置都未给 max_tokens」
//! 时调用 [`adaptive_max_tokens`];模型不在窗口表(`context_window_for
//! == None`,mock / 本地未知模型)时保持 `None`(行为不变)。

use crate::providers::context_window_for;
use crate::request::{ChatMessage, ChatRequest, ContentBlock, ToolSpec};

/// tokenizer 估算误差余量(输入估算 × (1+0.10) 预留)。
pub const TOKENIZER_ERROR_MARGIN: f64 = 0.10;
/// chat-template 每条消息框架 overhead(token)。
pub const CHAT_TEMPLATE_OVERHEAD_PER_MESSAGE: u32 = 8;
/// 每个工具定义的 schema 包裹 overhead(token)。
pub const TOOL_SCHEMA_OVERHEAD: u32 = 64;
/// 硬上限(输出预算永不超此值)。
pub const HARD_OUTPUT_CAP: u32 = 32_768;
/// 输出下限(再挤也要给模型保底口)。
pub const MIN_OUTPUT_FLOOR: u32 = 1024;

/// 文本 token 粗估:chars/4,向上取整(与 zhuye-compact 兜底估算同口径)。
fn estimate_text_tokens(text: &str) -> u32 {
    (text.chars().count() as u32).div_ceil(4)
}

/// 估算一次请求的输入侧 token(粗估,偏保守地偏小 —— 所以另有
/// 误差余量)。
///
/// 计入:system blocks 文本、全部消息的文本块、工具名 + 描述 +
/// schema JSON、每消息 / 每工具框架 overhead。
pub fn estimate_input_tokens(request: &ChatRequest) -> u32 {
    let mut total: u32 = 0;
    // system blocks。
    for b in &request.system.0 {
        total += estimate_text_tokens(&b.text);
    }
    // 消息(含各形态的文本面 + 框架 overhead)。
    for m in &request.messages {
        total += CHAT_TEMPLATE_OVERHEAD_PER_MESSAGE;
        match m {
            ChatMessage::System(s) => total += estimate_text_tokens(s),
            ChatMessage::User(u) => {
                for b in &u.blocks {
                    total += block_text_tokens(b);
                }
            }
            ChatMessage::Assistant(a) => {
                if let Some(t) = &a.text {
                    total += estimate_text_tokens(t);
                }
                if let Some(t) = &a.thinking {
                    total += estimate_text_tokens(t);
                }
                for c in &a.tool_calls {
                    // 调用框架 + 参数 JSON。
                    total += estimate_text_tokens(&c.name);
                    total += estimate_text_tokens(&c.arguments.to_string());
                }
            }
            ChatMessage::Tool(t) => {
                for b in &t.content {
                    total += block_text_tokens(b);
                }
            }
        }
    }
    // 工具 schema(name + description + parameters)。
    for t in &request.tools {
        total += TOOL_SCHEMA_OVERHEAD;
        match t {
            ToolSpec::Function {
                name,
                description,
                parameters,
            } => {
                total += estimate_text_tokens(name);
                total += estimate_text_tokens(description);
                total += estimate_text_tokens(&parameters.to_string());
            }
        }
    }
    total
}

fn block_text_tokens(b: &ContentBlock) -> u32 {
    match b {
        ContentBlock::Text { text } => estimate_text_tokens(text),
        // 图片按固定预算计(base64 体积巨大且方差大,粗估意义不大)。
        ContentBlock::Image { .. } => 1024,
    }
}

/// 自适应输出预算的完整分解(可观测)。
#[derive(Debug, Clone, PartialEq)]
pub struct OutputBudget {
    /// 最终建议 max_tokens。
    pub max_tokens: u32,
    /// 上下文窗口。
    pub context_window: u32,
    /// 输入侧估算(未加误差)。
    pub input_estimate: u32,
    /// 误差余量预留 token。
    pub error_margin_tokens: u32,
    /// 工具 schema 包裹 token(每消息框架已计入 input_estimate)。
    pub framing_tokens: u32,
    /// 预算被谁限制。
    pub clamped_by: BudgetClamp,
}

/// 预算限制来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetClamp {
    /// 硬上限(HARD_OUTPUT_CAP)。
    HardCap,
    /// 窗口余量(窗口 - 输入 - framing)。
    WindowHeadroom,
    /// 下限兜底(窗口严重不足时给 MIN_OUTPUT_FLOOR)。
    MinFloor,
}

/// 解析完整预算(带分解;测试与状态面板用)。
///
/// 语义:[`estimate_input_tokens`] 已含**每消息**框架 overhead,
/// `framing_tokens` 只单独报告**工具 schema** 的包裹开销。
pub fn resolve_output_budget(request: &ChatRequest, context_window: u32) -> OutputBudget {
    let input_estimate = estimate_input_tokens(request);
    let error_margin_tokens = (input_estimate as f64 * TOKENIZER_ERROR_MARGIN).ceil() as u32;
    let framing_tokens = TOOL_SCHEMA_OVERHEAD.saturating_mul(request.tools.len() as u32);

    let used = input_estimate
        .saturating_add(error_margin_tokens)
        .saturating_add(framing_tokens);
    let (max_tokens, clamped_by) = if used >= context_window {
        // 窗口已被输入吃满:给保底口,让模型至少能简短回应
        // (总溢出时 provider 自有报错路径)。
        (MIN_OUTPUT_FLOOR, BudgetClamp::MinFloor)
    } else {
        let headroom = context_window - used;
        if headroom >= HARD_OUTPUT_CAP {
            (HARD_OUTPUT_CAP, BudgetClamp::HardCap)
        } else {
            (headroom.max(MIN_OUTPUT_FLOOR), BudgetClamp::WindowHeadroom)
        }
    };
    OutputBudget {
        max_tokens,
        context_window,
        input_estimate,
        error_margin_tokens,
        framing_tokens,
        clamped_by,
    }
}

/// 便捷入口:模型在窗口表里才给预算,未知模型 `None`(行为不变)。
pub fn adaptive_max_tokens(request: &ChatRequest) -> Option<u32> {
    let window = context_window_for(&request.model)?;
    Some(resolve_output_budget(request, window).max_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{AssistantContent, SystemBlock, SystemBlocks, UserContent};

    fn user_msg(text: &str) -> ChatMessage {
        ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        })
    }

    fn assistant_call(name: &str, args_json: &str) -> ChatMessage {
        ChatMessage::Assistant(AssistantContent {
            text: None,
            tool_calls: vec![crate::request::ToolCallRequest {
                id: "c1".into(),
                name: name.into(),
                arguments: serde_json::from_str(args_json).expect("test args must be valid json"),
            }],
            thinking: None,
        })
    }

    #[test]
    fn estimate_counts_chars_and_framing() {
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            ..ChatRequest::default()
        };
        req.messages.push(user_msg(&"x".repeat(400)));
        // 400 chars / 4 = 100 + per-message 8。
        assert_eq!(
            estimate_input_tokens(&req),
            100 + CHAT_TEMPLATE_OVERHEAD_PER_MESSAGE
        );
    }

    #[test]
    fn estimate_counts_system_tools_and_calls() {
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(),
            system: SystemBlocks(vec![SystemBlock {
                text: "y".repeat(200),
                cache_control: None,
                ephemeral: false,
            }]),
            ..ChatRequest::default()
        };
        let _ = &req;
        req.messages.push(assistant_call(
            "bash",
            r#"{"cmd":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#,
        ));
        req.tools = vec![ToolSpec::Function {
            name: "bash".into(),
            description: "z".repeat(100),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let est = estimate_input_tokens(&req);
        // system 200/4=50;assistant 帧 8 + name 4/4=1 + args(88 chars/4=22)
        // = 31;tool: 64 + name(4/4=1) + desc(100/4=25) + params(~22/4=6) = 96。
        assert_eq!(est, 50 + 31 + 96, "实际 {est}");
    }

    #[test]
    fn unknown_model_yields_none() {
        let mut req = ChatRequest {
            model: "totally-unknown-model-xyz".into(),
            ..ChatRequest::default()
        };
        req.messages.push(user_msg("hi"));
        assert_eq!(
            adaptive_max_tokens(&req),
            None,
            "未知模型保持 None(行为不变)"
        );
    }

    #[test]
    fn known_model_gets_budget() {
        let mut req = ChatRequest {
            model: "claude-sonnet-4".into(), // 200k 窗口
            ..ChatRequest::default()
        };
        req.messages.push(user_msg(&"x".repeat(400)));
        let mt = adaptive_max_tokens(&req).expect("已知模型应有预算");
        // 小输入:headroom 巨大 → 硬上限 32768。
        assert_eq!(mt, HARD_OUTPUT_CAP);
    }

    #[test]
    fn budget_breakdown_is_consistent() {
        let mut req = ChatRequest {
            model: "gpt-4o".into(), // 128k
            ..ChatRequest::default()
        };
        for _ in 0..100 {
            req.messages.push(user_msg(&"x".repeat(400)));
        }
        let b = resolve_output_budget(&req, 128_000);
        // input_estimate 已含每消息框架:100 × (100 文本 + 8 帧) = 10800。
        assert_eq!(
            b.input_estimate,
            100 * (100 + CHAT_TEMPLATE_OVERHEAD_PER_MESSAGE)
        );
        assert_eq!(b.framing_tokens, 0, "无工具时 schema 包裹为 0");
        // error_margin_tokens 是**余量部分**(× margin),不是总预留;
        // 总预留 = input + margin = 10800 + 1080 = 11880。
        assert_eq!(
            b.error_margin_tokens,
            (b.input_estimate as f64 * TOKENIZER_ERROR_MARGIN).ceil() as u32
        );
        assert_eq!(b.clamped_by, BudgetClamp::HardCap);
        assert_eq!(b.max_tokens, HARD_OUTPUT_CAP);
    }

    #[test]
    fn near_full_window_falls_to_headroom_or_floor() {
        let mut req = ChatRequest {
            model: "gpt-4o".into(), // 128k
            ..ChatRequest::default()
        };
        // 500 条 ≈ 50k+ 输入 → headroom 仍巨大 → HardCap。
        for _ in 0..500 {
            req.messages.push(user_msg(&"x".repeat(400)));
        }
        let b = resolve_output_budget(&req, 128_000);
        assert_eq!(b.clamped_by, BudgetClamp::HardCap);
        // 1200 条 ≈ 120k+ → 余量极小 → WindowHeadroom / MinFloor。
        let mut big = ChatRequest {
            model: "gpt-4o".into(),
            ..ChatRequest::default()
        };
        for _ in 0..1200 {
            big.messages.push(user_msg(&"x".repeat(400)));
        }
        let b2 = resolve_output_budget(&big, 128_000);
        assert!(
            matches!(
                b2.clamped_by,
                BudgetClamp::WindowHeadroom | BudgetClamp::MinFloor
            ),
            "近满窗口应落入余量/保底档,实际 {b2:?}"
        );
        assert!(b2.max_tokens < HARD_OUTPUT_CAP);
    }

    #[test]
    fn overfull_window_floors_at_min() {
        let mut req = ChatRequest {
            model: "gpt-4o".into(), // 128k
            ..ChatRequest::default()
        };
        for _ in 0..2000 {
            req.messages.push(user_msg(&"x".repeat(400)));
        }
        let b = resolve_output_budget(&req, 128_000);
        assert_eq!(b.clamped_by, BudgetClamp::MinFloor);
        assert_eq!(b.max_tokens, MIN_OUTPUT_FLOOR);
    }

    #[test]
    fn estimate_text_tokens_rounds_up() {
        assert_eq!(estimate_text_tokens("a"), 1);
        assert_eq!(estimate_text_tokens(&"a".repeat(4)), 1);
        assert_eq!(estimate_text_tokens(&"a".repeat(5)), 2);
        assert_eq!(estimate_text_tokens(""), 0);
    }
}
