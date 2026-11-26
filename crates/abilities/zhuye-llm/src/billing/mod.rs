//! 计费与用量记账 —— 账号池、配额窗口、成本账本与凭证冷却。
//!
//! 这五件模块共享同一条生命周期：一次 LLM 调用消耗 token、可能触发
//! 限流、最终计入成本账本。
//!
//! - [`accounts`]：OAuth 登录与多账号轮换池（F31）。
//! - [`credential`]：单个 API 凭证及其权重 / 冷却覆盖。
//! - [`quota`]：token plan 的本地配额窗口追踪。
//! - [`cooldown`]：credential 的冷却状态与指数退避。
//! - [`spend`]：USD 预算账本与成本归因（F44 / F55）。

pub mod accounts;
pub mod cooldown;
pub mod credential;
pub mod quota;
pub mod spend;
