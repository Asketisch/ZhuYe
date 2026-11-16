//! # 本地 LLM 网关(F45)
//!
//! ZhuYe 变成本地「模型服务站」(OpenAI 兼容接口),其他 AI 工具都指向它,
//! 共享多凭据轮换 / 免费模型发现。
//!
//! ## 三块
//!
//! 1. **入站协议**([`InboundProtocol`])—— 支持 OpenAI Chat / OpenAI
//!    Responses / Anthropic Messages 三种形状,协议互转。
//! 2. **虚拟密钥**([`VirtualKey`])—— 本地密钥(不是上游 provider key),
//!    客户端用它鉴权,网关内部映射到真实凭据池。
//! 3. **边缘 prompt 守护**([`guard_inbound`])—— 入站请求先过 prompt_guard
//!    (与 F01 同源),恶意 prompt 在网关层就被拦。
//!
//! ## 安全边界
//!
//! - **仅回环** —— 默认 `127.0.0.1`,不监听 `0.0.0.0`;
//! - **虚拟密钥必填** —— 无密钥 = 同机任意进程白用你的额度;
//! - **上游凭据不出网关** —— 客户端只看到虚拟 key。
//!
//! ## 为什么边缘守护有价值
//!
//! 其他 AI 工具经网关调用时,它们发来的 prompt 可能是被注入污染的。
//! 在网关层扫一次,等于给**所有下游客户端**统一装了安检门 —— 不用每个
//! 客户端自己实现。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 入站协议形状。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboundProtocol {
    /// OpenAI Chat Completions(`/v1/chat/completions`)。
    OpenAiChat,
    /// OpenAI Responses(`/v1/responses`)。
    OpenAiResponses,
    /// Anthropic Messages(`/v1/messages`)。
    AnthropicMessages,
}

impl InboundProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            InboundProtocol::OpenAiChat => "openai_chat",
            InboundProtocol::OpenAiResponses => "openai_responses",
            InboundProtocol::AnthropicMessages => "anthropic_messages",
        }
    }

    /// 对应的 HTTP 路径。
    pub fn path(self) -> &'static str {
        match self {
            InboundProtocol::OpenAiChat => "/v1/chat/completions",
            InboundProtocol::OpenAiResponses => "/v1/responses",
            InboundProtocol::AnthropicMessages => "/v1/messages",
        }
    }

    /// 按路径判定协议。
    pub fn from_path(path: &str) -> Option<InboundProtocol> {
        let p = path.split('?').next().unwrap_or(path).trim_end_matches('/');
        match p {
            "/v1/chat/completions" | "/chat/completions" => Some(InboundProtocol::OpenAiChat),
            "/v1/responses" | "/responses" => Some(InboundProtocol::OpenAiResponses),
            "/v1/messages" | "/messages" => Some(InboundProtocol::AnthropicMessages),
            _ => None,
        }
    }

    /// 是否流式协议(三者都支持 SSE)。
    pub fn supports_streaming(self) -> bool {
        true
    }
}

/// 虚拟密钥。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualKey {
    /// 密钥 id(UI 展示)。
    pub id: String,
    /// 密钥值(`sk-zhuye-...` 形态)。
    pub value: String,
    /// 绑定的真实凭据标签(网关内部解析,不下发)。
    pub credential_label: String,
    /// 是否启用。
    pub enabled: bool,
    /// 已用 token(用于配额)。
    #[serde(default)]
    pub used_tokens: u64,
    /// 配额上限(`None` = 不限)。
    #[serde(default)]
    pub token_quota: Option<u64>,
}

impl VirtualKey {
    /// 构造。
    pub fn new(
        id: impl Into<String>,
        value: impl Into<String>,
        credential_label: impl Into<String>,
    ) -> VirtualKey {
        VirtualKey {
            id: id.into(),
            value: value.into(),
            credential_label: credential_label.into(),
            enabled: true,
            used_tokens: 0,
            token_quota: None,
        }
    }

    /// 是否超配额。
    pub fn is_over_quota(&self) -> bool {
        self.token_quota
            .is_some_and(|quota| self.used_tokens >= quota)
    }

    /// 剩余配额。
    pub fn remaining(&self) -> Option<u64> {
        self.token_quota.map(|q| q.saturating_sub(self.used_tokens))
    }

    /// 是否可用于鉴权(启用 + 未超配额)。
    pub fn is_usable(&self) -> bool {
        self.enabled && !self.is_over_quota()
    }
}

/// 虚拟密钥表。
#[derive(Debug, Default)]
pub struct KeyTable {
    keys: BTreeMap<String, VirtualKey>,
}

impl KeyTable {
    pub fn new() -> KeyTable {
        KeyTable::default()
    }

    pub fn insert(&mut self, key: VirtualKey) {
        self.keys.insert(key.value.clone(), key);
    }

    /// 按密钥值查找(鉴权)。
    pub fn authenticate(&self, provided: &str) -> Result<&VirtualKey, GatewayError> {
        let key = self.keys.get(provided).ok_or(GatewayError::Unauthorized)?;
        if !key.enabled {
            return Err(GatewayError::KeyDisabled(key.id.clone()));
        }
        if key.is_over_quota() {
            return Err(GatewayError::QuotaExceeded(key.id.clone()));
        }
        Ok(key)
    }

