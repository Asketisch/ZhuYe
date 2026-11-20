//! v1 遗留的**固定模型接入段** —— 只用于「读入旧配置并迁移」,不参与
//! 序列化。
//!
//! v1 的配置把「可接入的端点」钉死在三个固定槽位上:
//!
//! ```toml
//! [anthropic]        # 固定段
//! api_key = "..."
//! [openai]
//! [ollama]
//! ```
//!
//! v2 起这三段不再是运行期概念 —— 模型组(`[[model_group]]`)是唯一的
//! 接入单元。但**旧配置必须仍能读入**,否则老用户的 api_key 会静默
//! 丢失(serde 未开 `deny_unknown_fields`,直接删字段 = 悄悄忽略)。
//!
//! 因此这三段以 `LegacySections` 的形式挂在 [`ReflectConfig`] 的
//! **私有**字段上(`#[serde(skip_serializing)]`):
//!
//! - **读**:旧 TOML 里的 `[anthropic]` / `[openai]` / `[ollama]` 被解析
//!   到这里;
//! - **迁**:`crate::load` 的 v1→v2 迁移把三段就地转成等价模型组
//!   (`[anthropic]` → `id="anthropic"` / `endpoint="anthropic"`,其余同理),
//!   转完即清空;
//! - **写**:该字段整体不参与序列化 —— 写回只有新格式。

use serde::{Deserialize, Serialize};

use super::provider::CredentialConfig;

/// v1 的三个固定接入段 —— 读入旧配置用,迁移后即丢弃。
#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct LegacySections {
    /// 旧 `[anthropic]` 段。
    #[serde(default)]
    pub anthropic: Option<AnthropicSection>,
    /// 旧 `[openai]` 段。
    #[serde(default)]
    pub openai: Option<OpenAISection>,
    /// 旧 `[ollama]` 段。
    #[serde(default)]
    pub ollama: Option<OllamaSection>,
}

impl LegacySections {
    /// 三段是否全空(全空 = 没有旧配置需要迁移)。
    pub fn is_empty(&self) -> bool {
        self.anthropic.is_none() && self.openai.is_none() && self.ollama.is_none()
    }
}

/// v1 `[anthropic]` 段 —— 迁移到 `id="anthropic"` 的模型组。
#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct AnthropicSection {
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub credentials: Vec<CredentialConfig>,
    /// 默认采样温度(请求侧显式值优先;缺省不发送该字段)。
    #[serde(default)]
    pub temperature: Option<f32>,
    /// 默认输出上限(请求侧显式值优先;缺省走 provider 默认)。
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// v1 `[openai]` 段 —— 迁移到 `id="openai"` 的模型组。
#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct OpenAISection {
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub credentials: Vec<CredentialConfig>,
    /// `true` 时 builder 用 `OpenAIResponsesClient`(`/v1/responses`)
    /// 替代 `OpenAIClient`(`/v1/chat/completions`)。
    #[serde(default)]
    pub responses_api: bool,
    /// 默认采样温度(请求侧显式值优先;缺省不发送该字段)。
    #[serde(default)]
    pub temperature: Option<f32>,
    /// 默认输出上限(请求侧显式值优先;缺省走 provider 默认)。
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// v1 `[ollama]` 段 —— 迁移到 `id="ollama"` 的模型组。
#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct OllamaSection {
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub keep_alive_secs: Option<i64>,
    #[serde(default)]
    pub num_ctx: Option<u32>,
    #[serde(default)]
    pub num_gpu: Option<u32>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub credentials: Vec<CredentialConfig>,
    /// 默认采样温度(请求侧显式值优先;缺省不发送该字段)。
    #[serde(default)]
    pub temperature: Option<f32>,
    /// 默认输出上限(映射为 `options.num_predict`;请求侧显式值优先)。
    #[serde(default)]
    pub max_tokens: Option<u32>,
}
