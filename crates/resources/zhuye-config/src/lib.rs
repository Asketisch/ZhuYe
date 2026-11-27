//! `zhuye-config` — 统一 TOML 配置加载 + 热重载。
//!
//! M7 起取代 `zhuye-hooks::config::HooksConfig` 的单一职责，
//! 把模型 / compact / hooks 配置集中到 `~/.zhuye/config.toml`。
//!
//! ## 模块
//! - [`schema`] —— 数据结构 (`ZhuyeConfig` 及各 section)
//! - [`load`] —— 单次同步加载(含 v1→v2 迁移)
//! - [`watch`] —— 基于 `notify` 的热重载 (debounce 250ms)
//! - [`builder`] —— `ZhuyeConfig → ModelRegistry`
//!
//! ## 配置示例
//!
//! **模型组(`[[model_group]]`)是唯一的模型接入单元** —— 组 id 即模型
//! spec 前缀(`<组id>/<模型>`),也是 registry 的池名。
//!
//! ```toml
//! [active]
//! group = "anthropic"    # 活跃模型组 id
//!
//! [[model_group]]        # 模型组数组
//! id = "anthropic"       # 组 id = 模型 spec 前缀
//! endpoint = "anthropic" # 线格式:openai | anthropic | ollama
//! api_key = "sk-ant-..." # 也可由 ANTHROPIC_API_KEY env 提供(仅固定 id 组)
//! base_url = "https://api.anthropic.com"   # 可选:自定义 endpoint
//! model = "claude-3-5-sonnet-latest"       # 可选:默认模型
//!
//! [compact]                # 压缩策略段
//! trigger_tokens = 10000   # 触发自动压缩的输入 token 阈值
//!
//! [hooks]                  # 钩子总段
//! enabled = ["search_budget", "verification"]   # 启用的内置钩子列表
//!
//! [hooks.search_budget]    # search_budget 钩子专属配置
//! max_calls = 20           # 单 turn 最大搜索调用次数
//! ```
//!
//! v1 的固定段(`[anthropic]` / `[openai]` / `[ollama]`)仍可读入 —— 加载时
//! 自动迁移成等价模型组,写回只有新格式。

pub mod analytics;
pub mod builder;
pub mod error;
pub mod load;
pub mod migration;
pub mod model_discovery;
pub mod paths;
pub mod schema;
pub mod watch;

/// 将提示词默认选择原子写入配置文件，保留其余配置和注释。
pub fn save_prompt_selection(
    path: &std::path::Path,
    selection: &zhuye_protocol::PromptSelection,
) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let mut document = source
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| ConfigError::Parse(error.to_string()))?;
    if !document.as_table().contains_key("prompts") {
        document["prompts"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let table = document["prompts"]
        .as_table_like_mut()
        .ok_or_else(|| ConfigError::Parse("prompts 必须是表".into()))?;
    match selection {
        zhuye_protocol::PromptSelection::Default => {
            table.insert("mode", toml_edit::value("default"));
            table.remove("profile");
        }
        zhuye_protocol::PromptSelection::Auto => {
            table.insert("mode", toml_edit::value("auto"));
            table.remove("profile");
        }
        zhuye_protocol::PromptSelection::Manual { profile } => {
            table.insert("mode", toml_edit::value("manual"));
            table.insert("profile", toml_edit::value(profile.as_str()));
        }
    }
    let rendered = document.to_string();
    load_from_str(&rendered)?;
    zhuye_fs::write_atomic(path, rendered.as_bytes())?;
    Ok(())
}

pub use analytics::{
    ExporterGuard, Health, default_service_name, init_exporter, is_enabled as analytics_enabled,
    status_line as analytics_status_line,
};
pub use builder::{LspServerConfigShape, McpServerConfigShape};
pub use error::ConfigError;
pub use load::{
    default_config_path, load_default, load_default_for_runtime, load_from_file,
    load_from_file_with_migration, load_from_str, load_from_str_with_migration,
};
pub use model_discovery::{
    DEFAULT_CATALOG_CACHE_TTL_SECS, DEFAULT_DISCOVERY_TIMEOUT_SECS, DiscoveredCatalog,
    fetch_catalog, is_dynamic_model, pick_free_model, resolve_dynamic_model, resolve_model_spec,
};
pub use paths::{
    data_dir, data_path, data_path_or_relative, default_skill_dirs, project_agents_skills_dir,
    project_data_dir, project_data_path, resolve_data_dir, user_agents_skills_dir,
};
/// F70:feature flag 注册表(阶段 + env/config 覆盖 + 锁定)。
pub mod flags;
pub use flags::{FlagDecl, FlagRegistry, FlagState, ReleaseStage};
pub use schema::{
    ACP_DEFAULT_BIND, AcpSection, ActiveSection, AnalyticsSection, AskUserQuestionSection,
    BridgeSection, CompactSection, ContextWindowsSection, CoordinatorSection, CredentialConfig,
    DapSection, DynamicContextSection, GoalSection, HooksSection, LangfuseSection, LspFilePattern,
    LspServerEntry, LspServersSection, McpServerEntry, McpServersSection, McpTransport,
    ModelDiscoverySection, ModelGroupSection, ModelSection, NotificationChannel,
    NotificationsSection, PermissionsSection, PlanCompletionSection, PostgresSessionSection,
    PromptGuardSection, ProvenanceSection, QuotaConfig, QuotaSource, ReadBeforeEditSection,
    ResolvedAskUserQuestion, RoutingSection, SandboxSection, SanitizeSection, SearchBudgetSection,
    SpecSlotConfig, SseRedisSection, SubagentSpecConfig, TaskBudgetSection, TelemetrySection,
    TestRunnerSection, TokenBudgetSection, ToolJournalSection, TuiNotificationsSection,
    TurnSchedulingSection, VerificationSection, VoiceSection, WebSearchSection, ZhuyeConfig,
    parse_permission_entry,
};
pub use watch::{ConfigWatcher, WatcherHandle};

/// 配置目录的默认相对名（位于 `$HOME` 下）。
pub const DEFAULT_CONFIG_DIR: &str = ".zhuye";
/// 默认配置文件名。
pub const DEFAULT_CONFIG_FILE: &str = "config.toml";

#[cfg(test)]
mod prompt_selection_tests {
    #[test]
    fn saving_prompt_default_preserves_unrelated_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# 保留注释\n[custom]\nvalue = 42\n").unwrap();
        let selection = zhuye_protocol::PromptSelection::Manual {
            profile: "claude".into(),
        };
        super::save_prompt_selection(&path, &selection).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("# 保留注释"));
        assert!(content.contains("value = 42"));
        assert_eq!(super::load_from_str(&content).unwrap().prompts, selection);
        super::save_prompt_selection(&path, &zhuye_protocol::PromptSelection::Auto).unwrap();
        let content = std::fs::read_to_string(path).unwrap();
        assert!(!content.contains("profile ="));
        assert_eq!(
            super::load_from_str(&content).unwrap().prompts,
            zhuye_protocol::PromptSelection::Auto
        );
    }
}
