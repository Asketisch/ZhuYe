//! `zhuye-mcp` — MCP (Model Context Protocol) client integration (v0.3)。
//!
//! 每个 MCP server 暴露 N 个 tool,通过 `Tool` trait 包装后注册到
//! `ToolRegistry::Runtime`。LLM 层 (`zhuye-llm`) 无需改动:OpenAI /
//! Anthropic 的 tool schema 转换直接吃 JSON Schema 2020-12,与 MCP
//! `inputSchema` 同型。
//!
//! ## 范围 (v0.3)
//!
//! - transport: stdio + streamable-http
//! - 原语: Tools
//! - 协议版本: `2025-06-18`
//!
//! ## 能力清单(v1.6 更正)
//!
//! - 已实现:Tools、Resources(`ListMcpResources` / `ReadMcpResource`,
//!   v1.2 起真接线 —— 此前注释误标为未实现)、Prompts
//!   (`ListMcpPrompts` / `GetMcpPrompt`,v1.6 接线)、Elicitation
//!   (server→client `elicitation/create` 转成 `McpLifecycleEvent::Elicitation`,
//!   由宿主经 `Op::McpElicitationResponse` 裁决;**sampling 能力不声明**,
//!   避免宣称不存在的模型调用通道)。
//! - 客户端侧 OAuth(http 传输)已接线:静态 token / `token_env` /
//!   `client_credentials` / `refresh_token`;token 端点缺省时按受保护资源
//!   元数据(RFC 9728)与授权服务器元数据(RFC 8414)发现。授权码流程需要
//!   浏览器,由宿主完成 —— 引擎只发现端点并在上报里给出可执行提示。
//! - 未实现(留后续):Sampling;授权码流程的浏览器交互(宿主侧)。
//!
//! ## 关键约束
//!
//! - Tool 名强制 `mcp__<server>__<tool>` 前缀,与 7 个 Zhuye
//!   内置 tool 不冲突。
//! - 配置走 `[mcp_servers.<name>]` TOML 表风格(对齐 `.mcp.json`)。
//! - env 注入用 `env_clear()` 隔离,只传 `HOME`/`PATH` + 用户显式 env,
//!   避免泄漏 `OPENAI_API_KEY` 等密钥到 MCP 子进程。

#![allow(clippy::module_name_repetitions)] // 多个模块都暴露 *Config/*Error 等命名

pub mod catalog;
/// MCP 客户端侧（连接生命周期 / transport / tool 适配器）。
pub mod client;
pub mod config;
/// MCP 协议基础形状（内容块转换 / 协议版本）。
pub mod protocol;
/// MCP 协议完整性扩展（F29 OAuth / elicitation / sampling，F30 自暴露 server）。
pub mod protocol_ext;
/// 暴露为 agent 工具的 MCP 原语（prompts / resources）。
pub mod tools;

// 公开 API re-export
pub use catalog::registry::{McpRegistryEntry, curated_catalog, find_entry, install_hint};
pub use client::manager::{
    McpConnectionManager, McpLifecycleEvent, McpServerHandle, McpServerStatus, list_plugin_servers,
    scoped_plugin_name,
};
pub use client::tool::{McpToolAdapter, McpToolDescriptor};
pub use config::{McpOAuthConfig, McpServerConfig, McpTransport};
pub use protocol::content::convert_mcp_content;
pub use protocol::version::PROTOCOL_VERSION;
pub use tools::prompts::{GetMcpPromptTool, ListMcpPromptsTool};
pub use tools::resources::{ListMcpResourcesTool, ReadMcpResourceTool};

/// transport 模块的旧路径别名 —— 保持 `zhuye_mcp::transport::build_stdio`
/// 等既有调用点不变（实现已下沉到 [`client::transport`]）。
pub use client::transport;

/// MCP 集成错误类型。
///
/// 所有 `Tool::execute` 失败路径都映射回 `zhuye_protocol::ToolError`,
/// 此处 `McpError` 仅用于 `McpConnectionManager::start_server` 与
/// `reload` 等生命周期操作。
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// 配置校验失败(server 缺 command/url 等)。
    #[error("mcp config invalid: {0}")]
    ConfigInvalid(String),

    /// 启动 stdio 子进程失败。
    #[error("mcp spawn failed: {0}")]
    Spawn(#[from] std::io::Error),

    /// 握手 / initialize 失败。
    #[error("mcp initialize failed: {0}")]
    Initialize(String),

    /// 单次 tool call 失败。
    #[error("mcp call failed: {0}")]
    Call(String),

    /// 单次 tool call 超过 `timeout_ms`。
    #[error("mcp call timeout after {elapsed_ms}ms")]
    Timeout { elapsed_ms: u64 },

    /// 用户 (ctrl-c) 或 manager shutdown 触发 cancel。
    #[error("mcp call cancelled")]
    Cancelled,

    /// 优雅 shutdown 超时(>10s),需要 SIGKILL。
    #[error("mcp shutdown timeout")]
    ShutdownTimeout,

    /// 服务端要求 OAuth 授权,而当前没有可用凭据。
    ///
    /// 消息里带可执行的端点(授权 / token / 受保护资源元数据),宿主据此
    /// 完成授权码流程或换 token 后重试 —— 引擎不代跑浏览器。
    #[error("mcp auth required: {0}")]
    AuthRequired(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_exposed() {
        assert_eq!(PROTOCOL_VERSION, "2025-06-18");
    }

    #[test]
    fn error_display_includes_kind() {
        let e = McpError::Timeout { elapsed_ms: 250 };
        assert!(e.to_string().contains("250"));
    }
}
