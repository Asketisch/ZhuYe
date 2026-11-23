//! `catalog` —— 工具×模型两级提示词目录(`sources.json` 的 `tools` 段)。
//!
//! 结构:工具(`PromptTool`,一级菜单)→ 模型变体(`PromptVariant`,二级
//! 菜单)。变体分两类:
//! - **实体变体**:带 `file`(`prompts/tools/<tool>/<id>.md`,编译期内嵌于
//!   [`EMBEDDED_TOOL_PROMPTS`])—— 多模型工具(如 Claude Code / Codex)各
//!   模型策展文案,选中后按 default → 家族层 → 变体三级叠加;
//! - **回引变体**:带 `profile_ref`(复用既有厂商层,不新增文件)—— 单文件
//!   工具(如 gemini-cli / deepseek)的叶项,选中即选中原厂商层,解析走
//!   library 的平铺路径。
//!
//! 三条访问路径:
//! - [`tool_catalog()`]:菜单/`PromptListEvent.groups` 用的分组视图;
//! - [`tool_variant`] / [`tool_variant_text`]:按变体 id 查实体变体,供
//!   `resolve_selection` 做家族叠加;
//! - [`auto_profile()`]:「智能匹配模型」的引擎端实现 —— 模型 spec 归一化
//!   (小写、`_`/`.`→`-`)后精确 `models` 优先,其次最长 `model_prefixes`;
//!   同级已过评测(`evaluation:passed`)优先,再按清单顺序。
//!
//! 目录解析失败(清单损坏)时按空目录处理:菜单退化为平铺厂商层、auto
//! 回落 default —— 与旧版行为一致,不影响可用性。

use std::sync::OnceLock;

use zhuye_protocol::{PromptGroup, PromptVariantRef};

use crate::library::split_spec;

/// 工具变体内嵌文案:`(变体 id, 英文策展层全文)`。id 与 sources.json
/// `tools[].variants[].id` 及文件名严格一致(有测试锁定双向同步);
/// 文件头 HTML 注释记录来源路径、sha256 与策展规则。
#[rustfmt::skip]
pub const EMBEDDED_TOOL_PROMPTS: &[(&str, &str)] = &[
    // ── claude-code ──
    ("claude-code-opus-4.6", include_str!("../prompts/tools/claude-code/claude-code-opus-4.6.md")),
    ("claude-code-opus-4.7", include_str!("../prompts/tools/claude-code/claude-code-opus-4.7.md")),
    ("claude-code-opus-4.8", include_str!("../prompts/tools/claude-code/claude-code-opus-4.8.md")),
    ("claude-code-opus-5", include_str!("../prompts/tools/claude-code/claude-code-opus-5.md")),
    ("claude-code-opus-5.5", include_str!("../prompts/tools/claude-code/claude-code-opus-5.5.md")),
    ("claude-code-sonnet-4.6", include_str!("../prompts/tools/claude-code/claude-code-sonnet-4.6.md")),
    ("claude-code-sonnet-5", include_str!("../prompts/tools/claude-code/claude-code-sonnet-5.md")),
    ("claude-code-sonnet-5.5", include_str!("../prompts/tools/claude-code/claude-code-sonnet-5.5.md")),
    ("claude-code-haiku-4.5", include_str!("../prompts/tools/claude-code/claude-code-haiku-4.5.md")),
    ("claude-code-haiku-5.5", include_str!("../prompts/tools/claude-code/claude-code-haiku-5.5.md")),
    ("claude-code-fable-5", include_str!("../prompts/tools/claude-code/claude-code-fable-5.md")),
    ("claude-code-fable-5.1", include_str!("../prompts/tools/claude-code/claude-code-fable-5.1.md")),
    ("claude-code-fable-5.2", include_str!("../prompts/tools/claude-code/claude-code-fable-5.2.md")),
    // ── codex ──
    ("codex-gpt-5.6", include_str!("../prompts/tools/codex/codex-gpt-5.6.md")),
    ("codex-gpt-6-astra", include_str!("../prompts/tools/codex/codex-gpt-6-astra.md")),
    ("codex-gpt-6-luna", include_str!("../prompts/tools/codex/codex-gpt-6-luna.md")),
    ("codex-gpt-6-sol", include_str!("../prompts/tools/codex/codex-gpt-6-sol.md")),
    ("codex-gpt-6.1-sol", include_str!("../prompts/tools/codex/codex-gpt-6.1-sol.md")),
    ("codex-gpt-6.1-terra", include_str!("../prompts/tools/codex/codex-gpt-6.1-terra.md")),
    ("codex-gpt-6.2-astra", include_str!("../prompts/tools/codex/codex-gpt-6.2-astra.md")),
    ("codex-gpt-6.2-sol", include_str!("../prompts/tools/codex/codex-gpt-6.2-sol.md")),
    // ── kimi ──
    ("kimi-2.6", include_str!("../prompts/tools/kimi/kimi-2.6.md")),
    ("kimi-3", include_str!("../prompts/tools/kimi/kimi-3.md")),
    // ── qwen ──
    ("qwen3.6-plus", include_str!("../prompts/tools/qwen/qwen3.6-plus.md")),
    ("qwen3.8-max", include_str!("../prompts/tools/qwen/qwen3.8-max.md")),
    ("qwen4.0-max", include_str!("../prompts/tools/qwen/qwen4.0-max.md")),
];

