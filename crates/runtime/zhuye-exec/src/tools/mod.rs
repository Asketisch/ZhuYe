//! exec 层内置工具 —— 把引擎外的调度器包装成 agent 可调用的工具。
//!
//! 四件实现同构：均为 `impl Tool for XxxTool`，均把一个引擎外的运行时
//! 组件暴露为 agent 工具。
//!
//! - [`cron`]：把 cron 调度暴露为工具（v1.2 P1-2）。
//! - [`goal`]：把 goal 目标模式暴露为工具（v1.10）。
//! - [`memory`]：记忆读写工具（v1.x）。
//! - [`queue`]：把队列模式暴露为工具（v1.10）。

pub(crate) mod cron;
pub(crate) mod goal;
pub(crate) mod memory;
pub(crate) mod queue;
