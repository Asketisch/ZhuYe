//! MCP server 强类型配置。
//!
//! 在 `zhuye_config::McpServerEntry` 之上加一层 "rmcp-ready" 表示,
//! `McpConnectionManager::start_server` 直接消费本层类型;stdio / http
//! 的实际建立见 `transport.rs`。

use std::collections::HashMap;
use std::time::Duration;

use zhuye_config::McpServerConfigShape;

/// MCP transport 类型,对应 TOML `type = "stdio" | "http"`。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum McpTransport {
    #[default]
    Stdio,
    /// Streamable HTTP,别名 `streamable-http`。
    Http,
    /// Legacy MCP HTTP+SSE(仅作兼容别名):当前实现直接委托
    /// streamable-http 客户端,旧式 GET-endpoint 握手未实现,
    /// 见 `transport.rs::build_sse`。
    Sse,
}

impl From<zhuye_config::McpTransport> for McpTransport {
    fn from(t: zhuye_config::McpTransport) -> Self {
        match t {
            zhuye_config::McpTransport::Stdio => Self::Stdio,
            zhuye_config::McpTransport::Http => Self::Http,
            zhuye_config::McpTransport::Sse => Self::Sse,
        }
    }
}

/// 编译期就绪的 MCP server 配置(`zhuye_config::McpServerConfigShape` 的薄包装)。
///
/// `zhuye-config` 不直接出 `McpServerConfig` 是为了避免它在
/// `zhuye-mcp` 之下依赖过深;这里在 zhuye-mcp 侧做形态转换。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
    /// stdio 模式:子进程可执行文件路径。
    pub command: Option<String>,
    /// stdio 模式:子进程参数。
    pub args: Vec<String>,
    /// stdio 模式:除 `HOME`/`PATH` 之外注入到子进程的 env。
    pub env: HashMap<String, String>,
    /// http 模式:server URL。
    pub url: Option<String>,
    /// http 模式:自定义 HTTP header(`Authorization` 自动剥离到 `auth_header`)。
    pub headers: HashMap<String, String>,
    /// 握手、首次工具列表及单次 tool call 的超时,默认 30s。
    pub timeout: Duration,
    /// http 传输的 OAuth 配置(客户端侧);stdio 不适用。
    pub oauth: Option<McpOAuthConfig>,
}

/// MCP 客户端侧的 OAuth 配置。
///
/// 凭据优先级:`token` > `token_env` > 用 `client_id` / `client_secret`
/// 走 `client_credentials`(或 `refresh_token`)换 token。
/// 授权码流程需要浏览器,由宿主完成 —— 引擎只负责发现端点并上报。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct McpOAuthConfig {
    /// 静态 bearer token。
    pub token: Option<String>,
    /// 从该环境变量读取 bearer token(避免密钥落 config.toml)。
    pub token_env: Option<String>,
    /// 授权端点(仅用于上报;引擎不代跑浏览器)。
    pub authorization_url: Option<String>,
    /// token 端点;缺省时按受保护资源元数据发现。
    pub token_url: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scope: Option<String>,
    /// `client_credentials`(默认)/ `refresh_token`。
    pub grant_type: Option<String>,
    pub refresh_token: Option<String>,
}

impl McpServerConfig {
    /// 握手、首次工具列表及单次 tool call 的超时。
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

impl From<McpServerConfigShape> for McpServerConfig {
    fn from(s: McpServerConfigShape) -> Self {
        Self {
            name: s.name,
            transport: McpTransport::from(s.transport),
            command: s.command,
            args: s.args,
            env: s.env,
            url: s.url,
            headers: s.headers,
            timeout: s.timeout,
            oauth: s.oauth.map(|oauth| McpOAuthConfig {
                token: oauth.token,
                token_env: oauth.token_env,
                authorization_url: oauth.authorization_url,
                token_url: oauth.token_url,
                client_id: oauth.client_id,
                client_secret: oauth.client_secret,
                scope: oauth.scope,
                grant_type: oauth.grant_type,
                refresh_token: oauth.refresh_token,
            }),
        }
    }
}