/// 一个工具(一级菜单项)及其模型变体(二级菜单项)。
#[derive(Debug, Clone)]
pub struct PromptTool {
    /// 工具 id(如 `claude-code`)。
    pub id: String,
    /// 显示名(如 `Claude Code`;专有名词不翻译)。
    pub label: String,
    /// 变体解析时叠加的家族层(厂商层);叶项与家族层同名时只应用一次。
    pub family_layer: String,
    /// 模型变体,清单顺序即菜单与 auto 匹配的次优序。
    pub variants: Vec<PromptVariant>,
}

/// 一个模型变体。`file`(实体变体)与 `profile_ref`(回引既有厂商层)
/// 二选一,均缺省视为清单损坏、跳过。
#[derive(Debug, Clone)]
pub struct PromptVariant {
    /// 变体 id(全局唯一,即层文件名 stem;Manual 选择的 profile 值)。
    pub id: String,
    /// 显示名(如 `Opus 4.6`)。
    pub label: String,
    /// 精确模型名(归一化后全等比对)。
    pub models: Vec<String>,
    /// 家族前缀(归一化后前缀比对,最长优先)。
    pub model_prefixes: Vec<String>,
    /// 评测状态:`passed` 的变体在 auto 同分竞争时优先。
    pub evaluation: String,
    /// 实体变体的内嵌文件路径(相对 `prompts/`)。
    pub file: Option<String>,
    /// 回引变体指向的既有厂商层 id。
    pub profile_ref: Option<String>,
}

/// 解析 sources.json 的 `tools` 段为进程级静态目录。解析失败按空处理。
fn tools() -> &'static [PromptTool] {
    static TOOLS: OnceLock<Vec<PromptTool>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        let manifest: serde_json::Value =
            serde_json::from_str(include_str!("../prompts/sources.json")).unwrap_or_default();
        let Some(entries) = manifest.get("tools").and_then(|v| v.as_array()) else {
            tracing::warn!("prompt catalog: sources.json 缺少 tools 段,两级菜单退化为平铺");
            return Vec::new();
        };
        entries
            .iter()
            .filter_map(|entry| {
                let id = entry.get("id")?.as_str()?.to_string();
                let label = entry
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&id)
                    .to_string();
                let family_layer = entry
                    .get("family_layer")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&id)
                    .to_string();
                let variants = entry
                    .get("variants")?
                    .as_array()?
                    .iter()
                    .filter_map(|v| {
                        let id = v.get("id")?.as_str()?.to_string();
                        Some(PromptVariant {
                            label: v
                                .get("label")
                                .and_then(|l| l.as_str())
                                .unwrap_or(&id)
                                .to_string(),
                            id,
                            models: string_list(v.get("models")),
                            model_prefixes: string_list(v.get("model_prefixes")),
                            evaluation: v
                                .get("evaluation")
                                .and_then(|e| e.as_str())
                                .unwrap_or("unverified")
                                .to_string(),
                            file: v.get("file").and_then(|f| f.as_str()).map(str::to_string),
                            profile_ref: v
                                .get("profile_ref")
                                .and_then(|p| p.as_str())
                                .map(str::to_string),
                        })
                    })
                    .collect();
                Some(PromptTool {
                    id,
                    label,
                    family_layer,
                    variants,
                })
            })
            .collect()
    })
}

/// 可选字符串数组字段(`null`/缺失 → 空)。
fn string_list(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map_or_else(Vec::new, |items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::to_string)
                .collect()
        })
}

