//! `zhuye-git` — git 能力族:checkpoint 快照 / 分支与 PR 探测 /
//! assistant 指令解析 / 有界命令执行抽象。
//!
//! 收编两份客户端各自演化的 git 实现:
//!
//! - **checkpoint**:框架原有 `zhuye-tools/src/checkpoint.rs`(原语)与
//!   Reflect-TUI `git_checkpoint.rs`(tag 枚举 + 安全快照 + sha 校验)
//!   是**双实现**。合并后以 TUI 版为基(它修掉了框架版的洞:`rewind`
//!   先 `reset --hard` 再列,安全快照提交脱离分支历史后**列不出也回退
//!   不回去**),框架版的 rollout 记录 / conversation 截断留在 exec 工具层。
//! - **分支 / PR 探测**:TUI `branch_summary.rs`(499 行)只与
//!   `WorkspaceCommandExecutor` trait 对话,不绑渲染框架 —— 连抽象一起搬。
//! - **指令解析**:TUI `git_action_directives.rs`(纯 std 解析)。
//!
//! ## 与 `zhuye-tools` 的分工
//!
//! 本 crate 是**能力层**(纯 git 语义,无 Tool trait);`zhuye-tools` 的
//! checkpoint 原语与 `zhuye-exec` 的 CheckpointTool / RewindTool 是
//! **工具层**(权限 / 审批 / rollout 记录),下沉后改为调用本 crate。

pub mod branch;
pub mod checkpoint;
pub mod directives;
pub mod runner;

pub use branch::{
    GitBranchDiffStats, StatusLineGitSummary, StatusLinePullRequest, current_branch_name,
    status_line_git_summary,
};
pub use checkpoint::{
    CHECKPOINT_SUBJECT_PREFIX, CHECKPOINT_TAG_PREFIX, Checkpoint, GitError, create, is_repo, list,
    rewind,
};
pub use directives::{GitActionDirective, ParsedAssistantMarkdown, parse_assistant_markdown};
pub use runner::{CommandOutput, CommandRunner, WorkspaceCommand, WorkspaceCommandError};
