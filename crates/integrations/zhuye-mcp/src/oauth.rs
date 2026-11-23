//! # MCP 协议完整性扩展(F29)
//!
//! 补三块让远程 MCP 服务器真正可用的协议能力:
//!
//! 1. **OAuth 客户端**([`OAuthConfig`] / [`OAuthFlow`])—— 很多远程 MCP
//!    服务器要登录。实现发现 + PKCE + 回环回调 + 刷新令牌的形状。
//! 2. **Elicitation**([`ElicitationRequest`])—— 服务器会主动向用户提问
//!    (「请提供 API 端点」)。请求路由到本机审批 / ask_user。
//! 3. **Sampling**([`SamplingRequest`])—— 服务器请求宿主帮忙调一次模型。
//!    **必须走审批**(服务器不该能免费花用户的 token)。
//!
//! ## 安全约束(三条硬规则)
//!
//! - **Sampling 一律要审批** —— 否则恶意 MCP server 可以无限调用模型烧钱。
//! - **Elicitation 的 `secret` 字段不回显** —— 密码类输入不能进日志/上下文。
//! - **OAuth token 存 secrets 而非 config** —— 与 F08 一致,token 是凭据。
//!
//! ## 为什么 PKCE 是必须的
//!
//! 本地回环回调(`http://127.0.0.1:port/callback`)在同机其它进程可监听。
//! 没有 PKCE,攻击者截获 code 就能换 token。PKCE 的 `code_verifier` 只存在
//! 于发起方内存中,截获 code 也换不到 token。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// OAuth 授权服务器元数据(发现结果)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthMetadata {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    /// 可选:动态客户端注册端点。
    #[serde(default)]
    pub registration_endpoint: Option<String>,
    /// 支持的 scopes。
    #[serde(default)]
    pub scopes_supported: Vec<String>,
    /// 是否要求 PKCE(现代服务器应为 true)。
    #[serde(default)]
    pub require_pkce: bool,
}

/// OAuth 客户端配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthConfig {
    /// 客户端 id(动态注册或预置)。
    pub client_id: String,
    /// 客户端密钥(公开客户端通常为空)。
    #[serde(default)]
    pub client_secret: Option<String>,
    /// 请求的 scope。
    #[serde(default)]
    pub scopes: Vec<String>,
    /// 回调端口(`0` = 让系统分配)。
    #[serde(default)]
    pub callback_port: u16,
}

impl OAuthConfig {
    /// 是否公开客户端(无 secret)。
    pub fn is_public_client(&self) -> bool {
        self.client_secret.is_none()
    }

    /// 回调地址。
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/callback", self.callback_port)
    }

    /// scope 拼成空格分隔串(OAuth 规范)。
    pub fn scope_string(&self) -> String {
        self.scopes.join(" ")
    }
}

/// OAuth 流程阶段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthStage {
    /// 已发现元数据。
    Discovered,
    /// 已生成授权 URL,等用户在浏览器完成授权。
    AwaitingCallback,
    /// 已拿到 code,正在换 token。
    ExchangingCode,
    /// 已拿到 token。
    Authorized,
    /// 失败。
    Failed { reason: String },
}

/// 已获得的令牌。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthToken {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// 过期时间(UNIX 秒;`None` = 不过期/未知)。
    #[serde(default)]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub scope: Option<String>,
}

impl OAuthToken {
    /// 在 `now` 时刻是否已过期(留 60 秒余量)。
    pub fn is_expired_at(&self, now: u64) -> bool {
        match self.expires_at {
            Some(deadline) => now + 60 >= deadline,
            None => false,
        }
    }

    /// 是否可刷新。
    pub fn can_refresh(&self) -> bool {
        self.refresh_token.is_some()
    }
}

/// PKCE 挑战对。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkceChallenge {
    /// `code_verifier`(只存内存,永不落盘/进日志)。
    pub verifier: String,
    /// `code_challenge`(S256(verifier))。
    pub challenge: String,
    /// 固定 `"S256"`。
    pub method: String,
}

