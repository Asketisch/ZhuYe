//! MCP 协议完整性扩展 —— OAuth、elicitation、sampling 与自暴露 server。
//!
//! - [`oauth`]：OAuth / device flow / elicitation / sampling / 资源订阅
//!   协议层（F29）。
//! - [`server_mode`]：自暴露 MCP server 的工具清单、安全边界与 server
//!   card（F30）。

pub mod oauth;
pub mod server_mode;
