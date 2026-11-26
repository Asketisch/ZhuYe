//! zhuye-llm — ModelClient trait, Capabilities, provider 实现,以及 v1.0
//! 多 Provider 路由(`ModelRegistry` credential pool / `Credential` /
//! `RoutingPolicy` / `CooldownEntry`)。
//!
//! v0.3.1 内置三个 provider:OpenAI(Chat Completions)、Anthropic
//! (Messages)、Ollama(原生 `/api/chat` NDJSON 流)。
//! Responses API / MCP 推迟至 v1+ 引入。
//!
//! v1.0: 新增 `Credential` / `CredentialPool` / `RoutingPolicy` /
//! `CooldownEntry` 支持 round-robin credentials + cooldown + per-role
//! 路由。`ModelRegistry` 从单值 `HashMap<name, client>` 升级为
//! `HashMap<name, CredentialPool>` 池化结构,旧 `register` / `get` /
//! `resolve` / `list` API 保留(部分 `#[deprecated]`)。

/// 计费与用量记账（账号池 / 配额 / 成本账本 / 凭证冷却）。
pub mod billing;
/// v1.x:prompt cache 保温(cache warming)决策 —— 纯函数,零 I/O。
pub mod cache_warming;
pub mod capabilities;
pub mod client;
pub mod error;
pub mod event;
pub mod model_catalog;
/// models.dev 社区模型目录(窗口 / 输出上限 / 视觉 / 思考档位)。
pub mod models_dev;
pub mod output_budget;
pub mod policy;
pub mod providers;
pub mod reasoning_effort;
pub mod registry;
pub mod request;
/// F43:动态选模(回合分类 + 迟滞 + 角色绑定 + 能力槽位)。
pub mod routing;
/// v1.x:strict schema 折叠(不支持约束写进 description,双保险)。
pub mod schema_fold;

pub use billing::accounts;
pub use billing::cooldown;
pub use billing::credential;
pub use billing::quota;
pub use billing::spend;
pub use cache_warming::{
    CacheObservation, CacheWarmingDecision, CacheWarmingSettings, METADATA_WARM_REPLAY,
    METADATA_WARM_REPLAY_VALUE, SkipReason, WarmingAction, WarmingPhase, effective_ttl_secs,
    evaluate, refresh_delay, refresh_delay_secs,
};
pub use capabilities::{Capabilities, ProviderKind};
pub use client::ModelClient;
pub use cooldown::{CooldownEntry, CooldownReason};
pub use credential::Credential;
pub use error::LlmError;
pub use event::ChatEvent;
pub use model_catalog::{DEFAULT_ALWAYS_FREE, DEFAULT_FREE_ID_PATTERNS, FreeModelRules, ModelInfo};
pub use models_dev::{
    CACHE_TTL_SECS, DevCatalog, LEAST_WINDOW, MODELS_DEV_URL, ModelAbilities, abilities_for,
    enrich_model_info, install_catalog, refresh, refresh_if_stale, with_snapshot_catalog,
    with_test_catalog,
};
pub use policy::{Role, RoutingPolicy, SpecSlot};
pub use providers::{
    AnthropicClient, AnthropicConfig, DEFAULT_MOCK_MODEL, DEFAULT_MOCK_REPLY, ENV_MOCK_SCRIPT,
    KimiQuotaProvider, MinimaxQuotaProvider, MockClient, MockReply, ModelPricing, OllamaClient,
    OllamaConfig, OpenAIClient, OpenAIConfig, OpenAIResponsesClient, OpenAIResponsesConfig,
    QuotaError, QuotaProvider, QuotaSnapshot, RateCard, ZenmuxQuotaProvider, ZhipuQuotaProvider,
    context_window_for, context_window_override_for, cumulative_cost_usd, is_priced,
    normalize_model_spec, price, pricing_for, pricing_table, rate_card, rate_cards,
    set_context_window_overrides,
};
pub use quota::{CredentialKey, QuotaConfig, QuotaSource, QuotaTracker, SharedQuotaTracker};
pub use reasoning_effort::{
    DEFAULT_MAX_EFFORT, clamp_effort_for_model, effort_menu_for_model, effort_wire_str,
    max_effort_for_model, supported_efforts_for_model,
};
pub use registry::{CredentialPool, ModelRegistry, NextClient, PoolEntry, SharedModelRegistry};
pub use request::{
    AssistantContent, CacheBreak, CacheControl, CacheControlKind, CacheTtl, ChatMessage,
    ChatRequest, ContentBlock, ReasoningEffort, ResponseFormat, SystemBlock, SystemBlocks,
    ThinkingConfig, ToolCallRequest, ToolResult, ToolSpec, UserContent,
};
pub use schema_fold::{FoldedSchema, fold_to_strict, notes_to_description};