impl PkceChallenge {
    /// 从 verifier 构造(调用方生成随机 verifier)。
    ///
    /// `hash_fn` 由调用方注入(通常 SHA-256 + base64url);本 crate 不引
    /// 加密依赖。
    pub fn from_verifier<F>(verifier: impl Into<String>, hash_fn: F) -> PkceChallenge
    where
        F: Fn(&str) -> String,
    {
        let verifier = verifier.into();
        let challenge = hash_fn(&verifier);
        PkceChallenge {
            verifier,
            challenge,
            method: "S256".to_string(),
        }
    }

    /// 生成授权 URL 的查询参数。
    pub fn auth_params(&self) -> BTreeMap<String, String> {
        let mut params = BTreeMap::new();
        params.insert("code_challenge".into(), self.challenge.clone());
        params.insert("code_challenge_method".into(), self.method.clone());
        params
    }
}

/// 动态客户端注册请求(CIMD / RFC 7591)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientRegistrationRequest {
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    /// `"none"` = 公开客户端。
    pub token_endpoint_auth_method: String,
}

impl ClientRegistrationRequest {
    /// 公开客户端注册(本地 CLI 的标准形态)。
    pub fn public(client_name: impl Into<String>, redirect_uri: impl Into<String>) -> Self {
        ClientRegistrationRequest {
            client_name: client_name.into(),
            redirect_uris: vec![redirect_uri.into()],
            token_endpoint_auth_method: "none".to_string(),
        }
    }
}

// ── Elicitation ──────────────────────────────────────────────────────────

/// 服务器向用户提问的字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElicitationField {
    pub name: String,
    pub description: String,
    /// 是否密码类(**不得回显**)。
    #[serde(default)]
    pub secret: bool,
    /// 是否必填。
    #[serde(default)]
    pub required: bool,
}

/// Elicitation 请求(服务器 → 用户)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElicitationRequest {
    pub request_id: String,
    /// 服务器名(UI 显示"谁在问")。
    pub server: String,
    /// 给用户看的说明。
    pub message: String,
    pub fields: Vec<ElicitationField>,
}

impl ElicitationRequest {
    /// 必填字段列表。
    pub fn required_fields(&self) -> Vec<&ElicitationField> {
        self.fields.iter().filter(|f| f.required).collect()
    }

    /// 是否含密码类字段(UI 应遮罩输入)。
    pub fn has_secret_fields(&self) -> bool {
        self.fields.iter().any(|f| f.secret)
    }

    /// 校验用户提交的答案:必填非空、不含未知字段。
    pub fn validate_answers(
        &self,
        answers: &BTreeMap<String, String>,
    ) -> Result<(), ElicitationError> {
        for field in &self.fields {
            if field.required {
                match answers.get(&field.name) {
                    Some(v) if !v.trim().is_empty() => {}
                    _ => {
                        return Err(ElicitationError::MissingRequired(field.name.clone()));
                    }
                }
            }
        }
        let known: std::collections::HashSet<&str> =
            self.fields.iter().map(|f| f.name.as_str()).collect();
        for key in answers.keys() {
            if !known.contains(key.as_str()) {
                return Err(ElicitationError::UnknownField(key.clone()));
            }
        }
        Ok(())
    }
}

/// Elicitation 错误。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ElicitationError {
    #[error("missing required field: {0}")]
    MissingRequired(String),
    #[error("unknown field in answers: {0}")]
    UnknownField(String),
}

// ── Sampling ─────────────────────────────────────────────────────────────

/// 服务器请求宿主调一次模型。
///
/// **必须走审批** —— 服务器不该能免费花用户的 token。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SamplingRequest {
    pub request_id: String,
    pub server: String,
    /// 请求的提示词(截断后的预览,用于审批 UI)。
    pub prompt_preview: String,
    /// 请求的最大 token 数。
    pub max_tokens: u64,
    /// 服务器声明的用途(便于用户判断)。
    #[serde(default)]
    pub purpose: Option<String>,
}

