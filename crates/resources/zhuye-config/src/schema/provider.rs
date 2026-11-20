//! 凭据 / 配额的共享类型 —— `CredentialConfig`、`QuotaConfig`、
//! `QuotaSource`。
//!
//! v1 的三个固定接入段(`AnthropicSection` / `OpenAISection` /
//! `OllamaSection`)已移入 [`super::legacy`]:它们只用于读入旧配置并
//! 迁移,不再是运行期概念。

use serde::{Deserialize, Serialize};

// ── 共享类型 ──────────────────────────────────────────────────

/// 多端点轮换:`[[<组>.credentials]]` 数组里单条凭证的 schema 镜像。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CredentialConfig {
    /// 必填(用户诊断标识);缺省时 builder 兜底 `"env-N"` 或 `"default"`。
    pub label: String,
    /// 凭据密钥。
    ///
    /// `#[serde(default)]` —— **可以整条省略**。免 Key 的 OpenAI 兼容端点
    /// (免费模型池)是真实存在的接入形态:此前本字段是必填,GUI 的 plan
    /// 编辑器在 key 为空时**整行省略** `api_key`,写出的 TOML 直接反序列化
    /// 失败(整份配置保存被拒),于是「免 Key 端点」在 GUI 里根本无法表达。
    /// 缺省 = 空串 = 该凭据不带 Authorization 头(`OpenAIClient` 的既有约定)。
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub base_url: Option<String>,
    /// 该 plan 的模型名。解析优先级:env `ZHUYE_MODEL` > 被钉住条目的
    /// `model` > 组级 `model` 覆盖 > 未配置(不编造内置默认)。
    /// GUI 的 coding plan 编辑器把 model 写在这里而非组级。
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// 覆盖 `RoutingPolicy` 全局 cooldown 默认值,单位秒。
    #[serde(default)]
    pub cooldown_override_secs: Option<u64>,
    /// 可选的 token plan 配额声明。
    #[serde(default)]
    pub quota: Option<QuotaConfig>,
}

fn default_weight() -> u32 {
    1
}

/// token plan 配额声明。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct QuotaConfig {
    /// 配额重置周期(秒)。如 5h = 18000。
    pub window_secs: u64,
    /// 窗口内最大 token 数(input + output)。
    pub max_tokens: u64,
    /// 配额数据来源。`None` = 本地统计(默认);`Some` = 调厂商 quota API。
    #[serde(default)]
    pub check_via: Option<QuotaSource>,
}

/// 厂商 quota API 路由。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaSource {
    Kimi,
    Zhipu,
    Minimax,
    Zenmux,
    Volcengine,
    AnthropicUsage,
    OpenAIUsage,
}

impl CredentialConfig {
    /// 转为 `zhuye_llm::Credential`。
    pub fn to_credential(&self) -> zhuye_llm::Credential {
        use std::time::Duration;
        zhuye_llm::Credential {
            label: self.label.clone(),
            api_key: self.api_key.clone(),
            base_url: self.base_url.clone(),
            weight: if self.weight == 0 { 1 } else { self.weight },
            cooldown_override: self.cooldown_override_secs.map(Duration::from_secs),
        }
    }
}
