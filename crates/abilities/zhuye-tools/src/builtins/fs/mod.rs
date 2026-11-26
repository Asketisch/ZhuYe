//! 文件与目录读写工具。
//!
//! - `read` / `write` / `edit` / `delete`：单文件读改写删。
//! - `glob` / `grep`：按模式与内容检索（grep 走 BM25 排序）。
//! - `notebook_edit`：notebook 单元编辑（门控型插件）。
//! - `image_view`：图片查看。

pub mod delete;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod image_view;
pub mod notebook_edit;
pub mod read;
pub mod write;