impl SamplingRequest {
    /// 审批 UI 的一行摘要。
    pub fn approval_summary(&self) -> String {
        let purpose = self.purpose.as_deref().unwrap_or("(未声明用途)");
        format!(
            "{} 请求调用模型:{} · 最多 {} tokens",
            self.server, purpose, self.max_tokens
        )
    }

    /// 该请求是否明显异常(供审批 UI 标红)。
    ///
    /// 判据:请求 token 数异常大(> 32k)或未声明用途。
    pub fn is_suspicious(&self) -> bool {
        self.max_tokens > 32_768 || self.purpose.as_deref().unwrap_or("").trim().is_empty()
    }
}

/// Sampling 裁决(宿主侧)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SamplingDecision {
    /// 允许(调用模型)。
    Allow,
    /// 拒绝(不调用,回错误)。
    Deny,
}

impl SamplingDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            SamplingDecision::Allow => "allow",
            SamplingDecision::Deny => "deny",
        }
    }
}

/// 资源订阅状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceSubscription {
    pub uri: String,
    /// 订阅是否仍然有效。
    pub active: bool,
    /// 最近一次收到变更通知的时间。
    #[serde(default)]
    pub last_notified_at: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_s256(s: &str) -> String {
        // 测试用:确定性伪哈希(不引 sha2)。
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in s.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        format!("{h:016x}")
    }

    #[test]
    fn public_client_has_no_secret() {
        let cfg = OAuthConfig {
            client_id: "abc".into(),
            client_secret: None,
            scopes: vec!["read".into(), "write".into()],
            callback_port: 0,
        };
        assert!(cfg.is_public_client());
        assert_eq!(cfg.redirect_uri(), "http://127.0.0.1:0/callback");
        assert_eq!(cfg.scope_string(), "read write");
    }

    #[test]
    fn confidential_client_has_secret() {
        let cfg = OAuthConfig {
            client_id: "abc".into(),
            client_secret: Some("s".into()),
            scopes: vec![],
            callback_port: 8080,
        };
        assert!(!cfg.is_public_client());
        assert_eq!(cfg.redirect_uri(), "http://127.0.0.1:8080/callback");
    }

    #[test]
    fn pkce_challenge_shape() {
        let pkce = PkceChallenge::from_verifier("my-verifier-123", fake_s256);
        assert_eq!(pkce.method, "S256");
        assert_eq!(pkce.challenge, fake_s256("my-verifier-123"));
        assert_ne!(pkce.challenge, pkce.verifier, "challenge 不能等于 verifier");
        let params = pkce.auth_params();
        assert_eq!(params["code_challenge_method"], "S256");
    }

    #[test]
    fn token_expiry_with_margin() {
        let token = OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: Some(1000),
            scope: None,
        };
        assert!(!token.is_expired_at(900), "还有 100 秒,不算过期");
        assert!(token.is_expired_at(950), "留 60 秒余量 → 950 已算过期");
        assert!(token.is_expired_at(1000));
    }

    #[test]
    fn token_without_expiry_never_expires() {
        let token = OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scope: None,
        };
        assert!(!token.is_expired_at(u64::MAX / 2));
    }

    #[test]
    fn token_refresh_capability() {
        let with_refresh = OAuthToken {
            access_token: "a".into(),
            refresh_token: Some("r".into()),
            expires_at: None,
            scope: None,
        };
        assert!(with_refresh.can_refresh());
        let without = OAuthToken {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: None,
            scope: None,
        };
        assert!(!without.can_refresh());
    }

    #[test]
    fn client_registration_public() {
        let req = ClientRegistrationRequest::public("zhuye", "http://127.0.0.1:0/callback");
        assert_eq!(req.token_endpoint_auth_method, "none");
        assert_eq!(req.redirect_uris.len(), 1);
    }

    #[test]
    fn elicitation_required_fields() {
        let req = ElicitationRequest {
            request_id: "e1".into(),
            server: "api-server".into(),
            message: "请提供凭据".into(),
            fields: vec![
                ElicitationField {
                    name: "endpoint".into(),
                    description: "API 端点".into(),
                    secret: false,
                    required: true,
                },
                ElicitationField {
                    name: "token".into(),
                    description: "访问令牌".into(),
                    secret: true,
                    required: true,
                },
                ElicitationField {
                    name: "note".into(),
                    description: "备注".into(),
                    secret: false,
                    required: false,
                },
            ],
        };
        assert_eq!(req.required_fields().len(), 2);
        assert!(req.has_secret_fields());
    }

    #[test]
    fn elicitation_validate_answers_ok() {
        let req = ElicitationRequest {
            request_id: "e".into(),
            server: "s".into(),
            message: "m".into(),
            fields: vec![ElicitationField {
                name: "x".into(),
                description: "d".into(),
                secret: false,
                required: true,
            }],
        };
        let mut answers = BTreeMap::new();
        answers.insert("x".to_string(), "value".to_string());
        assert!(req.validate_answers(&answers).is_ok());
    }

    #[test]
    fn elicitation_validate_rejects_missing_required() {
        let req = ElicitationRequest {
            request_id: "e".into(),
            server: "s".into(),
            message: "m".into(),
            fields: vec![ElicitationField {
                name: "x".into(),
                description: "d".into(),
                secret: false,
                required: true,
            }],
        };
        let err = req.validate_answers(&BTreeMap::new()).unwrap_err();
        assert_eq!(err, ElicitationError::MissingRequired("x".into()));
    }

    #[test]
    fn elicitation_validate_rejects_blank_required() {
        let req = ElicitationRequest {
            request_id: "e".into(),
            server: "s".into(),
            message: "m".into(),
            fields: vec![ElicitationField {
                name: "x".into(),
                description: "d".into(),
                secret: false,
                required: true,
            }],
        };
        let mut answers = BTreeMap::new();
        answers.insert("x".to_string(), "   ".to_string());
        assert!(req.validate_answers(&answers).is_err(), "空白不算填了");
    }

    #[test]
    fn elicitation_validate_rejects_unknown_field() {
        let req = ElicitationRequest {
            request_id: "e".into(),
            server: "s".into(),
            message: "m".into(),
            fields: vec![],
        };
        let mut answers = BTreeMap::new();
        answers.insert("ghost".to_string(), "v".to_string());
        assert_eq!(
            req.validate_answers(&answers).unwrap_err(),
            ElicitationError::UnknownField("ghost".into())
        );
    }

    #[test]
    fn sampling_summary_and_suspicion() {
        let normal = SamplingRequest {
            request_id: "s1".into(),
            server: "helper".into(),
            prompt_preview: "summarize".into(),
            max_tokens: 1000,
            purpose: Some("summarize a file".into()),
        };
        assert!(!normal.is_suspicious());
        assert!(normal.approval_summary().contains("helper"));

        let huge = SamplingRequest {
            max_tokens: 100_000,
            ..normal.clone()
        };
        assert!(huge.is_suspicious(), "超大 token 请求应标红");

        let no_purpose = SamplingRequest {
            purpose: None,
            ..normal
        };
        assert!(no_purpose.is_suspicious(), "未声明用途应标红");
    }

    #[test]
    fn sampling_decision_labels() {
        assert_eq!(SamplingDecision::Allow.as_str(), "allow");
        assert_eq!(SamplingDecision::Deny.as_str(), "deny");
    }

    #[test]
    fn oauth_metadata_serializes() {
        let m = OAuthMetadata {
            authorization_endpoint: "https://x/auth".into(),
            token_endpoint: "https://x/token".into(),
            registration_endpoint: Some("https://x/register".into()),
            scopes_supported: vec!["read".into()],
            require_pkce: true,
        };
        let json = serde_json::to_string(&m).expect("serialize");
        let back: OAuthMetadata = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, m);
    }

    #[test]
    fn resource_subscription() {
        let sub = ResourceSubscription {
            uri: "file:///x".into(),
            active: true,
            last_notified_at: Some(100),
        };
        let json = serde_json::to_string(&sub).expect("serialize");
        let back: ResourceSubscription = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, sub);
    }
}

