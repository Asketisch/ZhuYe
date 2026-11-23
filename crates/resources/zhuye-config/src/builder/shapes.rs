//! MCP / LSP server 的编译期就绪配置形状。
//!
//! 之所以在 `zhuye-config` 层做校验:
//! 1. 校验可以独立单测,无需启动 rmcp 子进程;
//! 2. `zhuye-exec::handle_reload` 在 reload 时也需要复用同一套校验逻辑。

use std::collections::HashMap;
use std::time::Duration;

use crate::error::ConfigError;
use crate::schema::{LspFilePattern, LspServerEntry, McpServerEntry, McpTransport};

/// MCP tool call 默认超时 30s,与 `mcp.connection.timeoutMs` (Anthropic) 一致。
pub(crate) const DEFAULT_MCP_TIMEOUT_MS: u64 = 30_000;
/// LSP request 默认超时 30s,与 MCP 对齐。
const DEFAULT_LSP_TIMEOUT_MS: u64 = 30_000;

/// 编译期就绪的 MCP server 配置(`zhuye_mcp::McpServerConfig`)——
/// `zhuye-config` 不依赖 `zhuye-mcp`(避免循环),这里只产出
/// `McpServerConfigShape`,由 `zhuye-mcp` 自己的 `From` impl 收尾。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfigShape {
    pub name: String,
    pub transport: McpTransport,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub url: Option<String>,
    pub headers: HashMap<String, String>,
    pub timeout: Duration,
    /// http 传输的 OAuth 配置(客户端侧)。
    pub oauth: Option<McpOAuthConfigShape>,
}

/// `[mcp_servers.<name>.oauth]` 的编译期就绪形态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpOAuthConfigShape {
    pub token: Option<String>,
    pub token_env: Option<String>,
    pub authorization_url: Option<String>,
    pub token_url: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scope: Option<String>,
    pub grant_type: Option<String>,
    pub refresh_token: Option<String>,
}

impl McpServerConfigShape {
    pub(crate) fn from_entry(name: String, entry: &McpServerEntry) -> Result<Self, ConfigError> {
        match entry.transport {
            McpTransport::Stdio => {
                let cmd = entry.command.as_deref().ok_or_else(|| {
                    ConfigError::Build(format!(
                        "[mcp_servers.{name}] type=stdio requires 'command'"
                    ))
                })?;
                Ok(Self {
                    name,
                    transport: entry.transport,
                    command: Some(cmd.to_string()),
                    args: entry.args.clone().unwrap_or_default(),
                    env: entry.env.clone().unwrap_or_default(),
                    url: None,
                    headers: HashMap::new(),
                    timeout: Duration::from_millis(
                        entry.timeout_ms.unwrap_or(DEFAULT_MCP_TIMEOUT_MS),
                    ),
                    // stdio 传输不走 HTTP,OAuth 不适用。
                    oauth: None,
                })
            }
            McpTransport::Http | McpTransport::Sse => {
                let ty = match entry.transport {
                    McpTransport::Sse => "sse",
                    _ => "http",
                };
                let url = entry.url.as_deref().ok_or_else(|| {
                    ConfigError::Build(format!("[mcp_servers.{name}] type={ty} requires 'url'"))
                })?;
                Ok(Self {
                    name,
                    transport: entry.transport,
                    command: None,
                    args: Vec::new(),
                    env: HashMap::new(),
                    url: Some(url.to_string()),
                    headers: entry.headers.clone().unwrap_or_default(),
                    timeout: Duration::from_millis(
                        entry.timeout_ms.unwrap_or(DEFAULT_MCP_TIMEOUT_MS),
                    ),
                    oauth: entry.oauth.as_ref().map(|oauth| McpOAuthConfigShape {
                        token: oauth.token.clone(),
                        token_env: oauth.token_env.clone(),
                        authorization_url: oauth.authorization_url.clone(),
                        token_url: oauth.token_url.clone(),
                        client_id: oauth.client_id.clone(),
                        client_secret: oauth.client_secret.clone(),
                        scope: oauth.scope.clone(),
                        grant_type: oauth.grant_type.clone(),
                        refresh_token: oauth.refresh_token.clone(),
                    }),
                })
            }
        }
    }
}

/// 编译期就绪的 LSP server 配置(`zhuye_lsp::LspServerConfig` 的薄包装)。
///
/// `zhuye-config` 不依赖 `zhuye-lsp`,这里只产出
/// `LspServerConfigShape`,由 `zhuye-lsp` 的 `TryFrom` impl 收尾
/// (预编译 glob 放 zhuye-lsp,因为 `globset` 依赖不传到 config 层)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspServerConfigShape {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub patterns: Vec<LspFilePattern>,
    pub root_uri: Option<String>,
    pub initialization_options: Option<serde_json::Value>,
    pub timeout: Duration,
}

impl LspServerConfigShape {
    pub(crate) fn from_entry(name: String, entry: &LspServerEntry) -> Result<Self, ConfigError> {
        if entry.file_patterns.is_empty() {
            return Err(ConfigError::Build(format!(
                "[lsp_servers.{name}] requires at least one file_patterns entry"
            )));
        }
        if entry.command.is_empty() {
            return Err(ConfigError::Build(format!(
                "[lsp_servers.{name}] requires 'command'"
            )));
        }
        Ok(Self {
            name,
            command: entry.command.clone(),
            args: entry.args.clone(),
            env: entry.env.clone(),
            patterns: entry.file_patterns.clone(),
            root_uri: entry.root_uri.clone(),
            initialization_options: entry.initialization_options.clone(),
            timeout: Duration::from_millis(entry.timeout_ms.unwrap_or(DEFAULT_LSP_TIMEOUT_MS)),
        })
    }
}
