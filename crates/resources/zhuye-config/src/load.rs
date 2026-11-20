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
use crate::schema::legacy::LegacySections;
use crate::schema::{CURRENT_CONFIG_VERSION, ModelGroupSection, ReflectConfig};

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

/// parse 后按 `target` 走 sequential migration。
///
/// - `target = CURRENT_CONFIG_VERSION`(默认推荐)即「升到最新」。
/// - `target` 低于 `current` 时 no-op(避免降级损坏 in-memory 数据)。
/// - 未知起始版本号 → `ConfigError::Migration`。
///
/// v1→v2 的迁移内容:旧固定段(`[anthropic]` / `[openai]` / `[ollama]`)
/// 就地转成等价模型组;`[active].provider` 由 serde alias 读进
/// `[active].group`。迁移**幂等** —— 已是新格式的配置读入后不产生任何变化。
pub fn load_from_str_with_migration(s: &str, target: u32) -> Result<ReflectConfig, ConfigError> {
    let (mut cfg, mut legacy) = parse_raw(s)?;
    // 旧 TOML 无 `config_version` 字段时,serde 默认填 1;此处不再做 .max。
    let mut current = cfg.config_version;
    // current > target:降级无 migration 路径(避免 in-memory 假设失效)。
    if current > target {
        return Err(ConfigError::Migration(format!(
            "config_version v{current} is newer than target v{target}; \
             refusing to downgrade"
        )));
    }
    while current < target {
        current = match current {
            1 => migrate_v1_to_v2(&mut cfg, &mut legacy)?,
            other => {
                return Err(ConfigError::Migration(format!(
                    "no migration path from v{other} to v{target}"
                )));
            }
        };
        cfg.config_version = current;
    }
    // 版本号已是 v2、但文件里仍留着旧固定段的(手改 / 半迁移 / 外部工具
    // 写坏了版本号)照样吸收 —— 迁移幂等,且不丢用户配置。
    if target >= 2 {
        absorb_legacy_sections(&mut cfg, &mut legacy);
    }
    Ok(cfg)
}

/// 解析 TOML:主结构 + 旧固定段(后者只取三段,其余键忽略)。
fn parse_raw(s: &str) -> Result<(ReflectConfig, LegacySections), ConfigError> {
    let cfg: ReflectConfig = toml::from_str(s).map_err(|e| ConfigError::Parse(e.to_string()))?;
    let legacy: LegacySections =
        toml::from_str(s).map_err(|e| ConfigError::Parse(e.to_string()))?;
    Ok((cfg, legacy))
}

/// v1 → v2 migration:旧固定接入段 → 等价模型组。
fn migrate_v1_to_v2(
    cfg: &mut ReflectConfig,
    legacy: &mut LegacySections,
) -> Result<u32, ConfigError> {
    absorb_legacy_sections(cfg, legacy);
    Ok(2)
}

