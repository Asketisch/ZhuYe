//! `reasoning_effort` — 思考强度按模型分级的能力表(v1.10 功能4)。
//!
//! 不同模型族支持的思考强度上限不同:
//!
//! | 模型族 | 最高档 | wire 说明 |
//! |--------|--------|-----------|
//! | qwen 系列(`qwen3-*` / `qwen2.5-*`,走 OpenAI 兼容端点) | `high` | dashscope 兼容端点 `reasoning_effort` 最高 `high` |
//! | deepseek 系列(`deepseek-reasoner` 等) | `max` | 强度阶梯到 `max` |
//! | OpenAI(`gpt-5*` / `o1` / `o3` / `o4` / `o5`) | `ultra` | 官方 `reasoning_effort` 含 `ultra` 档 |
//! | 其余 / 未知模型 | `high` | 保守:多数 OpenAI 兼容端点只认识 low/medium/high |
//!
//! 钳制发生在**发 wire 之前**(`OpenAIClient` / `OpenAIResponsesClient`
//! 构造请求体时调 [`clamp_effort_for_model`]),用户侧槽位
//! (`AgentConfig.effort` / `Op::SetEffort` / `[model].reasoning_effort`)
//! 仍保存原始档位 —— 热切换模型后按新模型重新钳制,不需要回写槽位。
//!
//! 归一化口径与 [`crate::providers::context_window`] 一致:去 `provider/`
//! 前缀、去 ollama tag(`:7b`)、小写化,前缀匹配吞版本后缀。

use crate::request::ReasoningEffort;

/// 模型族前缀 → 最高支持档。**条目顺序即匹配顺序**(先命中先用),
/// 与 `CONTEXT_WINDOW_TABLE` 相同的 `starts_with` 策略。
const EFFORT_CAP_TABLE: &[(&str, ReasoningEffort)] = &[
    // ── qwen 系列:最高 high ─────────────────────────────────────────
    // (显式列出,虽然与默认档一致 —— 让「qwen 到 high」这条规则可被
    // 单测直接断言,后续若 qwen 开放更高档只改这一行。)
    ("qwen", ReasoningEffort::High),
    // ── deepseek 系列:最高 max ──────────────────────────────────────
    ("deepseek", ReasoningEffort::Max),
    // ── OpenAI 推理系:gpt-5 / o 系,最高 ultra ──────────────────────
    ("gpt-5", ReasoningEffort::Ultra),
    ("gpt-6", ReasoningEffort::Ultra),
    ("o1", ReasoningEffort::Ultra),
    ("o3", ReasoningEffort::Ultra),
    ("o4", ReasoningEffort::Ultra),
    ("o5", ReasoningEffort::Ultra),
];

/// 未知 / 未列模型族的保守上限:多数 OpenAI 兼容端点只认识
/// low / medium / high,给未知模型发 `max` / `ultra` 容易整请求被拒。
pub const DEFAULT_MAX_EFFORT: ReasoningEffort = ReasoningEffort::High;

/// 查 `model_spec` 支持的最高思考强度。归一化(去 provider 前缀 /
/// ollama tag、小写)后按前缀匹配;未知模型返回 [`DEFAULT_MAX_EFFORT`]。
///
/// # 示例
///
/// ```
/// use zhuye_llm::reasoning_effort::max_effort_for_model;
/// use zhuye_llm::ReasoningEffort;
///
/// assert_eq!(max_effort_for_model("qwen3-max"), ReasoningEffort::High);
/// assert_eq!(max_effort_for_model("deepseek-reasoner"), ReasoningEffort::Max);
/// assert_eq!(max_effort_for_model("openai/gpt-5.1"), ReasoningEffort::Ultra);
/// assert_eq!(max_effort_for_model("unknown-model"), ReasoningEffort::High);
/// ```
pub fn max_effort_for_model(model_spec: &str) -> ReasoningEffort {
    let needle = normalize(model_spec);
    if needle.is_empty() {
        return DEFAULT_MAX_EFFORT;
    }
    EFFORT_CAP_TABLE
        .iter()
        .find(|(prefix, _)| needle.starts_with(prefix))
        .map(|(_, cap)| *cap)
        .unwrap_or(DEFAULT_MAX_EFFORT)
}

/// 把请求档位钳制到目标模型支持的上限(取 `min`)。
///
/// # 示例
///
/// ```
/// use zhuye_llm::reasoning_effort::clamp_effort_for_model;
/// use zhuye_llm::ReasoningEffort;
///
/// // qwen 上限 high:ultra 被降到 high。
/// assert_eq!(
///     clamp_effort_for_model(ReasoningEffort::Ultra, "qwen3-235b"),
///     ReasoningEffort::High
/// );
/// // deepseek 上限 max:ultra 被降到 max,ultra 以下的档原样通过。
/// assert_eq!(
///     clamp_effort_for_model(ReasoningEffort::Ultra, "deepseek-chat"),
///     ReasoningEffort::Max
/// );
/// assert_eq!(
///     clamp_effort_for_model(ReasoningEffort::Max, "gpt-5.1"),
///     ReasoningEffort::Max
/// );
/// ```
pub fn clamp_effort_for_model(effort: ReasoningEffort, model_spec: &str) -> ReasoningEffort {
    effort.min(max_effort_for_model(model_spec))
}