// ── 客户端侧运行时:发现 + 换 token ─────────────────────────────────────

use crate::McpError;

/// MCP 客户端关心的 OAuth 端点(受保护资源元数据 + 授权服务器元数据)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OAuthEndpoints {
    /// 受保护资源元数据 URL(RFC 9728)。
    pub resource_metadata_url: Option<String>,
    /// 授权端点(授权码流程由宿主完成)。
    pub authorization_url: Option<String>,
    /// token 端点。
    pub token_url: Option<String>,
    /// 动态客户端注册端点。
    pub registration_url: Option<String>,
    /// 授权服务器 issuer。
    pub issuer: Option<String>,
    /// 服务端声明支持的 scope。
    pub scopes_supported: Vec<String>,
}

impl OAuthEndpoints {
    /// 面向宿主的可读摘要(出现在失败事件里,宿主据此完成授权)。
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(url) = &self.authorization_url {
            parts.push(format!("授权端点={url}"));
        }
        if let Some(url) = &self.token_url {
            parts.push(format!("token 端点={url}"));
        }
        if let Some(url) = &self.resource_metadata_url {
            parts.push(format!("受保护资源元数据={url}"));
        }
        if !self.scopes_supported.is_empty() {
            parts.push(format!("scope={}", self.scopes_supported.join(",")));
        }
        if parts.is_empty() {
            "服务端未提供可发现的端点".to_string()
        } else {
            parts.join("；")
        }
    }
}

