//! 引擎自省与调度基建工具。
//!
//! - `tool_search`：按关键词检索可用工具。
//! - `deferred_call`：把工具调用延迟到下一轮。
//! - `context_remaining`：剩余上下文预算查询。
//! - `background_status`：后台任务状态查询。

pub mod background_status;
pub mod context_remaining;
pub mod deferred_call;
pub mod tool_search;
