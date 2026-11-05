//! 分支 / PR 元数据探测 —— 移植 Reflect-TUI
//! `tui_core/branch_summary.rs`(499 行)。
//!
//! 设计原样保留:本模块**只与 [`crate::runner::CommandRunner`] 对话**,
//! 不直接用 `tokio::process::Command` —— 同一套查找逻辑在客户端连接
//! 到嵌入式或远程执行端时都能工作。所有查找尽力而为:命令失败 / 缺
//! `git` / 缺 `gh` / 未认证 / 非 git 目录 / 仓库状态模糊,都导致可选
//! 元数据缺失,而不是用户可见的错误。

use std::path::Path;

use serde::Deserialize;

use crate::runner::{CommandOutput, CommandRunner, WorkspaceCommand};

/// `HEAD` 与分支比较基准之间的新增 / 删除行数。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitBranchDiffStats {
    pub additions: u64,
    pub deletions: u64,
}

/// 状态行 / CLI 展示用的 git 摘要(按 cwd 缓存由调用方负责)。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StatusLineGitSummary {
    pub pull_request: Option<StatusLinePullRequest>,
    pub branch_change_stats: Option<GitBranchDiffStats>,
}

/// 当前检出分支关联的打开 PR。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusLinePullRequest {
    pub number: u64,
    pub url: String,
}

#[derive(Deserialize)]
struct DefaultBranch {
    merge_ref: String,
}

#[derive(Deserialize)]
struct GhPullRequestView {
    number: u64,
    url: String,
    state: String,
}

#[derive(Deserialize)]
struct GhPullRequestApiItem {
    number: u64,
    #[serde(rename = "html_url")]
    url: String,
    state: String,
}

#[derive(Deserialize)]
struct GhRepoView {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: Option<String>,
    parent: Option<GhRepoParent>,
}

#[derive(Deserialize)]
struct GhRepoParent {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
}

/// 当前检出分支名(非 git 目录 / detached HEAD → `None`)。
pub async fn current_branch_name(runner: &dyn CommandRunner, cwd: &Path) -> Option<String> {
    let output = run_git_command(runner, cwd, &["branch", "--show-current"])
        .await
        .ok()?;
    if !output.success() {
        return None;
    }
    Some(output.stdout.trim().to_string()).filter(|name| !name.is_empty())
}

/// 解析一个工作目录的 PR 与分支变更元数据。
///
/// PR 与 diff 统计并发运行(两者独立且都可选)。返回的摘要适合按 cwd
/// 缓存;若查找完成前 cwd 变了,调用方应丢弃。
pub async fn status_line_git_summary(
    runner: &dyn CommandRunner,
    cwd: &Path,
) -> StatusLineGitSummary {
    let (pull_request, branch_change_stats) = tokio::join!(
        open_pull_request(runner, cwd),
        branch_diff_stats_to_default_branch(runner, cwd),
    );
    StatusLineGitSummary {
        pull_request,
        branch_change_stats,
    }
}

/// 统计 `HEAD` 与仓库默认分支之间的**已提交**行变更。
///
/// 比较基准是与已验证默认分支引用的 merge-base。未提交的工作树编辑
/// 被故意忽略 —— 摘要总结的是检出的分支,不是脏工作树。
async fn branch_diff_stats_to_default_branch(
    runner: &dyn CommandRunner,
    cwd: &Path,
) -> Option<GitBranchDiffStats> {
    let git_dir = run_git_command(runner, cwd, &["rev-parse", "--git-dir"])
        .await
        .ok()?;
    if !git_dir.success() {
        return None;
    }

    let default_branch = get_default_branch(runner, cwd).await?;
    let merge_base = run_git_command(
        runner,
        cwd,
        &["merge-base", "HEAD", &default_branch.merge_ref],
    )
    .await
    .ok()?;
    if !merge_base.success() {
        return None;
    }
    let merge_base = merge_base.stdout.trim();
    if merge_base.is_empty() {
        return None;
    }

    let range = format!("{merge_base}..HEAD");
    let numstat = run_git_command(runner, cwd, &["diff", "--numstat", &range])
        .await
        .ok()?;
    if !numstat.success() || numstat.truncated {
        return None;
    }

    let mut additions = 0_u64;
    let mut deletions = 0_u64;
    for line in numstat.stdout.lines() {
        let mut columns = line.split('\t');
        additions += columns
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        deletions += columns
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
    }

    Some(GitBranchDiffStats {
        additions,
        deletions,
    })
}

