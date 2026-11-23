//! zhuye-prompt —— 分层 system prompt 构造与 prompt 缓存支持。
//!
//! Zhuye 路线图的 M4。四个模块:
//! - [`template`] —— 轻量 minijinja 封装,做 `{{ var }}` 变量替换。
//! - [`caching`] —— Anthropic `cache_control` 注入 + 变化检测。
//! - [`builder`] —— `LayeredPrompt` 组合(core / append / ephemeral),
//!   以及持有 `CacheBreakDetector`、从分层集合组装 `ChatRequest` 的
//!   `PromptBuilder`。
//! - [`library`] —— 按模型匹配的外置提示词库(default → 厂商 →
//!   精确模型名三级回退),v1.6 起支持 `.zhuye/prompts/*.md`。
//!
//! v0 只做 `{{ var }}` 变量替换;不支持 `{% if %}` / `{% for %}` 块
//! (与 Zhuye `manager.get_prompt` 语义一致)。

pub mod builder;
pub mod caching;
/// v1.7:工具×模型两级提示词目录(sources.json `tools` 段)—— 一级菜单
/// 工具名、二级菜单模型变体,以及「智能匹配模型」的引擎端匹配。
pub mod catalog;
pub mod library;
/// F69:Composer mention 解析(@file / #issue / !PR)+ picker 候选 + 上下文装配。
pub mod mentions;
pub mod resources;
/// F53:提示词工程(条件注入 + 就近优先 + 版本实验)。
pub mod rules;
pub mod template;

pub use builder::{
    AGENTS_MD_HEADER, CORE_PREFIX, EPHEMERAL_PREFIX, LayeredPrompt, MEMORY_HEADER,
    PLAN_MODE_HEADER, PromptBuilder, append_active_mode_section,
    append_active_mode_section_with_set, append_agents_md_section, is_section_off,
    plan_mode_enabled_for, plan_mode_guidance,
};
pub use caching::{
    CacheBreakDetector, CacheBreakError, CacheMonitorStats, DEFAULT_PREFIX_ANCHOR_OFFSET,
    find_prefix_anchor, inject_cache_control,
};
pub use catalog::{
    EMBEDDED_TOOL_PROMPTS, PromptTool, PromptVariant, auto_profile, catalog_variant_ids,
    tool_catalog, tool_variant, tool_variant_text, variant_family_layer,
};
pub use library::{
    DescriptionSource, PromptLibrary, PromptResolution, PromptSet, apply_model_overrides,
    init_library, library, split_spec,
};
pub use resources::{PromptResources, copy, prompts};
pub use template::{PromptError, render, render_with};

/// 收敛提示模板:当工具调用应停止、要求模型用此格式给出最终答案时,各收敛
/// 边路(nudge、max-iterations force-final、auto-continue)统一引用此常量,
/// 避免措辞漂移。
///
/// **不常驻** ephemeral `## Important`:运行时没有任何代码消费该标记
/// (turn 收尾判定是「模型不再调工具」),常驻注入只会让照字面执行的模型
/// 把标记回显进每条回答,污染输出。只在迭代预算耗尽等真正需要强收口压力
/// 的边路注入;展示层(TUI/GUI)对历史会话中已存在的标记做渲染剥离。
///
/// 模板值固定为 `"FINAL ANSWER: <answer>"`。现有测试(`single_turn.rs` /
/// `pre_loop_m4.rs`)用 `contains("FINAL ANSWER")` 断言,本常量值变更会
/// 破坏这些断言 —— 改前先 grep 确认。
pub const FINAL_ANSWER_TEMPLATE: &str = "FINAL ANSWER: <answer>";
