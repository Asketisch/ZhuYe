//! 注意:实现已下沉至 `zhuye-git` crate(git 能力族统一家)。本模块保留
//! 原公开 API 作为薄委托,避免破坏 `zhuye-exec` 等既有调用方;新代码应
//! 直接用 `zhuye_git::checkpoint`(它另有 tag 枚举 / 安全快照 / sha
//! 白名单校验等本委托未暴露的能力)。

//! `checkpoint` — v1.2 P0-3 git 快照与回退助手。
//!
//! 给 `CheckpointTool` / `RewindTool` 提供 git 操作原语,复用
//! [`crate::worktree::run_git`]。设计取舍(见 gap doc P0-3):
//! - **只回退工作区文件**(git tree),**不截断会话历史**。会话 JSONL
//!   append-only,rewind 只追加 `Rewind` marker(见
//!   `zhuye_rollout::index::write_rewind_record`)。
//! - checkpoint 用 `git add -A` + `git commit` 拍快照;无 repo 时 `git init`。
//! - rewind 用 `git reset --hard <sha>` + `git clean -fd` 恢复文件树。
//!
//! 验证标准(gap doc):执行文件修改后 rewind 应恢复到修改前状态。

use std::path::Path;

use crate::ToolError;

pub use zhuye_git::checkpoint::{
    create as git_create_checkpoint, find_checkpoint, list as list_checkpoint_tags,
};

/// `git add -A` + `git commit` 拍快照,返回新 HEAD sha。
///
/// 无变更时返回当前 HEAD sha(不报错);无 repo 时先 `git init`。
/// 委托 `zhuye_git::checkpoint` 的等价原语(差异:zhuye-git 版无变更时
/// 返回 `Ok(None)` 且不自动 `git init`,这里维持旧签名语义)。
pub fn git_auto_commit(workspace: &Path, msg: &str) -> Result<String, ToolError> {
    if zhuye_git::checkpoint::is_repo(workspace) {
        if let Some(cp) = zhuye_git::checkpoint::create(workspace, msg)
            .map_err(|e| ToolError::Execution(e.to_string()))?
        {
            return Ok(cp.sha);
        }
        // 无变更 → 当前 HEAD。
        return git_current_sha(workspace);
    }
    // 非 repo:先 init 再提交(旧语义)。
    let out = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(workspace)
        .output()
        .map_err(|e| ToolError::Execution(format!("git init failed: {e}")))?;
    if !out.status.success() {
        return Err(ToolError::Execution(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    match zhuye_git::checkpoint::create(workspace, msg)
        .map_err(|e| ToolError::Execution(e.to_string()))?
    {
        Some(cp) => Ok(cp.sha),
        None => git_current_sha(workspace),
    }
}

/// 当前 HEAD sha。
pub fn git_current_sha(workspace: &Path) -> Result<String, ToolError> {
    zhuye_git::checkpoint::current_sha(workspace)
        .ok_or_else(|| ToolError::Execution("git rev-parse HEAD failed (not a repo?)".into()))
}

/// `git reset --hard <sha>` + `git clean -fd` 恢复文件树。
pub fn git_reset_hard(workspace: &Path, sha: &str) -> Result<(), ToolError> {
    let out = std::process::Command::new("git")
        .args(["reset", "--hard", sha])
        .current_dir(workspace)
        .output()
        .map_err(|e| ToolError::Execution(format!("git reset failed: {e}")))?;
    if !out.status.success() {
        return Err(ToolError::Execution(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    let out = std::process::Command::new("git")
        .args(["clean", "-fd"])
        .current_dir(workspace)
        .output()
        .map_err(|e| ToolError::Execution(format!("git clean failed: {e}")))?;
    if !out.status.success() {
        return Err(ToolError::Execution(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

/// sha 是否为仓库里的合法 commit。
pub fn is_valid_commit(workspace: &Path, sha: &str) -> bool {
    std::process::Command::new("git")
        .args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
        .current_dir(workspace)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