/// 按默认分支发现顺序返回 git 远程列表(`origin` 优先)。
async fn get_git_remotes(runner: &dyn CommandRunner, cwd: &Path) -> Option<Vec<String>> {
    let output = run_git_command(runner, cwd, &["remote"]).await.ok()?;
    if !output.success() {
        return None;
    }
    let mut remotes: Vec<String> = output.stdout.lines().map(str::to_string).collect();
    if let Some(pos) = remotes.iter().position(|remote| remote == "origin") {
        let origin = remotes.remove(pos);
        remotes.insert(0, origin);
    }
    Some(remotes)
}

/// 解析分支变更比较应使用的默认分支引用。
///
/// 优先**远程跟踪引用**而非本地分支:功能克隆 / 陈旧本地 `main` 不会
/// 膨胀 diff。无远程默认分支时回退本地 `main` / `master`。
async fn get_default_branch(runner: &dyn CommandRunner, cwd: &Path) -> Option<DefaultBranch> {
    let remotes = get_git_remotes(runner, cwd).await.unwrap_or_default();
    for remote in remotes {
        if let Some(branch) =
            get_remote_default_branch_from_symbolic_ref(runner, cwd, &remote).await
        {
            return Some(branch);
        }
        if let Some(branch) = get_remote_default_branch_from_remote_show(runner, cwd, &remote).await
        {
            return Some(branch);
        }
    }
    get_default_branch_local(runner, cwd).await
}

/// 把远程的符号 HEAD 解析成具体远程跟踪引用。
///
/// 返回的引用**必须验证存在** —— 没有这一步,陈旧的 `origin/HEAD` 会
/// 指向已不存在的引用,让后续 merge-base 探测在更隐蔽的地方失败。
async fn get_remote_default_branch_from_symbolic_ref(
    runner: &dyn CommandRunner,
    cwd: &Path,
    remote: &str,
) -> Option<DefaultBranch> {
    let remote_head = format!("refs/remotes/{remote}/HEAD");
    let output = run_git_command(runner, cwd, &["symbolic-ref", "--quiet", &remote_head])
        .await
        .ok()?;
    if !output.success() {
        return None;
    }
    let trimmed = output.stdout.trim();
    let remote_ref_prefix = format!("refs/remotes/{remote}/");
    trimmed.strip_prefix(&remote_ref_prefix)?;
    if !git_ref_exists(runner, cwd, trimmed).await {
        return None;
    }
    Some(DefaultBranch {
        merge_ref: trimmed.to_string(),
    })
}

/// 解析 `git remote show` 输出发现远程默认分支(符号 HEAD 未配时的备底)。
async fn get_remote_default_branch_from_remote_show(
    runner: &dyn CommandRunner,
    cwd: &Path,
    remote: &str,
) -> Option<DefaultBranch> {
    let output = run_git_command(runner, cwd, &["remote", "show", remote])
        .await
        .ok()?;
    if !output.success() {
        return None;
    }
    for line in output.stdout.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("HEAD branch:") else {
            continue;
        };
        let name = rest.trim();
        let remote_ref = format!("refs/remotes/{remote}/{name}");
        if !name.is_empty() && git_ref_exists(runner, cwd, &remote_ref).await {
            return Some(DefaultBranch {
                merge_ref: remote_ref,
            });
        }
    }
    None
}

/// 无远程默认分支时回退本地 `main` / `master`。
async fn get_default_branch_local(runner: &dyn CommandRunner, cwd: &Path) -> Option<DefaultBranch> {
    for candidate in ["main", "master"] {
        let local_ref = format!("refs/heads/{candidate}");
        if git_ref_exists(runner, cwd, &local_ref).await {
            return Some(DefaultBranch {
                merge_ref: local_ref,
            });
        }
    }
    None
}

/// git 引用是否存在。
async fn git_ref_exists(runner: &dyn CommandRunner, cwd: &Path, reference: &str) -> bool {
    run_git_command(
        runner,
        cwd,
        &["rev-parse", "--verify", "--quiet", reference],
    )
    .await
    .is_ok_and(|output| output.success())
}

/// 解析与当前检出关联的打开 PR。
///
/// 先走基于分支的查找(`gh pr view`,便宜),再回退基于提交的查找
/// (`gh api .../commits/<sha>/pulls`,覆盖 fork 工作流)。
async fn open_pull_request(
    runner: &dyn CommandRunner,
    cwd: &Path,
) -> Option<StatusLinePullRequest> {
    if let Some(pull_request) = open_pull_request_for_current_branch(runner, cwd).await {
        return Some(pull_request);
    }
    open_pull_request_for_head_commit(runner, cwd).await
}