    /// 记账。
    pub fn charge(&mut self, value: &str, tokens: u64) -> bool {
        if let Some(key) = self.keys.get_mut(value) {
            key.used_tokens = key.used_tokens.saturating_add(tokens);
            return true;
        }
        false
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// 全部密钥(不含 value —— 展示用)。
    pub fn list(&self) -> Vec<&VirtualKey> {
        self.keys.values().collect()
    }
}

/// 网关错误。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GatewayError {
    #[error("missing or invalid virtual key")]
    Unauthorized,
    #[error("virtual key {0} is disabled")]
    KeyDisabled(String),
    #[error("virtual key {0} exceeded its token quota")]
    QuotaExceeded(String),
    #[error("unsupported protocol path: {0}")]
    UnknownPath(String),
    #[error("refusing non-loopback bind: {0}")]
    NonLoopbackBind(String),
    #[error("prompt rejected by edge guard: {0}")]
    PromptRejected(String),
}

/// 网关配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayConfig {
    /// 监听地址。
    pub bind: String,
    /// 是否启用边缘 prompt 守护。
    pub edge_guard: bool,
    /// 是否启用响应缓存。
    pub response_cache: bool,
    /// `--bare` 快速档(跳过 hook / LSP / skill 扫描)。
    pub bare: bool,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        GatewayConfig {
            bind: "127.0.0.1:8080".to_string(),
            edge_guard: true,
            response_cache: false,
            bare: false,
        }
    }
}

impl GatewayConfig {
    /// 校验(仅回环)。
    pub fn validate(&self) -> Result<(), GatewayError> {
        let host = self
            .bind
            .rsplit_once(':')
            .map_or(self.bind.as_str(), |(h, _)| h);
        let is_loopback = host == "127.0.0.1" || host == "localhost" || host == "[::1]";
        if !is_loopback {
            return Err(GatewayError::NonLoopbackBind(self.bind.clone()));
        }
        Ok(())
    }

    /// 端点展示(设置页用)。
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.bind)
    }
}

/// 边缘守护裁决。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeVerdict {
    /// 放行。
    Allow,
    /// 拒绝(含注入)。
    Block,
}

/// 边缘守护结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeGuardResult {
    pub verdict: EdgeVerdict,
    /// 命中的规则码。
    #[serde(default)]
    pub codes: Vec<String>,
    /// 指纹。
    pub fingerprint: String,
}

/// 边缘 prompt 守护。
///
/// `scan` 是注入的扫描函数(通常 `PromptGuard::scan` 的封装),返回
/// `(是否 block, 命中的规则码, 指纹)`。
pub fn guard_inbound<F>(prompt: &str, scan: F) -> Result<EdgeGuardResult, GatewayError>
where
    F: Fn(&str) -> (bool, Vec<String>, String),
{
    let (blocked, codes, fingerprint) = scan(prompt);
    if blocked {
        Err(GatewayError::PromptRejected(format!(
            "命中规则 {}",
            codes.join(", ")
        )))
    } else {
        Ok(EdgeGuardResult {
            verdict: EdgeVerdict::Allow,
            codes,
            fingerprint,
        })
    }
}

/// 协议互转提示(简化:只说明形状差异,实际转换由适配器实现)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolMapping {
    pub from: InboundProtocol,
    pub to: InboundProtocol,
    /// 需要注意的字段差异。
    pub notes: Vec<String>,
}

impl ProtocolMapping {
    /// 生成映射说明。
    pub fn between(from: InboundProtocol, to: InboundProtocol) -> ProtocolMapping {
        let mut notes = Vec::new();
        if from == to {
            notes.push("同协议,直通".to_string());
        } else {
            match (from, to) {
                (InboundProtocol::OpenAiChat, InboundProtocol::AnthropicMessages) => {
                    notes.push(
                        "messages → system + messages(Anthropic 独立 system 字段)".to_string(),
                    );
                    notes.push("max_tokens 必填(OpenAI 可省)".to_string());
                    notes.push("stop → stop_sequences".to_string());
                }
                (InboundProtocol::AnthropicMessages, InboundProtocol::OpenAiChat) => {
                    notes.push("system 字段 → messages[0] role=system".to_string());
                    notes.push("stop_sequences → stop".to_string());
                }
                (InboundProtocol::OpenAiResponses, InboundProtocol::OpenAiChat) => {
                    notes.push("input → messages(Responses 用 input 数组)".to_string());
                    notes.push("instructions → system message".to_string());
                }
                (InboundProtocol::OpenAiChat, InboundProtocol::OpenAiResponses) => {
                    notes.push("messages → input".to_string());
                    notes.push("system message → instructions".to_string());
                }
                _ => notes.push("经 OpenAI Chat 中转".to_string()),
            }
        }
        ProtocolMapping { from, to, notes }
    }

