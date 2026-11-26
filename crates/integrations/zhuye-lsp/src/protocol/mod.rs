//! LSP 协议形状 —— 请求方法、主动查询扩展与诊断缓存。
//!
//! - [`methods`]：`action` 枚举与各 action 的强类型 invoke 包装。
//! - [`active`]：主动查询扩展（F15）与 DAP 调试协议形状。
//! - [`diagnostics`]`publishDiagnostics` 通知的缓存注册表。

pub mod active;
pub mod diagnostics;
pub mod methods;
