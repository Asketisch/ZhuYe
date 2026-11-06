//! # 输出上限自适应拟合(v1.9 C8)
//!
//! 与 oh-my-pi `output-budget.ts` 对齐:请求未显式指定 `max_tokens`
//! 时,按**模型上下文窗口**自适应推一个安全输出上限,而不是把决定
//! 交给 provider 服务端默认值(默认值偏小,长答案被截断)。
//!
//! ## 两个修正项(报告 1.2-C8)
//!
//! 1. **tokenizer 误差余量**:输入 token 是近似估算,预算按
//!    `输入估算 × (1 + 误差余量)` 预留;实际分词和图片 token
//!    数依模型而异,仍可能超过窗口。
//! 2. **chat-template framing 计费**:每条消息的框架 token
//!    (role 标记、分隔符、tool schema 包裹)也占窗口,按
//!    `消息数 × 每条 overhead + 工具数 × tool overhead` 计入。
//!
//! ## 接线
//!
//! `providers::apply_request_param_defaults`(每个 provider client 的
//! `stream()` 入口)在「请求侧与 provider 段配置都未给 max_tokens」
//! 时调用 [`adaptive_max_tokens`];模型的上下文窗口或输出上限未知时
//! 保持 `None`,沿用 provider 原有默认值。

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

/// 文本 token 粗估:ASCII 字节按四分之一,非 ASCII 字节按二分之一。
/// 中日韩文字和表情不能按每四个字符一个 token 估算。
fn estimate_text_tokens(text: &str) -> u32 {
    let ascii = text.bytes().filter(u8::is_ascii).count();
    let non_ascii = text.len() - ascii;
    let estimate = ascii.div_ceil(4).saturating_add(non_ascii.div_ceil(2));
    u32::try_from(estimate).unwrap_or(u32::MAX)
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
    /// 工具 schema 包裹 token;已包含在 `input_estimate` 中,仅供观察。
    pub framing_tokens: u32,
    /// 预算被谁限制。
    pub clamped_by: BudgetClamp,
}

/// 预算限制来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetClamp {
    /// 全局硬上限或模型输出上限。
    HardCap,
    /// 窗口余量(窗口 - 输入估算 - 误差余量)。
    WindowHeadroom,
    /// 下限兜底(窗口严重不足时给 MIN_OUTPUT_FLOOR)。
    MinFloor,
}