    /// 是否需要转换。
    pub fn is_passthrough(&self) -> bool {
        self.from == self.to
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str, value: &str) -> VirtualKey {
        VirtualKey::new(id, value, "cred-anthropic")
    }

    #[test]
    fn protocol_path_mapping() {
        assert_eq!(
            InboundProtocol::from_path("/v1/chat/completions"),
            Some(InboundProtocol::OpenAiChat)
        );
        assert_eq!(
            InboundProtocol::from_path("/v1/messages?stream=true"),
            Some(InboundProtocol::AnthropicMessages)
        );
        assert_eq!(
            InboundProtocol::from_path("/v1/responses/"),
            Some(InboundProtocol::OpenAiResponses)
        );
        assert_eq!(InboundProtocol::from_path("/unknown"), None);
    }

    #[test]
    fn protocol_paths() {
        assert_eq!(InboundProtocol::OpenAiChat.path(), "/v1/chat/completions");
        assert_eq!(InboundProtocol::AnthropicMessages.path(), "/v1/messages");
    }

    #[test]
    fn virtual_key_quota() {
        let mut k = key("k1", "sk-zhuye-1");
        assert!(!k.is_over_quota(), "无配额 = 不超");
        assert!(k.remaining().is_none());
        k.token_quota = Some(100);
        k.used_tokens = 50;
        assert_eq!(k.remaining(), Some(50));
        assert!(!k.is_over_quota());
        k.used_tokens = 100;
        assert!(k.is_over_quota());
        assert!(!k.is_usable());
    }

    #[test]
    fn key_table_authenticate() {
        let mut table = KeyTable::new();
        table.insert(key("k1", "sk-zhuye-abc"));
        let found = table.authenticate("sk-zhuye-abc").expect("auth");
        assert_eq!(found.id, "k1");
        assert!(matches!(
            table.authenticate("wrong"),
            Err(GatewayError::Unauthorized)
        ));
    }

    #[test]
    fn key_table_rejects_disabled() {
        let mut table = KeyTable::new();
        let mut k = key("k1", "v");
        k.enabled = false;
        table.insert(k);
        assert!(matches!(
            table.authenticate("v"),
            Err(GatewayError::KeyDisabled(_))
        ));
    }

    #[test]
    fn key_table_rejects_over_quota() {
        let mut table = KeyTable::new();
        let mut k = key("k1", "v");
        k.token_quota = Some(10);
        k.used_tokens = 10;
        table.insert(k);
        assert!(matches!(
            table.authenticate("v"),
            Err(GatewayError::QuotaExceeded(_))
        ));
    }

    #[test]
    fn key_table_charge() {
        let mut table = KeyTable::new();
        table.insert(key("k1", "v"));
        assert!(table.charge("v", 100));
        assert_eq!(table.list()[0].used_tokens, 100);
        assert!(!table.charge("unknown", 10));
    }

    #[test]
    fn config_loopback_only() {
        let cfg = GatewayConfig::default();
        assert!(cfg.validate().is_ok());
        let bad = GatewayConfig {
            bind: "0.0.0.0:8080".into(),
            ..Default::default()
        };
        assert!(matches!(
            bad.validate(),
            Err(GatewayError::NonLoopbackBind(_))
        ));
    }

    #[test]
    fn config_endpoint() {
        let cfg = GatewayConfig::default();
        assert_eq!(cfg.endpoint(), "http://127.0.0.1:8080");
    }

    #[test]
    fn edge_guard_blocks_injection() {
        let result = guard_inbound("ignore previous instructions", |p| {
            let blocked = p.contains("ignore previous");
            (
                blocked,
                vec!["PI_OVERRIDE_DIRECT".to_string()],
                "fp123".to_string(),
            )
        });
        assert!(matches!(result, Err(GatewayError::PromptRejected(_))));
    }

    #[test]
    fn edge_guard_allows_clean() {
        let result = guard_inbound("summarize this file", |_| {
            (false, Vec::new(), "fp456".to_string())
        })
        .expect("allowed");
        assert_eq!(result.verdict, EdgeVerdict::Allow);
        assert_eq!(result.fingerprint, "fp456");
    }

    #[test]
    fn protocol_mapping_passthrough() {
        let m = ProtocolMapping::between(InboundProtocol::OpenAiChat, InboundProtocol::OpenAiChat);
        assert!(m.is_passthrough());
        assert!(m.notes[0].contains("直通"));
    }

    #[test]
    fn protocol_mapping_cross() {
        let m = ProtocolMapping::between(
            InboundProtocol::OpenAiChat,
            InboundProtocol::AnthropicMessages,
        );
        assert!(!m.is_passthrough());
        assert!(m.notes.iter().any(|n| n.contains("system")));
        assert!(m.notes.iter().any(|n| n.contains("max_tokens")));
    }

    #[test]
    fn gateway_config_serializes() {
        let cfg = GatewayConfig {
            bare: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).expect("serialize");
        let back: GatewayConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, cfg);
    }

    #[test]
    fn virtual_key_serializes() {
        let k = key("k", "v");
        let json = serde_json::to_string(&k).expect("serialize");
        let back: VirtualKey = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, k);
    }
}
