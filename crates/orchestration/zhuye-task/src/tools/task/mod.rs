//! TaskManager 的八个内置工具 —— 创建、查询、更新、列表、终止、
//! 读输出、认领、放回。
//!
//! 全部同构：`pub struct FooTool { manager: Arc<TaskManager> }`，
//! stateful 模式，依赖同一 manager。

pub mod task_claim;
pub mod task_create;
pub mod task_get;
pub mod task_list;
pub mod task_output;
pub mod task_release;
pub mod task_stop;
pub mod task_update;
