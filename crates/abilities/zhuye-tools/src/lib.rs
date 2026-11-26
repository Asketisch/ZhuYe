//! `zhuye-tools` —— Tool trait、内建工具,以及驱动 M2+ agent 循环的
//! 按并发安全性切分的 `ToolExecutionQueue`。

pub mod approval;
// 0.3:bash 命令参数别名解析下沉到 zhuye-protocol(与 ToolError 同一
// 理由:zhuye-tools → zhuye-hooks 是单向依赖,而该规范化函数被两边
// 共用的安全判定点需要)。此处 re-export 保持
// `zhuye_tools::bash_command::*` 路径可用。
pub use zhuye_protocol::bash_command;
// v1.4 D2:BM25 抽为独立叶 crate(zhuye-bm25),memory 检索共用;
// 此处整体 re-export,`crate::bm25::*` 路径与外部 `zhuye_tools::bm25`
// 使用方保持源码兼容。
pub use zhuye_bm25 as bm25;
pub mod builtin_tools;
pub mod builtins;
/// git 能力族(快照 / 回退 / worktree)。
pub mod checkpoint;
/// 执行形态(批量调用 / 进程组 / 跨进程工具 / 文档读取)。
pub mod execution;
/// 守卫与预算(参数修复 / 循环守卫 / 输出预算 / 网络加固)。
pub mod guards;
/// 挂起交互(人工输入 / plan 审批 / 异步子代理登记表)。
pub mod interaction;
pub mod queue;
pub mod registry;
pub mod sandbox;
pub mod sanitize;
pub mod spec;
pub mod tool;

pub use approval::{
    ApprovalContext, ApprovalGate, ApprovalWaiters, AskUserInputWaiters, AskUserQuestionWaiters,
    McpElicitationWaiters, PendingLease, PendingRegistry, complete_approval,
    complete_ask_user_input, complete_ask_user_question, new_pending_registry, pending_register,
    pending_resolve, pending_snapshot,
};
pub use bash_command::{BASH_COMMAND_KEYS, resolve_bash_command};
pub use bm25::{Bm25Hit, rank_lines, tokenize};
pub use builtin_tools::{
    BUILTIN_TOOL_COUNT, BundledToolGates, UNGATED_BUILTIN_TOOL_COUNT, register_builtin_tools,
    register_builtin_tools_with_gates,
};
pub use builtins::bash::{BashCommandClass, classify_command};
pub use checkpoint::checkpoint_tool::{CheckpointTool, RewindTool};
pub use checkpoint::worktree::{
    SessionWorktreeState, WorktreeCoordinator, create_worktree, default_worktree_path,
    detach_worktree, git_head_ref, git_root, remove_worktree, run_git, sanitize_branch_name,
    worktree_dirty,
};
pub use execution::remote::{RemoteBridge, RemoteTool};
pub use guards::loop_guard::{GuardDecision, LoopGuard, LoopGuardConfig};
pub use interaction::human_input::HumanInputStore;
pub use interaction::pending_subagent::{
    PendingCompletionGuard, PendingSubagentRegistry, PendingSubagentResult,
};
pub use interaction::plan_approval::{
    PlanApprovalGate, PlanApprovalWaiters, complete_plan_approval,
};
pub use queue::{ToolCallRequest, ToolExecutionQueue, ToolResult};
pub use registry::{ToolRegistry, ToolSource};
pub use sanitize::{SanitizeConfig, SanitizeError, Sanitizer, sanitize_text};
pub use spec::ToolSpec;
pub use tool::{
    BackgroundBashSandbox, BackgroundTaskInfo, TaskSpawner, Tool, ToolContext, ToolEventForwarder,
};
// 从 protocol 重新导出,让下游用户能用
// `zhuye_tools::ToolError` / `zhuye_tools::ToolOutput`,
// 无需直接依赖 `zhuye-protocol`。
pub use zhuye_protocol::{ToolError, ToolOutput};

/// 下列模块的旧路径别名 —— 实现已下沉到对应子目录,但 crate 外有深路径
/// 引用（如 `zhuye_tools::result_budget::ResultBudget`），保留别名以免破坏
/// 既有调用点。
pub use execution::child_process;
pub use execution::remote;
pub use guards::result_budget;

/// v1.x 功能 6:`AgentDefinition.readonly = true` 时从可见工具集中排除的
/// 变更类工具名。这些工具会修改文件系统 / 执行任意命令,只读 agent 不应拥有。
/// bash 因可执行任意写操作,整体排除(v1 不细分读写子命令)。
///
/// review 修复:实际注册名是 `delete_file` / `notebook_edit`,此前列表里的
/// `delete` / `NotebookEdit` 永不命中,只读 agent 仍持有删除与 notebook 写
/// 工具。旧拼写保留以防外部 spec 手写旧名。
pub const READONLY_DENYLIST: &[&str] = &[
    "write",
    "edit",
    "delete",
    "delete_file",
    "bash",
    "NotebookEdit",
    "notebook_edit",
];
