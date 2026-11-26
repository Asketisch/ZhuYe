//! git 能力族 —— 快照、回退与 worktree 会话状态。
//!
//! - [`snapshot`]：git 快照与回退原语（实现已下沉至 `zhuye-git`）。
//! - [`checkpoint_tool`]：`CheckpointTool` / `RewindTool` 两个工具。
//! - [`worktree`]：worktree 会话状态与辅助函数。

pub mod checkpoint_tool;
pub mod snapshot;
pub mod worktree;
