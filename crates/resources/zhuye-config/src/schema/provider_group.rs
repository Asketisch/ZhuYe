//! 模型组(`[[provider_group]]`)—— 命名 provider 条目,模型 spec 前缀。
//!
//! ## 背景
//!
//! 内置三段(`[anthropic]` / `[openai]` / `[ollama]`)把「可接入的端点」
//! 钉死在固定协议槽位上;第三方聚合网关(Magpie / OpenRouter / 各家中转)
//! 的接入形态是**任意多个命名端点,每个端点自带协议格式**。模型组把这个
//! 形态升为一等公民:
//!
//! ```toml
//! [[provider_group]]
//! id = "bigmodel"                 # 组 id = 模型 spec 前缀(`<id>/<model>`)
//! endpoint = "openai"             # 线格式:openai | anthropic
//! base_url = "https://open.bigmodel.cn/api/paas/v4"
//! api_key = "..."
//! model = "GLM-5.3-Flash"         # 组默认模型(支持 "free" 动态值)
//! ```
//!
//! 组 id 直接注册为 `ModelRegistry` 的池名 —— 热重载 / 冷却 failover /
//! 清单发现(`model = "free"`)全部因「池名 = 组 id」自动兼容。内置段
//! 保持原样(向后兼容,旧配置零迁移)。

use serde::{Deserialize, Serialize};

use super::provider::CredentialConfig;

/// 模型组 —— `[[provider_group]]` 数组条目。
///
/// 组内单凭据是常态(组 = 端点 + key);`credentials` 数组保留给
/// 「同一组多把 key 加权轮换」的高级形态,字段语义与 `[[<provider>.credentials]]`
/// 完全一致(复用 [`CredentialConfig`])。
#[derive(Debug, Default, Clone, PartialEq, Deserialize, Serialize)]
pub struct ProviderGroupSection {
    /// 组 id。必填;**模型 spec 前缀**(`<id>/<model>`),因此不得包含
    /// `/`,且不得与内置段名(anthropic/openai/ollama/mock)或其他组
    /// 冲突 —— 违例时 builder 跳过该组并 warn,不让整份配置失效。
    #[serde(default)]
    pub id: String,
    /// 展示名(GUI 卡片 / CLI 输出用)。缺省回退 `id`。
    #[serde(default)]
    pub label: Option<String>,
    /// 线格式:`"openai"`(Chat Completions,`responses_api = true` 时
    /// `/v1/responses`)或 `"anthropic"`(Messages)。未知值 builder 跳过
    /// 该组并 warn(与「坏值宽松吞掉」的既有口径一致)。
    #[serde(default)]
    pub endpoint: String,
    /// 组凭据密钥。空 = 免 Key 端点(不发认证头,与内置段同语义)。
    #[serde(default)]
    pub api_key: Option<String>,
    /// 组端点 base_url。
    #[serde(default)]
    pub base_url: Option<String>,
    /// 组默认模型。解析优先级:env `ZHUYE_MODEL` > 被钉住条目的 `model`
    /// > 本字段。支持 `"free"` 动态值(运行时经组池拉清单挑选)。
    #[serde(default)]
    pub model: Option<String>,
    /// `endpoint = "openai"` 时 `true` 用 `/v1/responses` 替代
    /// `/v1/chat/completions`(对齐 `[openai].responses_api`)。
    #[serde(default)]
    pub responses_api: bool,
    /// 覆盖默认 HTTP 超时(秒)。
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// 默认采样温度(请求侧显式值优先)。
    #[serde(default)]
    pub temperature: Option<f32>,
    /// 默认输出上限(请求侧显式值优先)。
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// 组内多凭据池(高级)。缺省 = 单凭据组(builder 把组级
    /// `api_key`/`base_url` wrap 成 label `"default"` 的单条目)。
    #[serde(default)]
    pub credentials: Vec<CredentialConfig>,
}

impl ProviderGroupSection {
    /// builder 侧的合法 id 校验:非空、不含 `/`、不含表头歧义字符
    /// (`[`/`]` 会破坏 TOML 数组表头),不与内置保留名冲突。
    pub fn is_valid_id(id: &str) -> bool {
        !id.is_empty()
            && !id.contains('/')
            && !id.contains('[')
            && !id.contains(']')
            && !matches!(id, "anthropic" | "openai" | "ollama" | "mock")
    }

    /// 展示名:显式 `label` 优先,缺省回退 `id`。
    pub fn display_label(&self) -> &str {
        self.label
            .as_deref()
            .filter(|l| !l.trim().is_empty())
            .unwrap_or(&self.id)
    }
}