/// 模型名归一化:小写、`_`/`.` 统一为 `-`,使 `claude-opus-4-6` 与
/// `claude-opus-4.6` 等价。
fn normalize(name: &str) -> String {
    name.to_ascii_lowercase().replace(['_', '.'], "-")
}

/// 按变体 id 查**实体变体**(带内嵌文件);回引变体走 library 平铺路径,
/// 不在此命中。
pub fn tool_variant(id: &str) -> Option<(&'static PromptTool, &'static PromptVariant)> {
    tools().iter().find_map(|tool| {
        tool.variants
            .iter()
            .find(|v| v.id == id && v.file.is_some())
            .map(|v| (tool, v))
    })
}

/// 实体变体的内嵌文案。
pub fn tool_variant_text(id: &str) -> Option<&'static str> {
    EMBEDDED_TOOL_PROMPTS
        .iter()
        .find(|(name, _)| *name == id)
        .map(|(_, text)| *text)
}

/// 两级菜单/`PromptListEvent.groups` 的分组视图(含回引叶项)。
pub fn tool_catalog() -> Vec<PromptGroup> {
    tools()
        .iter()
        .map(|tool| PromptGroup {
            id: tool.id.clone(),
            label: tool.label.clone(),
            family_layer: tool.family_layer.clone(),
            variants: tool
                .variants
                .iter()
                .map(|v| PromptVariantRef {
                    id: v.id.clone(),
                    label: v.label.clone(),
                })
                .collect(),
        })
        .collect()
}

/// 目录内全部变体 id(`available_layers` 用,回引项与平铺层天然去重)。
pub fn catalog_variant_ids() -> Vec<String> {
    tools()
        .iter()
        .flat_map(|tool| tool.variants.iter().map(|v| v.id.clone()))
        .collect()
}

/// 变体所属工具的家族层 id(供 `resolve_selection` 叠加;未命中 → None)。
pub fn variant_family_layer(id: &str) -> Option<&'static str> {
    tool_variant(id).map(|(tool, _)| tool.family_layer.as_str())
}

