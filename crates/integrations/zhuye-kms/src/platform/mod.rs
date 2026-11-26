//! 平台相关 I/O 后端 —— 按目标平台分派目录句柄实现。
//!
//! KMS 的目录遍历与文件读写依赖平台原语：Unix 用 `rustix` 的 `openat` 系
//! 列，Windows 用不允许删除共享的目录句柄固定路径。两套实现的语义差异
//! （符号链接跟随、权限保留、重解析点处理）由本层吸收，上层
//! [`KnowledgeManager`](crate::KnowledgeManager) 只面对同一套语义。

/// Unix 后端：目录句柄 + `openat` 相对操作。
#[cfg(unix)]
pub(crate) mod unix;

/// Windows 后端：固定目录句柄 + 重解析点校验。
#[cfg(windows)]
pub(crate) mod windows;
