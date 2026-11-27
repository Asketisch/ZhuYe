//! 配置加载:从 TOML 文件 / 字符串 / 默认路径。
//!
//! ## 迁移
//!
//! v1 的固定接入段(`[anthropic]` / `[openai]` / `[ollama]`)在 v2 已移除,
//! 但旧配置**仍必须读得进来** —— serde 未开 `deny_unknown_fields`,
//! 直接删字段会让老用户的 api_key 被静默忽略。做法是:
//!
//! 1. 解析时**额外**把三段读进 [`LegacySections`];
//! 2. v1→v2 迁移把三段就地转成等价模型组(`id` / `endpoint` 与段名一致);
//! 3. 迁移后的配置里再无旧段数据 —— 序列化写回只有新格式。

use std::path::{Path, PathBuf};

use crate::DEFAULT_CONFIG_FILE;
use crate::error::ConfigError;
use crate::migration::migrate_to_target;
use crate::schema::{CURRENT_CONFIG_VERSION, ReflectConfig};

/// config.toml 的绝对路径。
///
/// 优先级:`$ZHUYE_HOME/config.toml` > `$HOME/.zhuye/config.toml`。
///
/// 此前本函数只读 `$HOME`,而 session-notes / prompts 等
/// 数据目录跟随 `ZHUYE_HOME` —— 同一变量两套语义,多实例(测试 /
/// 多端点并行)拿不到配置隔离。现在配置读取与数据目录一致:
/// `ZHUYE_HOME` 指向的目录**就是** `.zhuye` 目录本身(其下直接放
/// `config.toml`),未设置时回落 `$HOME/.zhuye`。
///
/// home 解析统一走 [`crate::paths::data_dir`] —— 全框架唯一口径。
pub fn default_config_path() -> Option<PathBuf> {
    crate::paths::data_dir().map(|dir| dir.join(DEFAULT_CONFIG_FILE))
}

/// 从任意 TOML 字符串解析;失败转 `ConfigError::Parse`。
///
/// **会跑迁移**(升到 [`CURRENT_CONFIG_VERSION`])—— 旧配置里的固定接入段
/// 会被转成等价模型组,`[active].provider` 会被读成 `[active].group`。
/// 只想按原始 TOML 解析、不迁移的调用方请用
/// [`load_from_str_with_migration`](load_from_str_with_migration) 并显式
/// 指定 `target`。
pub fn load_from_str(s: &str) -> Result<ReflectConfig, ConfigError> {
    load_from_str_with_migration(s, CURRENT_CONFIG_VERSION)
}

/// parse 后按 `target` 走 sequential migration(实现见
/// [`crate::migration`]).
///
/// v1→v2 的迁移内容:旧固定段(`[anthropic]` / `[openai]` / `[ollama]`)
/// 就地转成等价模型组;`[active].provider` 转成 `[active].group`。
/// 迁移**幂等** —— 已是新格式的配置读入后不产生任何变化。
pub fn load_from_str_with_migration(s: &str, target: u32) -> Result<ReflectConfig, ConfigError> {
    migrate_to_target(s, target)
}

/// 从指定文件路径加载;不存在 → `Io`,解析失败 → `Parse`。
///
/// **会跑迁移**(升到 [`CURRENT_CONFIG_VERSION`])—— 语义与
/// [`load_from_str`] 一致。
pub fn load_from_file(path: &Path) -> Result<ReflectConfig, ConfigError> {
    let s = std::fs::read_to_string(path)?;
    load_from_str(&s)
}

/// 从指定文件路径加载并跑 migration 到 `CURRENT_CONFIG_VERSION`。
///
/// 与 [`load_from_file`] 语义一致(都升到最新版本);保留此函数是为了
/// 让「显式要迁移」的调用方(readiness 检查 / doctor)表达清楚意图。
pub fn load_from_file_with_migration(path: &Path) -> Result<ReflectConfig, ConfigError> {
    let s = std::fs::read_to_string(path)?;
    load_from_str_with_migration(&s, CURRENT_CONFIG_VERSION)
}

/// 走 `$HOME/.zhuye/config.toml`;文件缺失 / 解析失败均回退到 `Default`,
/// 并通过 `tracing::warn` 提示 —— 与 `HooksConfig::load_default` 语义一致。
///
/// **会跑迁移** —— 旧固定段读入后自动转成模型组。
pub fn load_default() -> ReflectConfig {
    load_default_impl(false).unwrap_or_default()
}

/// 为会话启动读取默认配置,显式指定的未知来源治理模式视为错误。
pub fn load_default_for_runtime() -> Result<ReflectConfig, ConfigError> {
    load_default_impl(true)
}

fn load_default_impl(validate_provenance: bool) -> Result<ReflectConfig, ConfigError> {
    let Some(path) = default_config_path() else {
        tracing::debug!("HOME unset; using ReflectConfig::default()");
        return Ok(ReflectConfig::default());
    };
    match load_from_file(&path) {
        Ok(c) => Ok(c),
        // 修复原因:把拼错的 enforce 当作默认 observe 会静默降级治理。
        Err(e @ ConfigError::InvalidProvenanceMode(_)) if validate_provenance => Err(e),
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "config load failed; using default");
            Ok(ReflectConfig::default())
        }
    }
}

#[cfg(test)]
mod tests;
