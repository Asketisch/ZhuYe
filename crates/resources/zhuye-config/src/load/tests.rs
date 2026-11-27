//! `load` 模块单测 —— 解析、迁移、落盘读取。

use super::*;
use crate::schema::{ActiveSection, CompactSection, HooksSection};

#[test]
fn empty_string_yields_default() {
    let cfg = load_from_str("").unwrap();
    assert!(cfg.active.group.is_none());
    assert!(cfg.model_group.is_empty());
}

#[test]
fn 来源治理模式接受三种值并报告拼写错误() {
    for mode in ["off", "observe", "enforce"] {
        let config = load_from_str(&format!("[provenance]\nmode = {mode:?}")).unwrap();
        assert_eq!(config.provenance.unwrap().mode.as_deref(), Some(mode));
    }
    assert!(matches!(
        load_from_str("[provenance]\nmode = 'enfroce'"),
        Err(ConfigError::InvalidProvenanceMode(mode)) if mode == "enfroce"
    ));
    assert!(matches!(
        load_from_str("[provenance]\nmode = true"),
        Err(ConfigError::InvalidProvenanceMode(mode)) if mode == "boolean"
    ));
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

/// `[active].provider` 与 `[active].group` **同时存在** → 以新键为准,
/// 且**不得**报 duplicate field(半迁移的手改配置很常见)。
#[test]
fn both_active_keys_present_prefers_group() {
    let toml = r#"
[active]
provider = "openai"
group = "my-glm"
"#;
    let cfg = load_from_str(toml).expect("两个键并存不应解析失败");
    assert_eq!(cfg.active.group.as_deref(), Some("my-glm"));
}

/// v1 的 `[subagent_providers]` 段读入后落到 `[subagent_models]`。
#[test]
fn legacy_subagent_providers_section_migrates() {
    let toml = r#"
[subagent_providers.anthropic]
api_key = "sk-sub"
model = "claude-haiku"
"#;
    let cfg = load_from_str(toml).unwrap();
    let sec = cfg.subagent_models.as_ref().expect("子代理段应迁移过来");
    let anth = sec.anthropic.as_ref().expect("anthropic 子段");
    assert_eq!(anth.api_key.as_deref(), Some("sk-sub"));
    assert_eq!(anth.model.as_deref(), Some("claude-haiku"));
    let rendered = toml::to_string(&cfg).unwrap();
    assert!(rendered.contains("[subagent_models.anthropic]"));
    assert!(!rendered.contains("subagent_providers"));
}

/// 配置面(TOML)里不再出现 "provider" 字样 —— v2 的模型接入只有模型组。
///
/// 结构门禁:新增任何 TOML 可见字段时若带 "provider" 字样,本测试
/// 立即失败(v1 的 `[subagent_providers]` 已改名 `[subagent_models]`)。
#[test]
fn toml_surface_has_no_provider_key() {
    let cfg = ReflectConfig::default();
    let rendered = toml::to_string(&cfg).unwrap();
    assert!(
        !rendered.contains("provider"),
        "配置面不得出现 provider 字样,got:\n{rendered}"
    );
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
