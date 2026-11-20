//! 模型组 → `CredentialPool` 的构造。
//!
//! 模型组是**唯一的接入单元**:每个组按 `endpoint` 分派到对应的 client
//! 构造,凭据来源是「组内 `credentials` 数组」优先、「组级单值」兜底。
//!
//! ## env 兜底
//!
//! 仅对三个**固定 id** 的组保留 v1 的 env 兜底(其余组一律不碰 env ——
//! 第三方网关误吞 `OPENAI_API_KEY` 比缺 key 更难诊断):
//!
//! | 组 id | 兜底 env | 兜底字段 |
//! |---|---|---|
//! | `anthropic` | `ANTHROPIC_API_KEY` | `api_key` |
//! | `openai` | `OPENAI_API_KEY` | `api_key` |
//! | `ollama` | `OLLAMA_HOST` / `OLLAMA_API_KEY` | `base_url` / `api_key` |

use std::sync::Arc;
use std::time::Duration;

use zhuye_llm::{
    AnthropicClient, AnthropicConfig, CredentialPool, ModelClient, OllamaClient, OllamaConfig,
    OpenAIClient, OpenAIConfig, OpenAIResponsesClient, OpenAIResponsesConfig, PoolEntry,
};

use crate::error::ConfigError;
use crate::schema::{
    AnthropicSubagentSection, CredentialConfig, ModelGroupSection, OllamaSubagentSection,
    OpenAISubagentSection,
};

/// 组 HTTP 超时默认 300s。语义为「建连 + 读空闲」双层超时
/// (zhuye-llm `stream_http_client`),非整请求总时长 —— reqwest 的
/// total timeout 会把超过时长的流式响应腰斩(实战 60s 整点断流),
/// 300s 读空闲同时覆盖推理模型 prefill/思考期的无字节窗口。
pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Ollama 默认超时 120s(本地推理首 token 慢)。
pub(crate) const DEFAULT_OLLAMA_TIMEOUT_SECS: u64 = 120;

pub(crate) const ENV_ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";
pub(crate) const ENV_OPENAI_API_KEY: &str = "OPENAI_API_KEY";
/// Ollama 启动时可被识别的 env。本地 `ollama serve` 一般不需要 key,
/// 但 Ollama Cloud / 反向代理可能设 `OLLAMA_HOST` 指向远端 server,
/// 或 `OLLAMA_API_KEY` 带 Bearer。
pub(crate) const ENV_OLLAMA_HOST: &str = "OLLAMA_HOST";
pub(crate) const ENV_OLLAMA_API_KEY: &str = "OLLAMA_API_KEY";

/// 组级 `api_key` 解析:组内显式值优先,再走固定 id 组的 env 兜底。
pub(crate) fn group_api_key(group: &ModelGroupSection) -> String {
    if let Some(key) = group.api_key.as_deref() {
        if !key.is_empty() {
            return key.to_string();
        }
    }
    match group.id.trim() {
        "anthropic" => std::env::var(ENV_ANTHROPIC_API_KEY).unwrap_or_default(),
        "openai" => std::env::var(ENV_OPENAI_API_KEY).unwrap_or_default(),
        "ollama" => std::env::var(ENV_OLLAMA_API_KEY).unwrap_or_default(),
        _ => String::new(),
    }
}

/// 组级 `base_url` 解析:组内显式值优先,再走 `ollama` 组的
/// `OLLAMA_HOST` 兜底。
pub(crate) fn group_base_url(group: &ModelGroupSection) -> Option<String> {
    if let Some(base) = group.base_url.as_deref() {
        if !base.trim().is_empty() {
            return Some(base.to_string());
        }
    }
    if group.id.trim() == "ollama" {
        return std::env::var(ENV_OLLAMA_HOST)
            .ok()
            .filter(|v| !v.trim().is_empty());
    }
    None
}

/// 模型组是否有可用凭据(与建池的注册条件同口径):组内 `credentials`
/// 数组非空,或组级 `api_key` / `base_url` 非空,或固定 id 组命中 env 兜底。
///
/// `endpoint = "ollama"` 是例外 —— 本地 `ollama serve` 不需要 key,
/// **声明了这个组**本身就是「我把这个端点接进来了」(v1 的 `[ollama]`
/// 段存在即算可用,口径一致)。
pub(crate) fn group_has_credential(group: &ModelGroupSection) -> bool {
    if group.endpoint.trim().eq_ignore_ascii_case("ollama") {
        return true;
    }
    !group.credentials.is_empty()
        || group.api_key.as_deref().is_some_and(|k| !k.is_empty())
        || group
            .base_url
            .as_deref()
            .is_some_and(|b| !b.trim().is_empty())
        || env_fallback_key(group).is_some()
}

