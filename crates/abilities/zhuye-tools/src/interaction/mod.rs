//! 挂起交互 —— 等待人工输入、审批门与在飞异步子代理登记表。
//!
//! - [`human_input`]：`request_human_input` 的持久化存储。
//! - [`plan_approval`]：Plan mode 审批路由（镜像 `approval::ApprovalGate`）。
//! - [`pending_subagent`]：在飞异步子代理委派登记表。

pub mod human_input;
pub mod pending_subagent;
pub mod plan_approval;