/// 从 `WWW-Authenticate: Bearer resource_metadata="..."` 里取元数据 URL。
pub fn parse_resource_metadata_url(header: &str) -> Option<String> {
    let lowered = header.to_ascii_lowercase();
    if !lowered.contains("bearer") {
        return None;
    }
    let marker = "resource_metadata=";
    let start = lowered.find(marker)? + marker.len();
    let rest = &header[start..];
    let value = rest.trim_start();
    let value = value.strip_prefix('"').unwrap_or(value);
    let end = value.find('"').unwrap_or(value.len());
    let url = value[..end].trim();
    (!url.is_empty()).then(|| url.to_string())
}

/// 受保护资源元数据(RFC 9728)。
#[derive(Debug, Clone, Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

/// 授权服务器元数据(RFC 8414)。
#[derive(Debug, Clone, Deserialize)]
struct AuthorizationServerMetadata {
    #[serde(default)]
    issuer: Option<String>,
    #[serde(default)]
    authorization_endpoint: Option<String>,
    #[serde(default)]
    token_endpoint: Option<String>,
    #[serde(default)]
    registration_endpoint: Option<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

/// 取受保护资源元数据(默认 well-known 路径;也接受显式 URL)。
pub async fn discover_protected_resource(
    http: &reqwest::Client,
    resource_url: &str,
    explicit_metadata_url: Option<&str>,
) -> Result<OAuthEndpoints, McpError> {
    let metadata_url = match explicit_metadata_url {
        Some(url) => url.to_string(),
        None => {
            let base = resource_url.trim_end_matches('/');
            // RFC 9728:路径插入式 —— <origin>/.well-known/oauth-protected-resource<path>
            let (origin, path) = split_origin_path(base);
            format!("{origin}/.well-known/oauth-protected-resource{path}")
        }
    };
    let response = http.get(&metadata_url).send().await.map_err(|error| {
        McpError::Initialize(format!("取受保护资源元数据失败({metadata_url}): {error}"))
    })?;
    if !response.status().is_success() {
        return Err(McpError::Initialize(format!(
            "受保护资源元数据不可用({metadata_url}): HTTP {}",
            response.status()
        )));
    }
    let metadata: ProtectedResourceMetadata = response
        .json()
        .await
        .map_err(|error| McpError::Initialize(format!("受保护资源元数据解析失败: {error}")))?;
    let mut endpoints = OAuthEndpoints {
        resource_metadata_url: Some(metadata_url),
        scopes_supported: metadata.scopes_supported,
        ..OAuthEndpoints::default()
    };
    // 授权服务器元数据:优先用 authorization_servers[0]。
    if let Some(issuer) = metadata.authorization_servers.first() {
        let discovered = discover_authorization_server(http, issuer).await?;
        endpoints.authorization_url = discovered.authorization_url;
        endpoints.token_url = discovered.token_url;
        endpoints.registration_url = discovered.registration_url;
        endpoints.issuer = discovered.issuer.or_else(|| Some(issuer.clone()));
        if endpoints.scopes_supported.is_empty() {
            endpoints.scopes_supported = discovered.scopes_supported;
        }
    }
    Ok(endpoints)
}

/// 取授权服务器元数据(RFC 8414)。
pub async fn discover_authorization_server(
    http: &reqwest::Client,
    issuer: &str,
) -> Result<OAuthEndpoints, McpError> {
    let base = issuer.trim_end_matches('/');
    let (origin, path) = split_origin_path(base);
    let candidates = [
        format!("{origin}/.well-known/oauth-authorization-server{path}"),
        format!("{origin}/.well-known/openid-configuration{path}"),
        format!("{base}/.well-known/oauth-authorization-server"),
    ];
    let mut last_error = String::new();
    for candidate in candidates {
        let response = match http.get(&candidate).send().await {
            Ok(response) => response,
            Err(error) => {
                last_error = error.to_string();
                continue;
            }
        };
        if !response.status().is_success() {
            last_error = format!("HTTP {}", response.status());
            continue;
        }
        let metadata: AuthorizationServerMetadata = match response.json().await {
            Ok(metadata) => metadata,
            Err(error) => {
                last_error = error.to_string();
                continue;
            }
        };
        return Ok(OAuthEndpoints {
            resource_metadata_url: None,
            authorization_url: metadata.authorization_endpoint,
            token_url: metadata.token_endpoint,
            registration_url: metadata.registration_endpoint,
            issuer: metadata.issuer.or_else(|| Some(base.to_string())),
            scopes_supported: metadata.scopes_supported,
        });
    }
    Err(McpError::Initialize(format!(
        "授权服务器元数据不可用({base}): {last_error}"
    )))
}

/// 极简 urlencoded 转义(键值都是 ASCII 配置值)。
fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// 拆出 origin 与 path(`https://h/p` → (`https://h`, `/p`))。
fn split_origin_path(url: &str) -> (String, String) {
    match url.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('/') {
            Some((host, path)) => (format!("{scheme}://{host}"), format!("/{path}")),
            None => (format!("{scheme}://{rest}"), String::new()),
        },
        None => (url.to_string(), String::new()),
    }
}

