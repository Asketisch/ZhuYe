//! 记忆生命周期 —— 抽取、整理与自进化。
//!
//! - [`extract`]：会话结束自动提炼事实进长期记忆，并管理去重、冲突
//!   消解、分层、衰减与驱逐（F17）。
//! - [`consolidate`]：确定性的记忆整理（去重 / 冲突消解），零 LLM 成本。
//! - [`evolution`]：技能自进化与经验沉淀（F18）。

pub mod consolidate;
pub mod evolution;
pub mod extract;