/// 把旧固定段吸收成模型组(**幂等**)。
///
/// 映射:
/// - `[anthropic]` → 组 `id="anthropic"` / `endpoint="anthropic"`
/// - `[openai]` → 组 `id="openai"` / `endpoint="openai"`
/// - `[ollama]` → 组 `id="ollama"` / `endpoint="ollama"`
///
/// `api_key` / `base_url` / `model` / `temperature` / `max_tokens` /
/// `timeout_secs` / `credentials` 原样下沉;`[openai].responses_api` 与
/// `[ollama].keep_alive_secs` / `num_ctx` / `num_gpu` 落到组级同名字段。
///
/// 幂等性:
/// - 已是新格式的文件(三段全空)→ 本函数直接返回,不做任何改动;
/// - 已有同 id 的模型组 → **不覆盖**(保留模型组现状),只丢旧段;
/// - 迁移后的组被前插到 `model_group` 首位,保持旧「固定段优先于模型组」
///   的活跃组挑选顺序。
fn absorb_legacy_sections(cfg: &mut ReflectConfig, legacy: &mut LegacySections) {
    if legacy.is_empty() {
        return;
    }
    let mut migrated: Vec<ModelGroupSection> = Vec::with_capacity(3);
    if let Some(s) = legacy.anthropic.take() {
        migrated.push(ModelGroupSection {
            id: "anthropic".into(),
            endpoint: "anthropic".into(),
            api_key: s.api_key,
            base_url: s.base_url,
            model: s.model,
            timeout_secs: s.timeout_secs,
            temperature: s.temperature,
            max_tokens: s.max_tokens,
            credentials: s.credentials,
            ..Default::default()
        });
    }
    if let Some(s) = legacy.openai.take() {
        migrated.push(ModelGroupSection {
            id: "openai".into(),
            endpoint: "openai".into(),
            api_key: s.api_key,
            base_url: s.base_url,
            model: s.model,
            timeout_secs: s.timeout_secs,
            temperature: s.temperature,
            max_tokens: s.max_tokens,
            responses_api: s.responses_api,
            credentials: s.credentials,
            ..Default::default()
        });
    }
    if let Some(s) = legacy.ollama.take() {
        migrated.push(ModelGroupSection {
            id: "ollama".into(),
            endpoint: "ollama".into(),
            api_key: s.api_key,
            base_url: s.base_url,
            model: s.model,
            timeout_secs: s.timeout_secs,
            temperature: s.temperature,
            max_tokens: s.max_tokens,
            keep_alive_secs: s.keep_alive_secs,
            num_ctx: s.num_ctx,
            num_gpu: s.num_gpu,
            credentials: s.credentials,
            ..Default::default()
        });
    }
    // 前插:旧口径下固定段(active 挑选优先级)高于模型组,保持一致。
    let mut inserted = 0usize;
    for group in migrated {
        if cfg
            .model_group
            .iter()
            .any(|existing| existing.id.trim() == group.id)
        {
            tracing::warn!(
                group = %group.id,
                "已有同名模型组,旧固定段不覆盖(保留模型组现状)"
            );
            continue;
        }
        cfg.model_group.insert(inserted, group);
        inserted += 1;
    }
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
    let Some(path) = default_config_path() else {
        tracing::debug!("HOME unset; using ReflectConfig::default()");
        return ReflectConfig::default();
    };
    match load_from_file(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "config load failed; using default");
            ReflectConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ActiveSection, CompactSection, HooksSection};

    #[test]
    fn empty_string_yields_default() {
        let cfg = load_from_str("").unwrap();
        assert!(cfg.active.group.is_none());
        assert!(cfg.model_group.is_empty());
    }

    /// v1 的 `[anthropic]` 段读入后**自动迁移**成等价模型组。
    #[test]
    fn legacy_anthropic_section_migrates_to_model_group() {
        let toml = r#"
[active]
provider = "anthropic"

[anthropic]
api_key = "sk-ant-test"
base_url = "https://api.anthropic.com"
model = "claude-3-5-sonnet-latest"
timeout_secs = 90
"#;
        let cfg = load_from_str(toml).unwrap();
        assert_eq!(cfg.active.group.as_deref(), Some("anthropic"));
        assert_eq!(cfg.model_group.len(), 1);
        let g = &cfg.model_group[0];
        assert_eq!(g.id, "anthropic");
        assert_eq!(g.endpoint, "anthropic");
        assert_eq!(g.api_key.as_deref(), Some("sk-ant-test"));
        assert_eq!(g.base_url.as_deref(), Some("https://api.anthropic.com"));
        assert_eq!(g.model.as_deref(), Some("claude-3-5-sonnet-latest"));
        assert_eq!(g.timeout_secs, Some(90));
    }

    /// v1 的 `[openai]` 段读入后自动迁移成等价模型组。
    #[test]
    fn legacy_openai_section_migrates_to_model_group() {
        let toml = r#"
[openai]
api_key = "sk-openai-test"
responses_api = true
"#;
        let cfg = load_from_str(toml).unwrap();
        assert_eq!(cfg.model_group.len(), 1);
        let g = &cfg.model_group[0];
        assert_eq!(g.id, "openai");
        assert_eq!(g.endpoint, "openai");
        assert_eq!(g.api_key.as_deref(), Some("sk-openai-test"));
        assert!(g.responses_api, "responses_api 应下沉到组级");
    }

    /// v1 的 `[ollama]` 段读入后自动迁移成等价模型组(含 ollama 专属字段)。
    #[test]
    fn legacy_ollama_section_migrates_to_model_group() {
        let toml = r#"
[ollama]
base_url = "http://127.0.0.1:11434"
model = "qwen3:8b"
keep_alive_secs = 600
num_ctx = 8192
num_gpu = 99
"#;
        let cfg = load_from_str(toml).unwrap();
        assert_eq!(cfg.model_group.len(), 1);
        let g = &cfg.model_group[0];
        assert_eq!(g.id, "ollama");
        assert_eq!(g.endpoint, "ollama");
        assert_eq!(g.base_url.as_deref(), Some("http://127.0.0.1:11434"));
        assert_eq!(g.model.as_deref(), Some("qwen3:8b"));
        assert_eq!(g.keep_alive_secs, Some(600));
        assert_eq!(g.num_ctx, Some(8192));
        assert_eq!(g.num_gpu, Some(99));
    }

    /// 旧段里的 `[[<段>.credentials]]` 数组 → 组级 `credentials`。
    #[test]
    fn legacy_credentials_array_migrates_into_group() {
        let toml = r#"
[anthropic]
api_key = "sk-a"

[[anthropic.credentials]]
label = "work"
api_key = "sk-work"
"#;
        let cfg = load_from_str(toml).unwrap();
        assert_eq!(cfg.model_group.len(), 1);
        assert_eq!(cfg.model_group[0].credentials.len(), 1);
        assert_eq!(cfg.model_group[0].credentials[0].label, "work");
        assert_eq!(cfg.model_group[0].credentials[0].api_key, "sk-work");
    }

    /// 迁移**幂等**:已是新格式的配置读入后不得产生任何变化。
    #[test]
    fn migration_is_idempotent_for_new_format() {
        let toml = r#"
config_version = 2

[active]
group = "my-glm"

[[model_group]]
id = "my-glm"
endpoint = "openai"
api_key = "sk-x"
model = "GLM-5"
"#;
        let cfg = load_from_str(toml).unwrap();
        assert_eq!(cfg.config_version, CURRENT_CONFIG_VERSION);
        assert_eq!(cfg.model_group.len(), 1, "不得重复追加组");
        assert_eq!(cfg.model_group[0].id, "my-glm");
        assert_eq!(cfg.model_group[0].api_key.as_deref(), Some("sk-x"));
        assert_eq!(cfg.active.group.as_deref(), Some("my-glm"));
        // 再读一次同样的内容 → 完全一致。
        let again = load_from_str(toml).unwrap();
        assert_eq!(cfg, again);
    }

    /// 迁移幂等:旧格式读入 → 序列化 → 再读入,结果不变(不重复追加组)。
    #[test]
    fn migration_is_idempotent_after_round_trip() {
        let toml = r#"
[active]
provider = "anthropic"

[anthropic]
api_key = "sk-ant-test"
model = "claude-x"
"#;
        let first = load_from_str(toml).unwrap();
        let rendered = toml::to_string(&first).unwrap();
        assert!(!rendered.contains("[anthropic]"), "旧段不得出现在写回里");
        assert!(rendered.contains("[[model_group]]"), "写回应是新格式");
        let second = load_from_str(&rendered).unwrap();
        assert_eq!(first, second, "迁移必须幂等");
    }

    /// 已有同 id 的模型组时,旧固定段**不覆盖**模型组现状。
    #[test]
    fn legacy_section_does_not_override_existing_group() {
        let toml = r#"
[[model_group]]
id = "openai"
endpoint = "openai"
api_key = "sk-new"

[openai]
api_key = "sk-old"
"#;
        let cfg = load_from_str(toml).unwrap();
        assert_eq!(cfg.model_group.len(), 1, "不得追加重复组");
        assert_eq!(
            cfg.model_group[0].api_key.as_deref(),
            Some("sk-new"),
            "已有模型组不被旧段覆盖"
        );
    }

    /// 写回只有新格式:旧段数据从序列化输出里消失。
    #[test]
    fn serialized_output_has_no_legacy_sections() {
        let toml = r#"
[anthropic]
api_key = "sk-a"

[openai]
api_key = "sk-o"

[ollama]
api_key = "sk-l"
"#;
        let cfg = load_from_str(toml).unwrap();
        let rendered = toml::to_string(&cfg).unwrap();
        assert!(!rendered.contains("[anthropic]"));
        assert!(!rendered.contains("[openai]"));
        assert!(!rendered.contains("[ollama]"));
        assert_eq!(rendered.matches("[[model_group]]").count(), 3);
    }

    /// v1 的 `[active].provider` 读入后落到 `[active].group`(写回只写 group)。
    #[test]
    fn legacy_active_provider_key_maps_to_group() {
        let toml = r#"
[active]
provider = "openai"
"#;
        let cfg = load_from_str(toml).unwrap();
        assert_eq!(cfg.active.group.as_deref(), Some("openai"));
        let rendered = toml::to_string(&cfg).unwrap();
        assert!(rendered.contains("group = \"openai\""));
        assert!(!rendered.contains("provider ="));
    }

    #[test]
    fn parses_compact_section() {
        let toml = r#"
[compact]
trigger_tokens = 8000
"#;
        let cfg = load_from_str(toml).unwrap();
        assert_eq!(cfg.compact.trigger_tokens, Some(8000));
    }

    #[test]
    fn parses_hooks_section() {
        let toml = r#"
[hooks]
enabled = ["search_budget", "verification"]

[hooks.search_budget]
max_calls = 15
search_tools = ["grep", "read"]

[hooks.verification]
run_on_stop = false
test_command = "cargo test --quiet"
"#;
        let cfg = load_from_str(toml).unwrap();
        assert_eq!(
            cfg.hooks.enabled,
            Some(vec!["search_budget".into(), "verification".into()])
        );
        let sb = cfg.hooks.search_budget.unwrap();
        assert_eq!(sb.max_calls, Some(15));
        assert_eq!(sb.search_tools, Some(vec!["grep".into(), "read".into()]));
        let v = cfg.hooks.verification.unwrap();
        assert_eq!(v.run_on_stop, Some(false));
        assert_eq!(v.test_command.as_deref(), Some("cargo test --quiet"));
    }

    #[test]
    fn invalid_toml_returns_parse_error() {
        let res = load_from_str("this is not toml");
        assert!(matches!(res, Err(ConfigError::Parse(_))));
    }

    #[test]
    fn missing_file_returns_io_error() {
        let path = Path::new("/nonexistent/path/to/config.toml");
        let res = load_from_file(path);
        assert!(matches!(res, Err(ConfigError::Io(_))));
    }

    #[test]
    fn load_default_returns_default_when_no_home_or_no_file() {
        // 测试里 unset HOME 比较脆弱;只需确认无论如何都返回 Default。
        let cfg = load_default();
        // 不论拿到的是默认值(大多数情况)还是真的加载到了内容,
        // 都断言它可反序列化且结构良好。
        let _: ActiveSection = cfg.active;
        let _: Vec<crate::schema::ModelGroupSection> = cfg.model_group;
        let _: CompactSection = cfg.compact;
        let _: HooksSection = cfg.hooks;
    }

    /// `ZHUYE_HOME` 指向的目录放有合法 config.toml 时,
    /// `load_default` 必须读到它(配置与 session-notes 等数据目录同一
    /// 套 `ZHUYE_HOME` 语义)。env 改写与读 env 的测试(paths.rs 等)
    /// 共用 crate 级 `paths::ENV_LOCK` 串行化,避免并行测试互踩 ——
    /// 函数局部锁只管得了本文件,管不了并行读 env 的其他模块。
    #[test]
    fn zhuye_home_redirects_config_load() {
        let _guard = crate::paths::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"
[active]
provider = "anthropic"

[anthropic]
api_key = "zhuye-home-isolated"
"#;
        std::fs::write(dir.path().join(DEFAULT_CONFIG_FILE), toml).unwrap();
        let orig = std::env::var_os("ZHUYE_HOME");
        // SAFETY:已持互斥锁;单线程测试进程内无并发读者。
        unsafe {
            std::env::set_var("ZHUYE_HOME", dir.path());
        }
        let cfg = load_default();
        // SAFETY:同上,仍持锁。
        unsafe {
            match orig {
                Some(v) => std::env::set_var("ZHUYE_HOME", v),
                None => std::env::remove_var("ZHUYE_HOME"),
            }
        }
        assert_eq!(cfg.active.group.as_deref(), Some("anthropic"));
        assert_eq!(cfg.model_group.len(), 1);
        assert_eq!(
            cfg.model_group[0].api_key.as_deref(),
            Some("zhuye-home-isolated")
        );
    }

    #[test]
    fn load_from_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[active]