/// 解析完整预算(带分解;测试与状态面板用)。
///
/// [`estimate_input_tokens`] 已含消息和工具的框架开销;
/// `framing_tokens` 单独报告工具部分,不再重复计入。
pub fn resolve_output_budget(request: &ChatRequest, context_window: u32) -> OutputBudget {
    let input_estimate = estimate_input_tokens(request);
    let error_margin_tokens = (input_estimate as f64 * TOKENIZER_ERROR_MARGIN).ceil() as u32;
    let framing_tokens = TOOL_SCHEMA_OVERHEAD.saturating_mul(request.tools.len() as u32);

    let used = input_estimate.saturating_add(error_margin_tokens);
    let output_cap = model_output_limit(&request.model)
        .unwrap_or(HARD_OUTPUT_CAP)
        .min(HARD_OUTPUT_CAP);
    let (max_tokens, clamped_by) = if used >= context_window {
        // 窗口已被输入吃满:给保底口,让模型至少能简短回应
        // (总溢出时 provider 自有报错路径)。
        (MIN_OUTPUT_FLOOR.min(output_cap), BudgetClamp::MinFloor)
    } else {
        let headroom = context_window - used;
        if headroom >= output_cap {
            (output_cap, BudgetClamp::HardCap)
        } else {
            (headroom, BudgetClamp::WindowHeadroom)
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

/// 便捷入口:只有窗口与输出上限均已知时才生成自动预算。
///
/// v1.10 功能3:窗口来源放宽 —— 用户在 config.toml 配了上下文窗口
///(`[context_windows]` / `[model].context_window`,即运行时 override 表
/// 命中)的模型,**输出上限未知时以 [`HARD_OUTPUT_CAP`] 兜底**也生成
/// 预算。此前私端 / 新模型两边都查不到 → `None` → 输出上限交给
/// provider 服务端默认(普遍偏小,长答案被截);现在只要窗口可配,
/// 预算就能按 `min(硬上限, 窗口余量)` 自适应。静态表已知但输出上限
/// 未知的模型(如 o3-mini 之外的多数条目)仍维持 `None`(行为不变)。
pub fn adaptive_max_tokens(request: &ChatRequest) -> Option<u32> {
    let window = context_window_for(&request.model)?;
    // 输出上限来源:精确输出上限表;窗口来自用户配置(override 命中)时
    // 视为可用(resolve_output_budget 内部以 HARD_OUTPUT_CAP 兜底)。
    let has_output_cap = model_output_limit(&request.model).is_some()
        || crate::providers::context_window_override_for(&request.model).is_some();
    if !has_output_cap {
        return None;
    }
    Some(resolve_output_budget(request, window).max_tokens)
}

/// 精确匹配已知模型和快照,避免把未知版本误判为同一输出上限。
fn model_output_limit(model: &str) -> Option<u32> {
    let model = model
        .rsplit('/')
        .next()?
        .split(':')
        .next()?
        .trim()
        .to_ascii_lowercase();
    match model.as_str() {
        "gpt-4o"
        | "gpt-4o-2024-08-06"
        | "gpt-4o-2024-11-20"
        | "gpt-4o-mini"
        | "gpt-4o-mini-2024-07-18" => Some(16_384),
        "gpt-4o-2024-05-13" | "gpt-4-turbo" | "gpt-4-turbo-2024-04-09" => Some(4_096),
        "gpt-4" | "gpt-4-0314" | "gpt-4-0613" => Some(8_192),
        "o3-mini" | "o3-mini-2025-01-31" => Some(32_768),
        "claude-3-5-sonnet" | "claude-3-5-sonnet-20240620" | "claude-3-5-sonnet-20241022" => {
            Some(8_192)
        }
        "claude-3-5-haiku"
        | "claude-3-5-haiku-20241022"
        | "claude-3-opus"
        | "claude-3-opus-20240229"
        | "claude-3-haiku"
        | "claude-3-haiku-20240307" => Some(4_096),
        "claude-sonnet-4"
        | "claude-sonnet-4-20250514"
        | "claude-sonnet-4-latest"
        | "claude-opus-4"
        | "claude-opus-4-20250514"
        | "claude-opus-4-latest" => Some(32_768),
        _ => None,
    }
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
        assert_eq!(b.max_tokens, 16_384);
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
        assert_eq!(estimate_text_tokens("中文"), 3);
        assert_eq!(estimate_text_tokens("a中"), 3);
        assert_eq!(estimate_text_tokens("😀"), 2);
        assert_eq!(estimate_text_tokens(""), 0);
    }

    #[test]
    fn chinese_input_reduces_available_output_budget() {
        let request = ChatRequest {
            model: "gpt-4".into(),
            messages: vec![user_msg(&"中".repeat(3_000))],
            ..ChatRequest::default()
        };
        assert_eq!(estimate_input_tokens(&request), 4_508);
        assert_eq!(adaptive_max_tokens(&request), Some(3_233));
    }

    #[test]
    fn automatic_budget_respects_known_model_output_limits() {
        for (model, expected) in [
            ("openai/gpt-4o", 16_384),
            ("gpt-4o-2024-05-13", 4_096),
            ("gpt-4o-mini", 16_384),
            ("gpt-4-turbo", 4_096),
            ("claude-3-5-sonnet-20241022", 8_192),
            ("claude-3-haiku-20240307", 4_096),
        ] {
            let request = ChatRequest {
                model: model.into(),
                ..ChatRequest::default()
            };
            assert_eq!(adaptive_max_tokens(&request), Some(expected), "{model}");
        }
        let unknown_snapshot = ChatRequest {
            model: "gpt-4o-future-snapshot".into(),
            ..ChatRequest::default()
        };
        assert_eq!(adaptive_max_tokens(&unknown_snapshot), None);
    }

    #[test]
    fn tool_overhead_is_counted_once_and_small_headroom_is_respected() {
        let request = ChatRequest {
            model: "unlisted".into(),
            tools: vec![ToolSpec::Function {
                name: "tool".into(),
                description: "description".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            ..ChatRequest::default()
        };
        let input = estimate_input_tokens(&request);
        let margin = (input as f64 * TOKENIZER_ERROR_MARGIN).ceil() as u32;
        let budget = resolve_output_budget(&request, input + margin + 100);
        assert_eq!(budget.framing_tokens, TOOL_SCHEMA_OVERHEAD);
        assert_eq!(budget.max_tokens, 100);
        assert_eq!(budget.clamped_by, BudgetClamp::WindowHeadroom);
    }

    // ── v1.10 功能3:配置了上下文窗口的私端模型也生成自适应预算 ─────

    #[test]
    fn configured_context_window_unlocks_budget_for_custom_model() {
        use crate::set_context_window_overrides;
        // 全局 override 表是进程级共享:与 context_window.rs 里的 override
        // 单测互斥,避免 set/clear 互相踩;模型名全局唯一防泄漏误伤。
        let _guard = crate::providers::context_window::override_test_lock()
            .lock()
            .unwrap();
        let mut req = ChatRequest {
            model: "zzob-custom-endpoint-model".into(),
            ..ChatRequest::default()
        };
        req.messages.push(user_msg("hi"));
        // 未配置时:窗口未知 → None(行为不变)。
        set_context_window_overrides(&std::collections::HashMap::new());
        assert_eq!(
            adaptive_max_tokens(&req),
            None,
            "未配置窗口的未知模型保持 None"
        );

        // 配置窗口后:窗口已知 → 输出上限以 HARD_OUTPUT_CAP 兜底。
        let mut entries = std::collections::HashMap::new();
        entries.insert("zzob-custom-endpoint-model".to_string(), 32_768u32);
        set_context_window_overrides(&entries);
        let mt = adaptive_max_tokens(&req).expect("配置窗口后应有预算");
        assert!(
            mt > 0 && mt <= HARD_OUTPUT_CAP,
            "预算落在 (0, HARD_OUTPUT_CAP],实际 {mt}"
        );
        // 清理全局表。
        set_context_window_overrides(&std::collections::HashMap::new());
    }
}
