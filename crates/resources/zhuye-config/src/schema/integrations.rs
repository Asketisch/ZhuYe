//! 集成配置 section（MCP / LSP / PostgreSQL / Redis / Bridge / Voice / DAP / ACP / web_search）。
//!
//! 包含:
//! - `McpServersSection` / `McpServerEntry` / `McpTransport`
//! - `LspServersSection` / `LspServerEntry` / `LspFilePattern`
//! - `PostgresSessionSection` / `SseRedisSection` / `BridgeSection`
//! - `VoiceSection` / `DapSection` / `AcpSection` / `WebSearchSection`

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── MCP (v0.3) ────────────────────────────────────────────────────────────

/// `[mcp_servers.<name>]` 表镜像。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct McpServersSection {
    #[serde(flatten)]
    pub servers: HashMap<String, McpServerEntry>,
}

/// 单个 MCP server 原始配置(未校验)。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct McpServerEntry {
    #[serde(rename = "type", default)]
    pub transport: McpTransport,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    pub url: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub always_load: Option<bool>,
    /// http 传输的 OAuth 配置(客户端侧)。
    #[serde(default)]
    pub oauth: Option<McpOAuthEntry>,
}

/// `[mcp_servers.<name>.oauth]` 表镜像 —— MCP 客户端侧的 OAuth 配置。
///
/// 凭据优先用 `token_env`(环境变量),避免把密钥写进 config.toml。
/// `grant_type`:
/// - `client_credentials`(默认):用 `client_id` / `client_secret` 换 token;
/// - `refresh_token`:用 `refresh_token` 换新 token。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct McpOAuthEntry {
    /// 静态 bearer token(优先级最高)。
    #[serde(default)]
    pub token: Option<String>,
    /// 从该环境变量读取 bearer token。
    #[serde(default)]
    pub token_env: Option<String>,
    /// 授权端点(授权码流程由宿主完成;客户端只上报,不代跑浏览器)。
    #[serde(default)]
    pub authorization_url: Option<String>,
    /// token 端点;缺省时按受保护资源元数据发现。
    #[serde(default)]
    pub token_url: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    /// `client_credentials`(默认)/ `refresh_token`。
    #[serde(default)]
    pub grant_type: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

/// MCP transport 枚举。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    #[default]
    Stdio,
    #[serde(alias = "streamable-http")]
    Http,
    #[serde(alias = "http-sse")]
    Sse,
}

// ── LSP (v0.5) ──────────────────────────────────────────────────────

/// `[lsp_servers.<name>]` 表镜像。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspServersSection {
    #[serde(flatten)]
    pub servers: HashMap<String, LspServerEntry>,
}

/// 单个 LSP server 原始配置(未校验)。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspServerEntry {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub file_patterns: Vec<LspFilePattern>,
    #[serde(default)]
    pub root_uri: Option<String>,
    #[serde(default)]
    pub initialization_options: Option<serde_json::Value>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// 单条文件 glob → languageId 映射。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LspFilePattern {
    pub glob: String,
    pub language_id: String,
}

// ── 其他集成 ────────────────────────────────────────────────────────────

/// P2: PostgreSQL 会话配置。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PostgresSessionSection {
    pub database_url: Option<String>,
    #[serde(default = "default_pg_prefix")]
    pub table_prefix: String,
}

fn default_pg_prefix() -> String {
    "zhuye_".into()
}

/// P2: SSE Redis stub 配置。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SseRedisSection {
    pub redis_url: Option<String>,
    #[serde(default = "default_sse_prefix")]
    pub key_prefix: String,
}

fn default_sse_prefix() -> String {
    "zhuye:sse:".into()
}

/// P3: Bridge 远程 stub。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BridgeSection {
    pub endpoint: Option<String>,
}

/// P3: 语音服务 stub。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct VoiceSection {
    #[serde(default)]
    pub enabled: Option<bool>,
    pub provider: Option<String>,
}

/// P2: DAP 调试器 stub。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DapSection {
    pub adapter: Option<String>,
}

/// ACP stub server 默认监听地址。
pub const ACP_DEFAULT_BIND: &str = "127.0.0.1:0";

/// P2: ACP stub server 配置。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AcpSection {
    #[serde(default = "default_acp_bind")]
    pub bind: String,
}

impl Default for AcpSection {
    fn default() -> Self {
        Self {
            bind: default_acp_bind(),
        }
    }
}

fn default_acp_bind() -> String {
    ACP_DEFAULT_BIND.into()
}

/// `[web_search]` 段。控制 Brave Search API key 等参数。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct WebSearchSection {
    #[serde(default)]
    pub api_key: Option<String>,
}