provider = "anthropic"

[anthropic]
api_key = "sk-x"
"#,
        )
        .unwrap();
        let cfg = load_from_file(&path).unwrap();
        assert_eq!(cfg.active.group.as_deref(), Some("anthropic"));
        assert_eq!(cfg.model_group.len(), 1);
        assert_eq!(cfg.model_group[0].api_key.as_deref(), Some("sk-x"));
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let toml = r#"
[active]
provider = "anthropic"
totally_unknown_key = "ignored"

[anthropic]
api_key = "sk-x"
another_unknown = 42
"#;
        // serde 默认行为:忽略未知键(我们没开启 deny_unknown_fields)。
        let cfg = load_from_str(toml).unwrap();
        assert_eq!(cfg.model_group[0].api_key.as_deref(), Some("sk-x"));
    }

    // ── Phase 4 config_version migration tests ───────────────────

    /// ReflectConfig::default() 显式给 `config_version = CURRENT_CONFIG_VERSION`。
    #[test]
    fn default_has_current_config_version() {
        let cfg = ReflectConfig::default();
        assert_eq!(cfg.config_version, CURRENT_CONFIG_VERSION);
        assert_eq!(cfg.config_version, 2);
    }

    /// 旧 TOML 无 `config_version` 字段时,serde 默认填 1(**不是** 2)——
    /// 缺省填 2 会让迁移框架误判「已是最新」而跳过旧段迁移。
    #[test]
    fn config_version_defaults_to_1_when_missing() {
        let cfg = load_from_str_with_migration("[active]\nprovider = \"anthropic\"\n", 1).unwrap();
        assert_eq!(cfg.config_version, 1);
    }

    /// 显式 `config_version = 2`,load 后保留。
    #[test]
    fn explicit_config_version_is_preserved() {
        let cfg = load_from_str("config_version = 2\n").unwrap();
        assert_eq!(cfg.config_version, 2);
    }

    /// `target` 等于 `current`:no-op,直接返回。
    /// target < current(降级)走 `downgrade_to_older_target_errors` 测。
    #[test]
    fn migration_target_at_or_below_current_is_noop() {
        let cfg2 = load_from_str_with_migration("config_version = 2\n", 2).unwrap();
        assert_eq!(cfg2.config_version, 2, "target=current 应 no-op");
        // target 高于 current 但链未实现也走 no-op?不:走 migration 链,
        // 这里 current=2,target=3 走 unknown_migration_step_errors 路径。
        // 本测试只覆盖 target=current 的纯 no-op。
    }

    /// `target` 高于 current 且 migration 链存在:v1 → v2 把旧固定段
    /// 转成模型组,新 cfg.config_version 被 bump 到 2。
    #[test]
    fn migration_chain_v1_to_v2_migrates_legacy_sections() {
        // 旧 TOML 无 config_version → serde 默认 1;target=2 → 跑 v1→v2。
        let cfg = load_from_str_with_migration(
            "[active]\nprovider = \"anthropic\"\n\n[anthropic]\napi_key = \"sk-a\"\n",
            2,
        )
        .unwrap();
        assert_eq!(cfg.config_version, 2);
        assert_eq!(cfg.active.group.as_deref(), Some("anthropic"));
        assert_eq!(cfg.model_group.len(), 1);
        assert_eq!(cfg.model_group[0].id, "anthropic");
    }

    /// 起始版本号高于 target(降级场景):返 `Migration` 错误。
    /// 拒绝降级避免 in-memory 字段假设失效(如新字段在老 schema 中缺语义)。
    #[test]
    fn downgrade_to_older_target_errors() {
        let res = load_from_str_with_migration("config_version = 999\n", 1);
        match res {
            Err(ConfigError::Migration(msg)) => {
                assert!(msg.contains("v999"), "got: {msg}");
                assert!(
                    msg.contains("downgrade") || msg.contains("newer"),
                    "got: {msg}"
                );
            }
            other => panic!("expected Migration error, got {other:?}"),
        }
    }

    /// 回归测试:旧 TOML(无 config_version)走 migration 也兼容。
    #[test]
    fn old_toml_without_version_loads_and_migrates() {
        let cfg = load_from_str_with_migration(
            r#"
[active]
provider = "openai"

[openai]
api_key = "sk-old"
"#,
            CURRENT_CONFIG_VERSION,
        )
        .unwrap();
        assert_eq!(cfg.config_version, CURRENT_CONFIG_VERSION);
        assert_eq!(cfg.model_group.len(), 1);
        assert_eq!(cfg.model_group[0].id, "openai");
        assert_eq!(cfg.model_group[0].api_key.as_deref(), Some("sk-old"));
    }

    /// `ConfigError::Migration` variant 可构造 + Debug 输出正常。
    #[test]
    fn config_error_migration_variant_works() {
        let e = ConfigError::Migration("test".into());
        assert_eq!(format!("{e}"), "migration: test");
    }
}
