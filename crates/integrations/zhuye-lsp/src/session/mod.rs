//! 单个 LSP server 的会话状态 —— 连接、文档同步与路由。
//!
//! - [`client`]：单个 server 的运行时内部状态。
//! - [`document`]：文档状态同步（didOpen / didChange）。
//! - [`matching`]：文件路径 → LSP server 路由。

pub mod client;
pub mod document;
pub mod matching;
