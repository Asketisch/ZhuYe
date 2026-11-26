//! MCP 客户端侧 —— 连到外部 MCP server 并适配其工具。
//!
//! - [`manager`]：连接生命周期管理（start_server / reload / shutdown）。
//! - [`transport`]：stdio、streamable-http 与 legacy SSE transport 构造。
//! - [`tool`]：把单个 MCP tool 包成 `zhuye_tools::Tool` 的适配器。

pub mod manager;
pub mod tool;
pub mod transport;