/// 固定 id 组的 env 兜底 key(有值才返回)。
fn env_fallback_key(group: &ModelGroupSection) -> Option<String> {
    let name = match group.id.trim() {
        "anthropic" => ENV_ANTHROPIC_API_KEY,
        "openai" => ENV_OPENAI_API_KEY,
        "ollama" => ENV_OLLAMA_API_KEY,
        _ => return None,
    };
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// 池条目 label 归一:空 label 回落 `"default"`(组级隐式条目的既有约定)。
fn effective_label(label: &str) -> String {
    if label.is_empty() {
        "default".to_string()
    } else {
        label.to_string()
    }
}

/// 池条目 weight 归一:`0` 回落 `1`(weighted round-robin 不接受零权重)。
fn effective_weight(weight: u32) -> u32 {
    if weight == 0 { 1 } else { weight }
}

/// 把 `[[model_group]]` 条目展开为 `CredentialPool`(池名 = 组 id,
/// 由调用方注册)。
///
/// 注册条件:有 key 或有非空 base_url(免 Key 端点也要注册);都空 =
/// 「用户没配这个组」,不注册。`endpoint = "ollama"` 是例外 —— 声明了组
/// 就算接入(本地 `ollama serve` 不需要 key)。
///
/// `endpoint` 分派 `openai`(含 `responses_api`)/ `anthropic` / `ollama`;
/// 未知值 warn 后返回 `None` 跳过(坏值宽松吞掉,不让整份 config 失效)。
pub(crate) fn build_group_pool(
    group: &ModelGroupSection,
) -> Result<Option<CredentialPool>, ConfigError> {
    let endpoint = group.endpoint.trim().to_ascii_lowercase();
    if !matches!(endpoint.as_str(), "openai" | "anthropic" | "ollama") {
        tracing::warn!(
            group = %group.id,
            endpoint = %endpoint,
            "模型组 endpoint 未知(可选 openai | anthropic | ollama),跳过该组"
        );
        return Ok(None);
    }
    let key = group_api_key(group);
    let base_url = group_base_url(group);
    let has_custom_endpoint = base_url.as_deref().is_some_and(|b| !b.trim().is_empty());
    // ollama 端点:组声明本身就是「我把这个端点接进来了」—— 本地
    // `ollama serve` 不需要 key,组级单值全空也照常注册。
    let keyless_ok = endpoint == "ollama";
    let cred_configs: Vec<CredentialConfig> = if !group.credentials.is_empty() {
        group.credentials.clone()
    } else {
        if key.is_empty() && !has_custom_endpoint && !keyless_ok {
            return Ok(None);
        }
        vec![CredentialConfig {
            label: "default".to_string(),
            api_key: key.clone(),
            base_url: base_url.clone(),
            model: None,
            weight: 1,
            cooldown_override_secs: None,
            quota: None,
        }]
    };
    let default_timeout =
        Duration::from_secs(group.timeout_secs.unwrap_or(if endpoint == "ollama" {
            DEFAULT_OLLAMA_TIMEOUT_SECS
        } else {
            DEFAULT_TIMEOUT_SECS
        }));
    let default_model = group.model.clone().unwrap_or_default();
    let entries = match endpoint.as_str() {
        "openai" => build_openai_entries(
            &cred_configs,
            base_url.as_deref(),
            default_timeout,
            group.temperature,
            group.max_tokens,
            group.responses_api,
            &default_model,
        )?,
        "anthropic" => build_anthropic_entries(
            &cred_configs,
            base_url.as_deref(),
            default_timeout,
            group.temperature,
            group.max_tokens,
        )?,
        "ollama" => build_ollama_entries(
            &cred_configs,
            base_url.as_deref(),
            &key,
            default_timeout,
            group.temperature,
            group.max_tokens,
            group.keep_alive_secs,
            group.num_ctx,
            group.num_gpu,
            group.model.as_deref(),
        )?,
        // 上面的 matches! 已排除其余取值。
        _ => return Ok(None),
    };
    Ok(Some(CredentialPool { entries }))
}

/// 把一批凭据条目逐个构造为 `AnthropicClient` 并组装 `PoolEntry`。
fn build_anthropic_entries(
    cred_configs: &[CredentialConfig],
    default_base_url: Option<&str>,
    timeout: Duration,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
) -> Result<Vec<PoolEntry>, ConfigError> {
    let mut entries = Vec::with_capacity(cred_configs.len());
    for cfg in cred_configs {
        let client_cfg = AnthropicConfig {
            api_key: cfg.api_key.clone(),
            base_url: cfg
                .base_url
                .clone()
                .or_else(|| default_base_url.map(str::to_string)),
            timeout,
            // 组级采样参数默认值(请求侧显式值优先)。
            temperature,
            max_tokens,
        };
        let client =
            AnthropicClient::new(client_cfg).map_err(|e| ConfigError::Build(e.to_string()))?;
        entries.push(PoolEntry {
            client: Arc::new(client),
            label: effective_label(&cfg.label),
            weight: effective_weight(cfg.weight),
        });
    }
    Ok(entries)
}

/// 把一批凭据条目逐个构造为 OpenAI 系 client(`responses_api` 分派
/// Responses / Chat Completions)并组装 `PoolEntry`。
#[allow(clippy::too_many_arguments)]
fn build_openai_entries(
    cred_configs: &[CredentialConfig],
    default_base_url: Option<&str>,
    timeout: Duration,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    responses_api: bool,
    default_model: &str,
) -> Result<Vec<PoolEntry>, ConfigError> {
    let mut entries = Vec::with_capacity(cred_configs.len());
    for cfg in cred_configs {
        let base = cfg
            .base_url
            .clone()
            .or_else(|| default_base_url.map(str::to_string));
        // `responses_api = true` 时用 Responses client,否则 Chat
        // Completions(默认,向后兼容)。
        let client: Arc<dyn ModelClient> = if responses_api {
            let client_cfg = OpenAIResponsesConfig {
                api_key: cfg.api_key.clone(),
                base_url: base,
                timeout_secs: timeout.as_secs(),
                model: default_model.to_string(),
                temperature,
                max_tokens,
            };
            Arc::new(
                OpenAIResponsesClient::new(client_cfg)
                    .map_err(|e| ConfigError::Build(e.to_string()))?,
            )
        } else {
            let client_cfg = OpenAIConfig {
                api_key: cfg.api_key.clone(),
                base_url: base,
                timeout,
                temperature,
                max_tokens,
            };
            Arc::new(OpenAIClient::new(client_cfg).map_err(|e| ConfigError::Build(e.to_string()))?)
        };
        entries.push(PoolEntry {
            client,
            label: effective_label(&cfg.label),
            weight: effective_weight(cfg.weight),
        });
    }
    Ok(entries)
}

/// 把一批凭据条目逐个构造为 `OllamaClient` 并组装 `PoolEntry`。
#[allow(clippy::too_many_arguments)]
fn build_ollama_entries(
    cred_configs: &[CredentialConfig],
    default_base_url: Option<&str>,
    default_api_key: &str,
    timeout: Duration,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    keep_alive_secs: Option<i64>,
    num_ctx: Option<u32>,
    num_gpu: Option<u32>,
    model: Option<&str>,
) -> Result<Vec<PoolEntry>, ConfigError> {
    let mut entries = Vec::with_capacity(cred_configs.len());
    for cfg in cred_configs {
        let client_cfg = OllamaConfig {
            base_url: cfg
                .base_url
                .clone()
                .or_else(|| default_base_url.map(str::to_string)),
            api_key: if cfg.api_key.is_empty() {
                Some(default_api_key.to_string())
            } else {
                Some(cfg.api_key.clone())
            },
            keep_alive_secs,
            num_ctx,
            num_gpu,
            timeout,
            model: model.map(str::to_string),
            // 组级采样参数默认值(请求侧显式值优先)。
            temperature,
            max_tokens,
        };
        let client =
            OllamaClient::new(client_cfg).map_err(|e| ConfigError::Build(e.to_string()))?;
        entries.push(PoolEntry {
            client: Arc::new(client),
            label: effective_label(&cfg.label),
            weight: effective_weight(cfg.weight),
        });
    }
    Ok(entries)
}

// ── 子代理独立凭证池 ────────────────────────────────────────────

/// 把 `[subagent_models.anthropic]` 段展开为 `CredentialPool`,
/// 语义与主组一致但使用 subagent section(不回退 env)。
pub(crate) fn build_subagent_anthropic_pool(
    section: &AnthropicSubagentSection,
) -> Result<Option<CredentialPool>, ConfigError> {
    let cred_configs: Vec<CredentialConfig> = if !section.credentials.is_empty() {
        section.credentials.clone()
    } else {
        let key = section.api_key.clone().unwrap_or_default();
        if key.is_empty() {
            return Ok(None);
        }
        vec![CredentialConfig {
            label: "default".to_string(),
            api_key: key,
            base_url: section.base_url.clone(),
            model: None,
            weight: 1,
            cooldown_override_secs: None,
            quota: None,
        }]
    };
    let default_timeout = Duration::from_secs(section.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let mut entries = Vec::with_capacity(cred_configs.len());
    for cfg in &cred_configs {
        let client_cfg = AnthropicConfig {
            api_key: cfg.api_key.clone(),
            base_url: cfg.base_url.clone().or_else(|| section.base_url.clone()),
            timeout: default_timeout,
            // subagent 段无采样配置,显式 None(不走默认推导)。
            temperature: None,
            max_tokens: None,
        };
        let client =
            AnthropicClient::new(client_cfg).map_err(|e| ConfigError::Build(e.to_string()))?;
        entries.push(PoolEntry {
            client: Arc::new(client),
            label: effective_label(&cfg.label),
            weight: effective_weight(cfg.weight),
        });
    }
    Ok(Some(CredentialPool { entries }))
}

/// 把 `[subagent_models.openai]` 段展开为 `CredentialPool`
/// (不回退 env)。
pub(crate) fn build_subagent_openai_pool(
    section: &OpenAISubagentSection,
) -> Result<Option<CredentialPool>, ConfigError> {
    let cred_configs: Vec<CredentialConfig> = if !section.credentials.is_empty() {
        section.credentials.clone()
    } else {
        let key = section.api_key.clone().unwrap_or_default();
        if key.is_empty() {
            return Ok(None);
        }
        vec![CredentialConfig {
            label: "default".to_string(),
            api_key: key,
            base_url: section.base_url.clone(),
            model: None,
            weight: 1,
            cooldown_override_secs: None,
            quota: None,
        }]
    };
    let default_timeout = Duration::from_secs(section.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let mut entries = Vec::with_capacity(cred_configs.len());
    for cfg in &cred_configs {
        let client_cfg = OpenAIConfig {
            api_key: cfg.api_key.clone(),
            base_url: cfg.base_url.clone().or_else(|| section.base_url.clone()),
            timeout: default_timeout,
            // subagent 段无采样配置,显式 None(不走默认推导)。
            temperature: None,
            max_tokens: None,
        };
        let client =
            OpenAIClient::new(client_cfg).map_err(|e| ConfigError::Build(e.to_string()))?;
        entries.push(PoolEntry {
            client: Arc::new(client),
            label: effective_label(&cfg.label),
            weight: effective_weight(cfg.weight),
        });
    }
    Ok(Some(CredentialPool { entries }))
}

/// 把 `[subagent_models.ollama]` 段展开为 `CredentialPool`
/// (不回退 env)。
pub(crate) fn build_subagent_ollama_pool(
    section: &OllamaSubagentSection,
) -> Result<Option<CredentialPool>, ConfigError> {
    // Subagent Ollama 不回退 env —— 完全由 config 控制。
    let cred_configs: Vec<CredentialConfig> = if !section.credentials.is_empty() {
        section.credentials.clone()
    } else {
        vec![CredentialConfig {
            label: "default".to_string(),
            api_key: section.api_key.clone().unwrap_or_default(),
            base_url: section.base_url.clone(),
            model: None,
            weight: 1,
            cooldown_override_secs: None,
            quota: None,
        }]
    };
    let merged_timeout =
        Duration::from_secs(section.timeout_secs.unwrap_or(DEFAULT_OLLAMA_TIMEOUT_SECS));
    let mut entries = Vec::with_capacity(cred_configs.len());
    for cfg in &cred_configs {
        let client_cfg = OllamaConfig {
            base_url: cfg.base_url.clone().or_else(|| section.base_url.clone()),
            api_key: if cfg.api_key.is_empty() {
                section.api_key.clone()
            } else {
                Some(cfg.api_key.clone())
            },
            keep_alive_secs: section.keep_alive_secs,
            num_ctx: section.num_ctx,
            num_gpu: section.num_gpu,
            timeout: merged_timeout,
            model: section.model.clone(),
            // subagent 段无采样配置,显式 None(不走默认推导)。
            temperature: None,
            max_tokens: None,
        };
        let client =
            OllamaClient::new(client_cfg).map_err(|e| ConfigError::Build(e.to_string()))?;
        entries.push(PoolEntry {
            client: Arc::new(client),
            label: effective_label(&cfg.label),
            weight: effective_weight(cfg.weight),
        });
    }
    Ok(Some(CredentialPool { entries }))
}
