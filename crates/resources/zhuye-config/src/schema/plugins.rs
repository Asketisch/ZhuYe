//! 插件系统配置 section。
//!
//! 包含:
//! - `PluginMarketplaceConfig` / `PluginMarketplaceKind` — marketplace 源定义
//! - `PluginsSection` — 已启用插件列表 + marketplace 源

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

// ── Plugins (v1.0.0-rc2) ───────────────────────────────────────────

/// 单个 plugin 在 `[plugins.marketplaces.<name>]` 下的源定义。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PluginMarketplaceConfig {
    #[serde(rename = "type", default)]
    pub kind: PluginMarketplaceKind,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub r#ref: Option<String>,
    #[serde(default)]
    pub auto_update: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginMarketplaceKind {
    #[default]
    Directory,
    File,
    Github,
    Url,
}

/// `[plugins]` 段 —— 描述已启用的 plugin 与已知的 marketplace 源。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PluginsSection {
    #[serde(default)]
    pub enabled_plugins: Vec<String>,
    /// 显式禁用列表(`[plugins].disabled_plugins`,条目形如
    /// `name@marketplace` 或裸 `name`)。仅作用于**默认开启的 bundled
    /// 门控插件**(web-search / notebook 等,清单见 `zhuye-plugin`
    /// `BUNDLED_PLUGIN_DEFAULTS`):写入即让对应工具不进注册表
    /// (tools 面消失、tool_search 搜不到)。
    ///
    /// 与 `[active].disallowed_tools`(工具名级可见性过滤,工具已注册
    /// 仅 LLM 不可见)**粒度不同、互不替代**:前者是插件级注册开关,
    /// 后者是工具级可见性裁剪;三层叠加顺序 = 注册门控 → 可见性过滤
    /// → 执行期审批。空 = 无禁用(additive,存量配置零影响)。
    #[serde(default)]
    pub disabled_plugins: Vec<String>,
    #[serde(default)]
    pub marketplaces: HashMap<String, PluginMarketplaceConfig>,
}

impl PluginsSection {
    /// 检查某个 plugin id 是否启用。
    pub fn is_enabled(&self, plugin_id: &str) -> bool {
        self.enabled_plugins.iter().any(|p| p == plugin_id)
    }

    /// 检查某个 plugin 是否被显式禁用(条目取 `@` 前名字比对,
    /// 与 enabled 列表的 `goal-queue@inline` 比对口径一致)。
    pub fn is_disabled(&self, name: &str) -> bool {
        self.disabled_plugins
            .iter()
            .any(|p| p.split('@').next() == Some(name))
    }
}