/// 档位 → wire 字符串(OpenAI `reasoning_effort` 参数取值口径)。
pub fn effort_wire_str(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Max => "max",
        ReasoningEffort::Ultra => "ultra",
    }
}

/// 列出目标模型支持的完整档位阶梯(`Low..=cap`),供 CLI / TUI 展示
/// 「这个模型能选哪些档」,也供 `/effort` 类交互做参数校验。
pub fn supported_efforts_for_model(model_spec: &str) -> Vec<ReasoningEffort> {
    let cap = max_effort_for_model(model_spec);
    [
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::Max,
        ReasoningEffort::Ultra,
    ]
    .into_iter()
    .take_while(|e| *e <= cap)
    .collect()
}

/// 归一化:去 provider 前缀(`rsplit_once('/')` 取末段)、去 ollama tag
/// (`:7b`)、小写化 trim。与 `context_window_for` 的归一化三步一致。
fn normalize(model_spec: &str) -> String {
    let no_provider = model_spec
        .rsplit_once('/')
        .map(|(_, m)| m)
        .unwrap_or(model_spec);
    let no_tag = no_provider.split(':').next().unwrap_or(no_provider);
    no_tag.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::ReasoningEffort;

    #[test]
    fn caps_by_family() {
        // qwen 系列(带 provider 前缀 / tag / 大小写)→ high。
        assert_eq!(max_effort_for_model("qwen3-max"), ReasoningEffort::High);
        assert_eq!(
            max_effort_for_model("dashscope/qwen3-coder"),
            ReasoningEffort::High
        );
        assert_eq!(max_effort_for_model("Qwen2.5:72b"), ReasoningEffort::High);
        // deepseek → max。
        assert_eq!(
            max_effort_for_model("deepseek-reasoner"),
            ReasoningEffort::Max
        );
        assert_eq!(
            max_effort_for_model("deepseek/deepseek-chat"),
            ReasoningEffort::Max
        );
        // OpenAI → ultra。
        assert_eq!(max_effort_for_model("gpt-5.1"), ReasoningEffort::Ultra);
        assert_eq!(max_effort_for_model("openai/gpt-5"), ReasoningEffort::Ultra);
        assert_eq!(max_effort_for_model("o3-mini"), ReasoningEffort::Ultra);
        // 未知模型 → 保守 high。
        assert_eq!(max_effort_for_model("glm-4.7"), ReasoningEffort::High);
        assert_eq!(max_effort_for_model(""), ReasoningEffort::High);
    }

    #[test]
    fn clamp_takes_min() {
        assert_eq!(
            clamp_effort_for_model(ReasoningEffort::Ultra, "qwen3-max"),
            ReasoningEffort::High
        );
        assert_eq!(
            clamp_effort_for_model(ReasoningEffort::Ultra, "deepseek-chat"),
            ReasoningEffort::Max
        );
        assert_eq!(
            clamp_effort_for_model(ReasoningEffort::Ultra, "gpt-5.1"),
            ReasoningEffort::Ultra
        );
        // 低于上限的档位原样通过。
        assert_eq!(
            clamp_effort_for_model(ReasoningEffort::Low, "deepseek-reasoner"),
            ReasoningEffort::Low
        );
    }

    #[test]
    fn wire_str_ladder() {
        assert_eq!(effort_wire_str(ReasoningEffort::Low), "low");
        assert_eq!(effort_wire_str(ReasoningEffort::Medium), "medium");
        assert_eq!(effort_wire_str(ReasoningEffort::High), "high");
        assert_eq!(effort_wire_str(ReasoningEffort::Max), "max");
        assert_eq!(effort_wire_str(ReasoningEffort::Ultra), "ultra");
    }

    #[test]
    fn supported_ladder_lengths() {
        use crate::request::ReasoningEffort as E;
        // qwen:low..high = 3 档;deepseek:4 档;openai:5 档。
        assert_eq!(
            supported_efforts_for_model("qwen3-max"),
            vec![E::Low, E::Medium, E::High]
        );
        assert_eq!(
            supported_efforts_for_model("deepseek-chat"),
            vec![E::Low, E::Medium, E::High, E::Max]
        );
        assert_eq!(
            supported_efforts_for_model("gpt-5.1"),
            vec![E::Low, E::Medium, E::High, E::Max, E::Ultra]
        );
        // 未知模型与 qwen 同阶梯(保守 3 档)。
        assert_eq!(
            supported_efforts_for_model("mystery-model"),
            vec![E::Low, E::Medium, E::High]
        );
    }
}
