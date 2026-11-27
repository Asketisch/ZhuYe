//! 配置版本迁移 —— v1 → v2:旧固定接入段 → 等价模型组。
//!
//! v1 的配置把「可接入的端点」钉死在 `[anthropic]` / `[openai]` /
//! `[ollama]` 三个固定槽位上;v2 起只剩模型组(`[[model_group]]`)。
//! 旧配置**必须仍能读入** —— serde 未开 `deny_unknown_fields`,直接删字段
//! 会让老用户的 api_key 被静默忽略。
//!
//! 做法:解析时**额外**把旧形态读进 [`LegacySections`],迁移时转成模型组;
//! 迁移后的配置里再无旧段数据 —— 序列化写回只有新格式。

use crate::error::ConfigError;
use crate::schema::legacy::LegacySections;
use crate::schema::{ModelGroupSection, ZhuyeConfig};

/// 把 TOML 解析成「主结构 + 旧形态」两份。
///
/// 旧形态单独解析 —— `ZhuyeConfig` 里已经没有 `[anthropic]` 等槽位,
/// 直接反序列化会**静默忽略**它们(serde 未开 `deny_unknown_fields`),
/// 老用户的 api_key 就这么丢了。
pub(crate) fn parse_raw(s: &str) -> Result<(ZhuyeConfig, LegacySections), ConfigError> {
    let cfg: ZhuyeConfig = toml::from_str(s).map_err(|error| {
        // 修复原因:错误类型的显式模式也不能走普通 TOML 解析失败的启动兜底。
        if let Ok(table) = s.parse::<toml::Table>()
            && let Some(mode) = table.get("provenance").and_then(|value| value.get("mode"))
            && !mode.is_str()
        {
            return ConfigError::InvalidProvenanceMode(mode.type_str().into());
        }
        ConfigError::Parse(error.to_string())
    })?;
    if let Some(mode) = cfg
        .provenance
        .as_ref()
        .and_then(|section| section.mode.as_deref())
        && !["off", "observe", "enforce"]
            .iter()
            .any(|value| mode.trim().eq_ignore_ascii_case(value))
    {
        return Err(ConfigError::InvalidProvenanceMode(mode.to_owned()));
    }
    let legacy: LegacySections =
        toml::from_str(s).map_err(|e| ConfigError::Parse(e.to_string()))?;
    Ok((cfg, legacy))
}

/// 按 `target` 走 sequential migration。
///
/// - `target = CURRENT_CONFIG_VERSION`(默认推荐)即「升到最新」。
/// - `target` 低于 `current` 时 no-op(避免降级损坏 in-memory 数据)。
/// - 未知起始版本号 → `ConfigError::Migration`。
pub(crate) fn migrate_to_target(s: &str, target: u32) -> Result<ZhuyeConfig, ConfigError> {
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

/// v1 → v2 migration:旧固定接入段 → 等价模型组。
fn migrate_v1_to_v2(
    cfg: &mut ZhuyeConfig,
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
/// - `[active].provider` → `[active].group`(仅当 group 未设)
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
fn absorb_legacy_sections(cfg: &mut ZhuyeConfig, legacy: &mut LegacySections) {
    // v1 的 `[active].provider` → `[active].group`(仅当 group 未设 ——
    // 两个键同时存在时以新键为准,不报 duplicate field)。
    if cfg.active.group.is_none()
        && let Some(p) = legacy
            .active
            .as_ref()
            .and_then(|a| a.provider.as_deref())
            .map(str::trim)
            .filter(|p| !p.is_empty())
    {
        cfg.active.group = Some(p.to_string());
    }
    // v1 的 `[subagent_providers]` → `[subagent_models]`(仅当新段未设)。
    if cfg.subagent_models.is_none()
        && let Some(sec) = legacy.subagent_providers.take()
    {
        cfg.subagent_models = Some(sec);
    }
    if legacy.anthropic.is_none() && legacy.openai.is_none() && legacy.ollama.is_none() {
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