/// token 端点的请求体参数。
#[derive(Debug, Clone)]
pub enum TokenGrant {
    /// `client_credentials`。
    ClientCredentials {
        client_id: String,
        client_secret: Option<String>,
        scope: Option<String>,
    },
    /// `refresh_token`。
    RefreshToken {
        client_id: Option<String>,
        refresh_token: String,
        scope: Option<String>,
    },
}

/// 用授权码换 token(client_credentials / refresh_token)。
pub async fn request_token(
    http: &reqwest::Client,
    token_url: &str,
    grant: &TokenGrant,
) -> Result<OAuthToken, McpError> {
    let mut form: Vec<(&str, String)> = Vec::new();
    match grant {
        TokenGrant::ClientCredentials {
            client_id,
            client_secret,
            scope,
        } => {
            form.push(("grant_type", "client_credentials".to_string()));
            form.push(("client_id", client_id.clone()));
            if let Some(secret) = client_secret {
                form.push(("client_secret", secret.clone()));
            }
            if let Some(scope) = scope {
                form.push(("scope", scope.clone()));
            }
        }
        TokenGrant::RefreshToken {
            client_id,
            refresh_token,
            scope,
        } => {
            form.push(("grant_type", "refresh_token".to_string()));
            form.push(("refresh_token", refresh_token.clone()));
            if let Some(client_id) = client_id {
                form.push(("client_id", client_id.clone()));
            }
            if let Some(scope) = scope {
                form.push(("scope", scope.clone()));
            }
        }
    }
    // 手写 urlencoded body:reqwest 的 `form()` 需要额外 feature,
    // 这里不引入 —— body 就是几个键值对,手写更可控。
    let body = form
        .iter()
        .map(|(key, value)| format!("{}={}", url_encode(key), url_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let response = http
        .post(token_url)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|error| McpError::Initialize(format!("token 请求失败({token_url}): {error}")))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(McpError::Initialize(format!(
            "token 请求被拒({token_url}): HTTP {status} {body}"
        )));
    }
    let token: OAuthToken = response
        .json()
        .await
        .map_err(|error| McpError::Initialize(format!("token 响应解析失败: {error}")))?;
    Ok(token)
}

