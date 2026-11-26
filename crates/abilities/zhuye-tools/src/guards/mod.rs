//! 守卫与预算 —— 限制、拦截与修正工具行为。
//!
//! - [`arg_repair`]：按 schema 无损修正模型给错的参数形状（F22）。
//! - [`loop_guard`]：循环失控守卫，区分「重复调用」与「无进展」双档处置。
//! - [`result_budget`]：超长输出的落盘与信封替换（三层预算）。
//! - [`web_guard`]：搜索后端 failover 与 SSRF 加固（F38）。

pub mod arg_repair;
pub mod loop_guard;
pub mod result_budget;
pub mod web_guard;
