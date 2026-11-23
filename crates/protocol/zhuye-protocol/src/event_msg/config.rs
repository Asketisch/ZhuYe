//! 配置热重载载荷(+ SystemTime serde 适配器)。

use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// M7: 配置文件热重载事件。`path` 是触发变更的文件;`sections_changed` 是
/// 受影响段名列表(例如 `["anthropic", "compact"]`),便于 UI 只在关键段变更
/// 时提示。`at` 用 `SystemTime` 而非 `Instant`,因为事件可能跨进程持久化。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigReloadedEvent {
    /// 触发重载的配置文件路径。
    pub path: PathBuf,
    /// 受影响的配置段名列表(如 `["anthropic", "compact"]`)。
    pub sections_changed: Vec<String>,
    /// 重载发生时间(序列化为 UNIX 秒数)。
    #[serde(with = "systemtime_serde")]
    pub at: SystemTime,
}

/// 两级菜单的一个模型变体(id + 显示名),GUI 据此渲染二级菜单项。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptVariantRef {
    /// 变体 id(`PromptSelection::Manual.profile` 的合法取值)。
    pub id: String,
    /// 显示名(如 `Opus 4.6`;专有名词不翻译)。
    pub label: String,
}

/// 两级提示词目录的一个工具分组(一级菜单项)。
///
/// 多模型工具(如 Claude Code / Codex)带多个模型变体;单文件工具
/// (如 gemini-cli / deepseek)恰一个回引变体,GUI 渲染为一级叶项。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptGroup {
    /// 工具 id(如 `claude-code`)。
    pub id: String,
    /// 显示名(如 `Claude Code`)。
    pub label: String,
    /// 该组变体解析时叠加的家族层(厂商层)id —— 客户端用它把平铺
    /// `available` 清单里已被组覆盖的家族层滤掉,避免菜单重复。
    #[serde(default)]
    pub family_layer: String,
    /// 模型变体,清单顺序即展示顺序。
    pub variants: Vec<PromptVariantRef>,
}

/// v1.6.1:`Op::PromptCommand { action: List }` 的应答 —— 可用提示词层与
/// 当前生效层。TUI/GUI 的 `/prompt` overlay 据此渲染选择列表。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptListEvent {
    /// 可绑定的层文件名(不含 `.md`;内置厂商层 + 工具变体 + 用户目录
    /// 层,去重)。
    pub available: Vec<String>,
    /// 当前 model spec(如 `"anthropic/claude-sonnet-5-5"`)。
    pub current_spec: String,
    /// 当前 spec 实际命中的用户层文件名(诊断用,内嵌层不记录)。
    pub active_layers: Vec<String>,
    /// 当前生效的选择策略。
    #[serde(default)]
    pub selection: crate::PromptSelection,
    /// 当前请求使用的方案。
    #[serde(default)]
    pub profile: String,
    /// 未命中时回落的原因。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    /// v1.7:两级菜单的工具分组(工具 → 模型变体)。空清单(旧引擎/
    /// 目录损坏)时省略,客户端退化为按 `available` 平铺渲染。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<PromptGroup>,
}

/// v1.6.1:`Op::PromptCommand { action: Switch }` 的应答 —— 提示词库重扫
/// 完成。UI 据此提示「提示词已刷新」并展示新命中。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptSwitchedEvent {
    /// 触发重扫的动作名(`"reload"`;预留未来聚焦切换的层名)。
    pub action: String,
    /// 重扫后当前 spec 的生效用户层(让 UI 立即看到新命中)。
    pub active_layers: Vec<String>,
    /// 当前生效的选择策略。
    #[serde(default)]
    pub selection: crate::PromptSelection,
    /// 当前请求使用的方案。
    #[serde(default)]
    pub profile: String,
    /// 未命中时回落的原因。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
}

/// v1.x:`Op::RescanExtensions` 的应答 —— 运行时扩展目录重扫完成。
/// UI 据此提示「skills 已刷新」并展示新条目数。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtensionsRescannedEvent {
    /// skills 目录重扫后的条目总数(文件型 + 插件型;未请求重扫时为 0)。
    pub skills: usize,
    /// commands 重扫请求是否被处理。当前 commands 索引的运行时重扫尚未
    /// 接线(命令正文实时读盘,增删文件走插件重挂路径),恒为 `false`。
    pub commands: bool,
}

/// F01:`Op::QueryGuardStats` 的应答 —— prompt_guard 裁决统计快照。
/// UI(`/status` 安全段 / Desktop 设置)据此展示「guard 开没开、拦了
/// 多少」。`enabled = false` 时四计数恒 0(guard 未接线就没有计数源)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct GuardStatsEvent {
    /// prompt_guard 是否启用(F01 起默认开启;`[prompt_guard]
    /// enabled = false` / `ZHUYE_PROMPT_GUARD=0` 关闭)。
    pub enabled: bool,
    /// 工具定义(外部源 MCP/plugin/runtime 注册期)扫描 Block、
    /// 拒绝注册的累计次数。
    pub definitions_blocked: u64,
    /// 工具定义扫描 Review、放行注册但标注的累计次数。
    pub definitions_reviewed: u64,
    /// 工具输出扫描 Block、内容被替换拦截的累计次数。
    pub outputs_blocked: u64,
    /// 工具输出扫描 Review、metadata 标注的累计次数。
    pub outputs_reviewed: u64,
}

/// `SystemTime` 的 serde 适配 —— 序列化为 UNIX 秒数,跨进程持久化稳定。
pub(super) mod systemtime_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        let dur = t.duration_since(UNIX_EPOCH).unwrap_or_default();
        dur.as_secs().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SystemTime, D::Error> {
        let secs = u64::deserialize(d)?;
        Ok(UNIX_EPOCH + Duration::from_secs(secs))
    }
}
