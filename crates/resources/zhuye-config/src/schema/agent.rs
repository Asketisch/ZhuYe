//! Agent / routing / subagent / coordinator / compact / token budget 配置。
//!
//! 包含:
//! - `RoutingSection` / `SpecSlotConfig`
//! - `SubagentProvidersSection` / `SubagentSpecConfig`
//! - `CoordinatorSection` / `CompactSection` / `TokenBudgetSection`

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::schema::provider::CredentialConfig;

// ── v1.0 多 Provider 路由:per-role 路由策略段 ─────────────────

/// v1.0 多 Provider 路由:`[routing]` 段。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RoutingSection {
    #[serde(default)]
    pub main: SpecSlotConfig,
    #[serde(default)]
    pub compact: SpecSlotConfig,
    #[serde(default)]
    pub subagent: SpecSlotConfig,
    /// 单次 LLM 调用的最大尝试次数(含同凭证重试 / 冷却 / failover)。
    /// 缺省用 `RoutingPolicy::default()` 的 10 —— 即「初始 1 次 + 最多
    /// 9 次自动重试」,瞬时错误(网络 / SSE 中断 / 5xx / 429)引擎内
    /// 自动重试,烧满才报错;调小可限制最坏情况下的空转时长。
    #[serde(default)]
    pub max_attempts: Option<u32>,
}

/// 单角色 slot 配置。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SpecSlotConfig {
    #[serde(default)]
    pub primary: Option<String>,
    #[serde(default)]
    pub fallbacks: Vec<String>,
    #[serde(default)]
    pub weights: Vec<u32>,
    /// v1.11 桌面#3:复杂度分级路由(`[routing.<role>.complexity]`)——
    /// 把思考强度档位(low/medium/high/max/ultra,即用户对「任务复杂度」
    /// 的表达)映射到不同模型 spec。配置后 `Op::SetEffort` / `/effort`
    /// 切档时**同时切模型**(未配置的档位回退 `primary`),实现「简单任务
    /// 用便宜快模型、复杂任务用旗舰模型」的分级路由。
    ///
    /// ⚠️ 缓存提示:同一路由组内切换模型会使 provider prompt cache
    /// **全部失效**(cache 键含模型名)。高频档位建议固定同一模型,
    /// `fallbacks` 仅用于可用性兜底;组间共享前缀的收益只在同模型时成立。
    #[serde(default)]
    pub complexity: Option<ComplexityRoutes>,
}

/// v1.11 桌面#3:effort 档位 → 模型 spec 映射表。
/// `None` 档位 = 回退 slot 的 `primary`。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ComplexityRoutes {
    #[serde(default)]
    pub low: Option<String>,
    #[serde(default)]
    pub medium: Option<String>,
    #[serde(default)]
    pub high: Option<String>,
    #[serde(default)]
    pub max: Option<String>,
    #[serde(default)]
    pub ultra: Option<String>,
}

impl ComplexityRoutes {
    /// 查某档位的映射模型(空串 = 未配置,调用方回退 primary)。
    pub fn route_for(&self, effort: zhuye_protocol::ReasoningEffortMirror) -> Option<&str> {
        let spec = match effort {
            zhuye_protocol::ReasoningEffortMirror::Low => &self.low,
            zhuye_protocol::ReasoningEffortMirror::Medium => &self.medium,
            zhuye_protocol::ReasoningEffortMirror::High => &self.high,
            zhuye_protocol::ReasoningEffortMirror::Max => &self.max,
            zhuye_protocol::ReasoningEffortMirror::Ultra => &self.ultra,
        };
        spec.as_deref().filter(|s| !s.trim().is_empty())
    }
}

// ── 子代理 provider 配置 ──────────────────────────────────────

/// `[subagent_providers]` 段。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubagentProvidersSection {
    #[serde(default)]
    pub anthropic: Option<AnthropicSubagentSection>,
    #[serde(default)]
    pub openai: Option<OpenAISubagentSection>,
    #[serde(default)]
    pub ollama: Option<OllamaSubagentSection>,
}

/// Subagent 专用的 Anthropic provider 配置。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AnthropicSubagentSection {
    #[serde(default)]
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub credentials: Vec<CredentialConfig>,
}

/// Subagent 专用的 OpenAI provider 配置。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct OpenAISubagentSection {
    #[serde(default)]
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub credentials: Vec<CredentialConfig>,
}

/// Subagent 专用的 Ollama provider 配置。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct OllamaSubagentSection {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub keep_alive_secs: Option<i64>,
    pub num_ctx: Option<u32>,
    pub num_gpu: Option<u32>,
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub credentials: Vec<CredentialConfig>,
}

// ── v1.x 功能 2:用户自定义 subagent spec ──────────────────────

/// v1.x 功能 2:`[[subagents]]` 数组里单条 subagent spec。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubagentSpecConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub role: String,
    pub model: Option<String>,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub allowed_skills: Vec<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
}

// ── 协调器配置 ──────────────────────────────────────────────────

/// v1.1.0 Phase 4:`[coordinator]` 段配置定义。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CoordinatorSection {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub system_prompt_path: Option<PathBuf>,
    #[serde(default)]
    pub max_workers: Option<u8>,
}

// ── 压缩与预算 ──────────────────────────────────────────────────

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CompactSection {
    pub trigger_tokens: Option<u32>,
}

/// v1.2 P1-12:Token 预算上限(`[token_budget]` 段)。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TokenBudgetSection {
    #[serde(default)]
    pub session_total_tokens: Option<u64>,
    #[serde(default)]
    pub per_turn_input_tokens: Option<u32>,
}
