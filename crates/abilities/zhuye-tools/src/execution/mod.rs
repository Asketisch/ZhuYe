//! 执行形态 —— 批量调用、进程组、跨进程工具与文档读取。
//!
//! - [`code_mode`]：Code Mode 批量工具调用计划（F21，受限计划而非任意代码）。
//! - [`child_process`]：Unix 进程组隔离与后代清理。
//! - [`remote`]：serve 模式下客户端注册的跨语言自定义工具。
//! - [`office`]：Word / Excel / PPT / PDF 的读取与提取（F37）。

pub mod child_process;
pub mod code_mode;
pub mod office;
pub mod remote;
