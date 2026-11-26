//! 命令执行与工作区切换工具。
//!
//! - `bash`：执行 shell 命令。
//! - `echo`：回显文本。
//! - `enter_worktree` / `exit_worktree`：切换 git worktree 工作区。

pub mod bash;
pub mod echo;
pub mod enter_worktree;
pub mod exit_worktree;
