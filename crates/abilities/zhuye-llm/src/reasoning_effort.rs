//! `reasoning_effort` — 思考强度按模型分级的能力表(v1.10 功能4)。
//!
//! 不同模型族支持的思考强度上限不同:
//!
//! | 模型族 | 最高档 | wire 说明 |
//! |--------|--------|-----------|
//! | qwen3.8 系列(`qwen3.8-*`) | `xhigh` | 厂商词表 `low/medium/xhigh`,顶档 `xhigh` |
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
/// 与 `CONTEXT_WINDOW_TABLE` 相同的 `starts_with` 策略;更长前缀必须排在
/// 更短前缀之前(`qwen3.8` 在 `qwen` 前)。
const EFFORT_CAP_TABLE: &[(&str, ReasoningEffort)] = &[
    // ── qwen3.8 系列:最高 xhigh ─────────────────────────────────────
    // (厂商词表 low/medium/xhigh,必须排在泛 `qwen` 条目之前 ——
    // starts_with 先命中先用,否则被 qwen→high 短路。)
    ("qwen3.8", ReasoningEffort::XHigh),
    // ── qwen 系列(旧代):最高 high ─────────────────────────────────
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
/// ollama tag、小写)后三级级联:
///
/// 1. 内置静态表(前缀匹配)—— 编码了本引擎对各模型族 wire 词表的
///    决策,权威优先;
/// 2. models.dev 社区目录的档位投票(`crate::models_dev` 的
///    `effort_levels`,把厂商档位词映射到本阶梯后取最大)—— 静态表
///    未收录的新模型 / 私端模型由此获得真实档位,而不是一律保守 high;
/// 3. 都未命中返回 [`DEFAULT_MAX_EFFORT`]。
///
/// # 示例
///
/// ```
/// use zhuye_llm::reasoning_effort::max_effort_for_model;
/// use zhuye_llm::ReasoningEffort;
///
/// assert_eq!(max_effort_for_model("qwen3-max"), ReasoningEffort::High);
/// assert_eq!(max_effort_for_model("qwen3.8-flash"), ReasoningEffort::XHigh);
/// assert_eq!(max_effort_for_model("deepseek-reasoner"), ReasoningEffort::Max);
/// assert_eq!(max_effort_for_model("openai/gpt-5.1"), ReasoningEffort::Ultra);
/// assert_eq!(max_effort_for_model("unknown-model"), ReasoningEffort::High);
/// ```
pub fn max_effort_for_model(model_spec: &str) -> ReasoningEffort {
    let needle = normalize(model_spec);
    if needle.is_empty() {
        return DEFAULT_MAX_EFFORT;
    }
    if let Some((_, cap)) = EFFORT_CAP_TABLE
        .iter()
        .find(|(prefix, _)| needle.starts_with(prefix))
    {
        return *cap;
    }
    if let Some(levels) = crate::models_dev::abilities_for(model_spec).effort_levels {
        if let Some(cap) = ladder_cap(&levels) {
            return cap;
        }
    }
    DEFAULT_MAX_EFFORT
}

/// 厂商档位词表 → 本阶梯的最大档。只认 low/medium/high/xhigh/max/ultra
/// 六个词;`none` / `minimal` / 未知词不参与取最大(它们不是「更高的档」)。
fn ladder_cap(levels: &[String]) -> Option<ReasoningEffort> {
    levels
        .iter()
        .filter_map(|s| match s.to_ascii_lowercase().as_str() {
            "low" => Some(ReasoningEffort::Low),
            "medium" => Some(ReasoningEffort::Medium),
            "high" => Some(ReasoningEffort::High),
            "xhigh" => Some(ReasoningEffort::XHigh),
            "max" => Some(ReasoningEffort::Max),
            "ultra" => Some(ReasoningEffort::Ultra),
            _ => None,
        })
        .max()
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
        ReasoningEffort::XHigh => "xhigh",
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
        ReasoningEffort::XHigh,
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
    let no_provider = model_spec.rsplit_once('/').map_or(model_spec, |(_, m)| m);
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
        // qwen3.8 系列:xhigh(静态表长前缀必须压过泛 qwen 条目)。
        assert_eq!(max_effort_for_model("qwen3.8-flash"), ReasoningEffort::XHigh);
        assert_eq!(
            max_effort_for_model("anthropic/qwen3.8-flash-next"),
            ReasoningEffort::XHigh
        );
        assert_eq!(max_effort_for_model("QWEN3.8-27B-NVFP4"), ReasoningEffort::XHigh);
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
        // 未知模型 → 保守 high(用假模型名断言,不依赖目录数据)。
        assert_eq!(
            max_effort_for_model("zzcap-unknown-9x"),
            ReasoningEffort::High
        );
        assert_eq!(max_effort_for_model(""), ReasoningEffort::High);
    }

    #[test]
    fn clamp_takes_min() {
        assert_eq!(
            clamp_effort_for_model(ReasoningEffort::Ultra, "qwen3-max"),
            ReasoningEffort::High
        );
        // qwen3.8 上限 xhigh:ultra / max 都降到 xhigh(选 max 的用户
        // 实际收到厂商词表顶档,而不是不认识的高档词)。
        assert_eq!(
            clamp_effort_for_model(ReasoningEffort::Max, "qwen3.8-flash"),
            ReasoningEffort::XHigh
        );
        assert_eq!(
            clamp_effort_for_model(ReasoningEffort::XHigh, "qwen3-max"),
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
        assert_eq!(effort_wire_str(ReasoningEffort::XHigh), "xhigh");
        assert_eq!(effort_wire_str(ReasoningEffort::Max), "max");
        assert_eq!(effort_wire_str(ReasoningEffort::Ultra), "ultra");
    }

    #[test]
    fn supported_ladder_lengths() {
        use crate::request::ReasoningEffort as E;
        // qwen:low..high = 3 档;qwen3.8:+xhigh = 4 档;deepseek:5 档;
        // openai:6 档。
        assert_eq!(
            supported_efforts_for_model("qwen3-max"),
            vec![E::Low, E::Medium, E::High]
        );
        assert_eq!(
            supported_efforts_for_model("qwen3.8-flash-next"),
            vec![E::Low, E::Medium, E::High, E::XHigh]
        );
        assert_eq!(
            supported_efforts_for_model("deepseek-chat"),
            vec![E::Low, E::Medium, E::High, E::XHigh, E::Max]
        );
        assert_eq!(
            supported_efforts_for_model("gpt-5.1"),
            vec![E::Low, E::Medium, E::High, E::XHigh, E::Max, E::Ultra]
        );
        // 未知模型与 qwen 同阶梯(保守 3 档)。
        assert_eq!(
            supported_efforts_for_model("zzladder-mystery"),
            vec![E::Low, E::Medium, E::High]
        );
    }

    /// 目录档位投票补未知模型族:静态表未收录时,models.dev 的档位
    /// 词表映射到本阶梯取最大;词表外的词(none/minimal)不抬升上限。
    /// install_catalog 是进程级全局,与 models_dev / context_window 的
    /// 目录单测互斥,用各自独有的假模型名避免串扰。
    #[test]
    fn catalog_levels_fill_unknown_families() {
        use crate::models_dev::{DevCatalog, ModelAbilities, install_catalog};
        let _guard = crate::models_dev::catalog_test_lock().lock().unwrap();
        install_catalog(DevCatalog::from_entries([(
            "zzeff-new-model".to_string(),
            ModelAbilities {
                effort_levels: Some(vec![
                    "low".into(),
                    "medium".into(),
                    "high".into(),
                    "max".into(),
                ]),
                ..Default::default()
            },
        )]));
        assert_eq!(
            max_effort_for_model("zzeff-new-model"),
            ReasoningEffort::Max,
            "目录说支持到 max → 上限从保守 high 升到 max"
        );
        assert_eq!(supported_efforts_for_model("zzeff-new-model").len(), 5);
        // 目录词表含 xhigh → 上限 xhigh(qwen3.8 之外新厂商的通用通道)。
        install_catalog(DevCatalog::from_entries([(
            "zzeff-xhigh-model".to_string(),
            ModelAbilities {
                effort_levels: Some(vec!["low".into(), "medium".into(), "xhigh".into()]),
                ..Default::default()
            },
        )]));
        assert_eq!(
            max_effort_for_model("zzeff-xhigh-model"),
            ReasoningEffort::XHigh,
            "目录 xhigh 词必须映射进阶梯,不再被静默丢弃"
        );
        // 词表外的词不参与取最大 → 仍走保守默认。
        install_catalog(DevCatalog::from_entries([(
            "zzeff-odd-vocab".to_string(),
            ModelAbilities {
                effort_levels: Some(vec!["none".into(), "minimal".into()]),
                ..Default::default()
            },
        )]));
        assert_eq!(max_effort_for_model("zzeff-odd-vocab"), DEFAULT_MAX_EFFORT);
        // 静态表权威优先:目录就算给了更高档,已收录模型族不被覆盖。
        install_catalog(DevCatalog::from_entries([(
            "qwen3-max".to_string(),
            ModelAbilities {
                effort_levels: Some(vec!["low".into(), "ultra".into()]),
                ..Default::default()
            },
        )]));
        assert_eq!(
            max_effort_for_model("qwen3-max"),
            ReasoningEffort::High,
            "静态表 qwen → high 压过目录的 ultra"
        );
        install_catalog(DevCatalog::default());
    }
}