/// 按配置解析出可用的 bearer token。
///
/// 顺序:静态 `token` → `token_env` 环境变量 → 客户端凭据换 token
/// (token 端点缺省时按受保护资源元数据发现)。
pub async fn resolve_token(
    http: &reqwest::Client,
    oauth: &crate::config::McpOAuthConfig,
    resource_url: &str,
) -> Result<Option<String>, McpError> {
    if let Some(token) = oauth
        .token
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(Some(token.trim().to_string()));
    }
    if let Some(env_name) = oauth.token_env.as_ref().filter(|name| !name.is_empty())
        && let Ok(value) = std::env::var(env_name)
        && !value.trim().is_empty()
    {
        return Ok(Some(value.trim().to_string()));
    }
    // 需要换 token:必须有 client_id(或 refresh_token)。
    let refresh_token = oauth
        .refresh_token
        .clone()
        .filter(|value| !value.is_empty());
    let client_id = oauth.client_id.clone().filter(|value| !value.is_empty());
    if client_id.is_none() && refresh_token.is_none() {
        return Ok(None);
    }
    let token_url = match oauth.token_url.clone().filter(|value| !value.is_empty()) {
        Some(url) => url,
        None => {
            let endpoints = discover_protected_resource(http, resource_url, None).await?;
            let summary = endpoints.summary();
            endpoints.token_url.ok_or_else(|| {
                McpError::AuthRequired(format!("MCP 服务端要求授权,但未发现 token 端点({summary})"))
            })?
        }
    };
    let grant = if let Some(refresh) = refresh_token {
        TokenGrant::RefreshToken {
            client_id,
            refresh_token: refresh,
            scope: oauth.scope.clone(),
        }
    } else {
        TokenGrant::ClientCredentials {
            client_id: client_id.unwrap_or_default(),
            client_secret: oauth.client_secret.clone(),
            scope: oauth.scope.clone(),
        }
    };
    let token = request_token(http, &token_url, &grant).await?;
    Ok(Some(token.access_token))
}

#[cfg(test)]
mod runtime_tests {
    use super::*;

    #[test]
    fn parses_resource_metadata_from_www_authenticate() {
        let header = r#"Bearer realm="mcp", resource_metadata="https://h/.well-known/oauth-protected-resource/mcp""#;
        assert_eq!(
            parse_resource_metadata_url(header).as_deref(),
            Some("https://h/.well-known/oauth-protected-resource/mcp")
        );
        assert!(parse_resource_metadata_url("Basic realm=\"x\"").is_none());
    }

    #[test]
    fn splits_origin_and_path() {
        assert_eq!(
            split_origin_path("https://h/mcp"),
            ("https://h".to_string(), "/mcp".to_string())
        );
        assert_eq!(
            split_origin_path("https://h"),
            ("https://h".to_string(), String::new())
        );
    }
}