async fn open_pull_request_for_current_branch(
    runner: &dyn CommandRunner,
    cwd: &Path,
) -> Option<StatusLinePullRequest> {
    let output = run_gh_command(runner, cwd, &["pr", "view", "--json", "number,url,state"])
        .await
        .ok()?;
    if !output.success() {
        return None;
    }
    pull_request_from_view_output(&output.stdout)
}

/// 在**父仓库优先**的仓库搜索顺序里找 HEAD 的打开 PR。
async fn open_pull_request_for_head_commit(
    runner: &dyn CommandRunner,
    cwd: &Path,
) -> Option<StatusLinePullRequest> {
    let head_sha = current_head_sha(runner, cwd).await?;
    for repo in gh_repo_search_order(runner, cwd).await? {
        let endpoint = format!("repos/{repo}/commits/{head_sha}/pulls");
        let output = run_gh_command(
            runner,
            cwd,
            &[
                "api",
                "-H",
                "Accept: application/vnd.github+json",
                &endpoint,
            ],
        )
        .await
        .ok()?;
        if output.success()
            && let Some(pull_request) = pull_request_from_api_output(&output.stdout)
        {
            return Some(pull_request);
        }
    }
    None
}

async fn current_head_sha(runner: &dyn CommandRunner, cwd: &Path) -> Option<String> {
    let output = run_git_command(runner, cwd, &["rev-parse", "HEAD"])
        .await
        .ok()?;
    if !output.success() {
        return None;
    }
    Some(output.stdout.trim().to_string()).filter(|sha| !sha.is_empty())
}

async fn gh_repo_search_order(runner: &dyn CommandRunner, cwd: &Path) -> Option<Vec<String>> {
    let output = run_gh_command(
        runner,
        cwd,
        &["repo", "view", "--json", "nameWithOwner,parent"],
    )
    .await
    .ok()?;
    if !output.success() {
        return None;
    }
    repo_search_order_from_output(&output.stdout)
}

fn pull_request_from_view_output(stdout: &str) -> Option<StatusLinePullRequest> {
    let pull_request = serde_json::from_str::<GhPullRequestView>(stdout).ok()?;
    pull_request
        .state
        .eq_ignore_ascii_case("open")
        .then_some(StatusLinePullRequest {
            number: pull_request.number,
            url: pull_request.url,
        })
}

fn pull_request_from_api_output(stdout: &str) -> Option<StatusLinePullRequest> {
    serde_json::from_str::<Vec<GhPullRequestApiItem>>(stdout)
        .ok()?
        .into_iter()
        .find(|pull_request| pull_request.state.eq_ignore_ascii_case("open"))
        .map(|pull_request| StatusLinePullRequest {
            number: pull_request.number,
            url: pull_request.url,
        })
}

fn repo_search_order_from_output(stdout: &str) -> Option<Vec<String>> {
    let repo = serde_json::from_str::<GhRepoView>(stdout).ok()?;
    let mut repos = Vec::new();
    if let Some(parent) = repo.parent {
        repos.push(parent.name_with_owner);
    }
    if let Some(name_with_owner) = repo.name_with_owner
        && !repos.iter().any(|repo| repo == &name_with_owner)
    {
        repos.push(name_with_owner);
    }
    if repos.is_empty() {
        return None;
    }
    Some(repos)
}

async fn run_git_command(
    runner: &dyn CommandRunner,
    cwd: &Path,
    args: &[&str],
) -> Result<CommandOutput, crate::runner::WorkspaceCommandError> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push("git".to_string());
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    let cmd = WorkspaceCommand::new(argv, cwd.to_string_lossy().into_owned())
        .with_env(vec![("GIT_OPTIONAL_LOCKS".to_string(), "0".to_string())]);
    runner.run(&cmd).await
}

/// 运行 `gh` 命令。禁用提示 —— 状态行探测是后台工作,需要认证 / 用户
/// 输入的命令应该失败并隐藏可选的 PR 项,而不是卡住。
async fn run_gh_command(
    runner: &dyn CommandRunner,
    cwd: &Path,
    args: &[&str],
) -> Result<CommandOutput, crate::runner::WorkspaceCommandError> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push("gh".to_string());
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    let cmd = WorkspaceCommand::new(argv, cwd.to_string_lossy().into_owned()).with_env(vec![
        ("GH_PROMPT_DISABLED".to_string(), "1".to_string()),
        ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
    ]);
    runner.run(&cmd).await
}

#[cfg(test)]
mod tests;