/// 「智能匹配模型」:按模型 spec 在目录中选最优变体。
///
/// 评分序:精确 `models` 命中 > 最长 `model_prefixes` 命中;同级
/// `evaluation:passed` 优先;再同则清单顺序(先登记的工具/变体优先)。
/// 无命中返回 None,调用方回落 default 并给出 fallback_reason。
pub fn auto_profile(spec: &str) -> Option<String> {
    let (_, model) = split_spec(spec);
    let model = normalize(&model);
    // 候选比较键(元组越大越优):kind(2=精确 > 1=前缀)→ 前缀长度
    // → passed → 清单序(Reverse 使登记在前的优先)。
    type Score = (u8, usize, bool, std::cmp::Reverse<(usize, usize)>);
    let mut best: Option<(Score, String)> = None;
    for (tool_index, tool) in tools().iter().enumerate() {
        for (variant_index, variant) in tool.variants.iter().enumerate() {
            // 精确优先;否则取最长命中前缀。
            let matched = if variant.models.iter().any(|name| normalize(name) == model) {
                Some((2u8, 0usize))
            } else {
                variant
                    .model_prefixes
                    .iter()
                    .map(|p| normalize(p))
                    .filter(|p| model.starts_with(p.as_str()))
                    .max_by_key(|p| p.len())
                    .map(|p| (1u8, p.len()))
            };
            let Some((kind, prefix_len)) = matched else {
                continue;
            };
            let score: Score = (
                kind,
                prefix_len,
                variant.evaluation == "passed",
                std::cmp::Reverse((tool_index, variant_index)),
            );
            if best.as_ref().is_none_or(|(current, _)| score > *current) {
                best = Some((score, variant.id.clone()));
            }
        }
    }
    best.map(|(_, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_parses_with_expected_shape() {
        let groups = tool_catalog();
        assert_eq!(groups.len(), 9, "9 个工具组,实际: {groups:?}");
        let claude = groups.iter().find(|g| g.id == "claude-code").unwrap();
        assert_eq!(claude.label, "Claude Code");
        assert_eq!(claude.variants.len(), 13);
        assert_eq!(claude.variants[0].id, "claude-code-opus-4.6");
        assert_eq!(claude.variants[0].label, "Opus 4.6");
        // 叶项组恰一个回引变体。
        let glm = groups.iter().find(|g| g.id == "glm").unwrap();
        assert_eq!(glm.variants.len(), 1);
        assert_eq!(glm.variants[0].id, "glm");
    }

    /// sources.json 的实体变体与内嵌注册表必须双向同步 —— 清单登记了
    /// 文件而注册表漏 include_str!(或反向)都会在这里红。
    #[test]
    fn embedded_registry_matches_catalog_files() {
        let filed: Vec<&str> = tools()
            .iter()
            .flat_map(|tool| tool.variants.iter())
            .filter(|v| v.file.is_some())
            .map(|v| v.id.as_str())
            .collect();
        let embedded: Vec<&str> = EMBEDDED_TOOL_PROMPTS.iter().map(|(id, _)| *id).collect();
        assert_eq!(filed.len(), embedded.len(), "实体变体数应与注册表一致");
        for id in &filed {
            assert!(
                embedded.contains(id),
                "变体 `{id}` 在清单有 file 但注册表缺 include_str!"
            );
        }
        for id in &embedded {
            assert!(filed.contains(id), "注册表条目 `{id}` 不在清单的实体变体里");
        }
    }

    #[test]
    fn auto_profile_prefers_exact_over_prefix_and_longest_prefix() {
        // 精确命中 opus-4.8,即使 opus-4.6 登记在前且带家族前缀。
        assert_eq!(
            auto_profile("anthropic/claude-opus-4-8").as_deref(),
            Some("claude-code-opus-4.8")
        );
        // 归一化:点号与横杠等价。
        assert_eq!(
            auto_profile("claude/claude-sonnet-5-5").as_deref(),
            Some("claude-code-sonnet-5.5")
        );
        assert_eq!(
            auto_profile("claude-sonnet-5.5").as_deref(),
            Some("claude-code-sonnet-5.5")
        );
        // 未登记的具体型号落最长家族前缀:sonnet-5 的 `claude-sonnet-5`
        // 长于 sonnet-5.5 的 catch-all `claude`。
        assert_eq!(
            auto_profile("claude-sonnet-5-9").as_deref(),
            Some("claude-code-sonnet-5")
        );
        // 未知 claude 型号落 sonnet-5.5 的 catch-all(与厂商层语义对齐)。
        assert_eq!(
            auto_profile("claude-3-5-sonnet").as_deref(),
            Some("claude-code-sonnet-5.5")
        );
        // codex 代际默认人格:`gpt-6.1` 精确命中 6.1-sol(该代默认人格)。
        assert_eq!(
            auto_profile("openai/gpt-6.1").as_deref(),
            Some("codex-gpt-6.1-sol")
        );
        // OpenAI 家族 catch-all 镜像 provider_hint:`o3`/`chatgpt` 前缀落 gpt-5.6。
        assert_eq!(auto_profile("o3-mini").as_deref(), Some("codex-gpt-5.6"));
        assert_eq!(auto_profile("chatgpt-4o").as_deref(), Some("codex-gpt-5.6"));
        // 叶项:gemini / glm / kimi 家族命中回引变体。
        assert_eq!(
            auto_profile("gemini/gemini-3-pro").as_deref(),
            Some("gemini")
        );
        assert_eq!(auto_profile("zhipu/glm-5").as_deref(), Some("glm"));
        assert_eq!(auto_profile("moonshot/kimi-k3").as_deref(), Some("kimi-3"));
        assert_eq!(auto_profile("qwen3.8-max").as_deref(), Some("qwen3.8-max"));
        // 全无命中:未知模型回落 None。
        assert_eq!(auto_profile("mystery/unknown-model"), None);
        assert_eq!(auto_profile("stub/test"), None);
        assert_eq!(auto_profile(""), None);
    }

    #[test]
    fn tool_variant_only_matches_file_backed_entries() {
        // 实体变体命中并带家族层。
        let (tool, variant) = tool_variant("codex-gpt-6-sol").expect("实体变体应命中");
        assert_eq!(tool.id, "codex");
        assert_eq!(tool.family_layer, "gpt");
        assert_eq!(variant.label, "GPT-6 Sol");
        // 回引叶项不在此命中(走平铺路径)。
        assert!(tool_variant("gemini").is_none());
        assert!(tool_variant("不存在").is_none());
        assert_eq!(variant_family_layer("codex-gpt-6-sol"), Some("gpt"));
        assert_eq!(variant_family_layer("gemini"), None);
        assert!(tool_variant_text("codex-gpt-6-sol").is_some());
        assert!(tool_variant_text("gemini").is_none());
    }

    #[test]
    fn normalize_unifies_separators_and_case() {
        assert_eq!(normalize("Claude-Opus_4.6"), "claude-opus-4-6");
        assert_eq!(normalize("GLM-5"), "glm-5");
    }
}
