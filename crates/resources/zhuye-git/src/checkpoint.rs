//! git checkpoint 快照与回退 —— 合并框架 `zhuye-tools/src/checkpoint.rs`
//! 与 Reflect-TUI `git_checkpoint.rs` 双实现。
//!
//! ## 为什么必须打 tag
//!
//! 只靠 subject 前缀 + `git log` 列 checkpoint 是不够的:`rewind` 会先做
//! 一次安全快照提交,然后 `reset --hard` 回旧提交 —— 这一步之后,安全
//! 快照提交**不在当前分支的历史上**了,`git log` 从 HEAD 出发看不到它,
//! 于是用户想「撤销这次回退」时那个 sha 已经列不出来、也 rewind 不过去。
//! tag 独立存活于 `refs/tags/`,`reset --hard` 不会动它,所以 checkpoint
//! 在回退之后依然可达、可列、可再回退。
//!
//! ## 安全约束
//!
//! 1. **回退前先做一次快照提交** —— 用户反悔了还能再回退到回退前的状态;
//! 2. **只接受本模块自己创建的 checkpoint 提交** —— tag 枚举 + subject
//!    前缀双重校验,畸形 sha / 用户手挑的任意提交都会被拒绝,不会被打进
//!    `reset --hard`;
//! 3. **非 git 仓库 / git 不在 PATH 都明确报错**,不静默成功;
//! 4. `git add -A -- :/` 覆盖整个仓库(仍按 .gitignore 过滤);`--no-verify`
//!    跳过 pre-commit hook(checkpoint 是安全网,不该被项目自己的
//!    lint/format hook 挡住而静默失败)。

use std::path::Path;
use std::process::Command;

/// checkpoint 提交的 subject 前缀。`list` 靠它识别自己创建的提交。
pub const CHECKPOINT_SUBJECT_PREFIX: &str = "zhuye-checkpoint:";

/// checkpoint 的 git tag 前缀(见模块 doc「为什么必须打 tag」)。
pub const CHECKPOINT_TAG_PREFIX: &str = "zhuye-cp/";

/// 一个 checkpoint 条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub sha: String,
    /// 提交 subject(去掉前缀后的 label 部分)。
    pub subject: String,
    /// 提交时间(ISO 8601)。
    pub committed_at: String,
}

/// git 操作失败的原因。给用户可操作的信息,不是裸的 stderr。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    /// `git` 可执行文件起不来(不在 PATH / 无执行权限)。
    GitUnavailable(String),
    /// 当前目录不在 git 仓库里。
    NotARepo,
    /// 目标 sha 不是本模块创建的 checkpoint 提交。
    NotACheckpoint(String),
    /// 其它 git 失败(带 stderr)。
    Failed(String),
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::GitUnavailable(e) => write!(f, "git 不可用: {e}"),
            GitError::NotARepo => write!(f, "当前目录不是 git 仓库"),
            GitError::NotACheckpoint(sha) => write!(f, "{sha} 不是烛夜创建的 checkpoint"),
            GitError::Failed(e) => write!(f, "git 失败: {e}"),
        }
    }
}

impl std::error::Error for GitError {}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, GitError> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| GitError::GitUnavailable(e.to_string()))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        // `rev-parse --show-toplevel` 在非仓库里退出码非 0,归到 NotARepo。
        if args.first() == Some(&"rev-parse") && stderr.contains("not a git repository") {
            return Err(GitError::NotARepo);
        }
        Err(GitError::Failed(stderr))
    }
}

/// 是否是 git 仓库。
pub fn is_repo(cwd: &Path) -> bool {
    run_git(cwd, &["rev-parse", "--show-toplevel"]).is_ok()
}

/// 创建 checkpoint:`git add -A` + `git commit` + 打 tag。
///
/// 没有改动时不创建空提交,返回 `Ok(None)` —— 调用方据此提示「无改动」。
/// `label` 为空时用默认文案。
pub fn create(cwd: &Path, label: &str) -> Result<Option<Checkpoint>, GitError> {
    create_snapshot(cwd, label, false)
}

/// 获取可回退的 checkpoint;干净的普通提交也会生成带标签的空快照。
pub fn ensure(cwd: &Path, label: &str) -> Result<Checkpoint, GitError> {
    if let Some(checkpoint) = create(cwd, label)? {
        return Ok(checkpoint);
    }
    if let Some(checkpoint) =
        current_sha(cwd).and_then(|sha| find_checkpoint(cwd, &sha, usize::MAX).ok())
    {
        return Ok(checkpoint);
    }
    create_snapshot(cwd, label, true)?
        .ok_or_else(|| GitError::Failed("无法创建 checkpoint 快照".into()))
}

fn create_snapshot(
    cwd: &Path,
    label: &str,
    allow_empty: bool,
) -> Result<Option<Checkpoint>, GitError> {
    if !is_repo(cwd) {
        return Err(GitError::NotARepo);
    }
    // `:/` 从仓库根匹配,即使调用方位于子目录也要保存全部将被重置的改动。
    run_git(cwd, &["add", "-A", "--", ":/"])?;
    // 有暂存内容才提交,避免 `nothing to commit` 的非零退出。
    let staged = run_git(cwd, &["diff", "--cached", "--name-only"])?;
    if staged.trim().is_empty() && !allow_empty {
        return Ok(None);
    }
    let label = label.trim();
    let subject = if label.is_empty() {
        CHECKPOINT_SUBJECT_PREFIX.to_string()
    } else {
        format!("{CHECKPOINT_SUBJECT_PREFIX} {label}")
    };
    // `--no-verify` 跳过 pre-commit hook:checkpoint 是安全网,不该被项目
    // 自己的 lint/format hook 挡住而静默失败。
    run_git(
        cwd,
        &["commit", "--no-verify", "-m", &subject, "--allow-empty"],
    )?;
    let sha = run_git(cwd, &["rev-parse", "HEAD"])?.trim().to_string();
    // 完整 SHA 避免短前缀碰撞;打标签失败时不能继续回退。
    let tag = format!("{CHECKPOINT_TAG_PREFIX}{sha}");
    run_git(cwd, &["tag", &tag, &sha]).map_err(|error| {
        GitError::Failed(format!("checkpoint {sha} 已提交但标签创建失败: {error}"))
    })?;
    let meta = run_git(cwd, &["log", "-1", "--format=%cI%x1f%s"])?;
    let (committed_at, subject) = match meta.trim().split_once('\u{1f}') {
        Some((t, s)) => (t.to_string(), s.to_string()),
        None => (String::new(), subject.clone()),
    };
    Ok(Some(Checkpoint {
        sha,
        subject: subject
            .strip_prefix(CHECKPOINT_SUBJECT_PREFIX)
            .unwrap_or(&subject)
            .trim()
            .to_string(),
        committed_at,
    }))
}

/// 列出最近的 checkpoint 提交(最新在前)。
///
/// 走 tag 而非 `git log`:回退之后的安全快照已不在分支历史上,只有 tag
/// 还指着它(见模块 doc)。
pub fn list(cwd: &Path, limit: usize) -> Result<Vec<Checkpoint>, GitError> {
    if !is_repo(cwd) {
        return Err(GitError::NotARepo);
    }
    if limit == 0 {
        return Ok(Vec::new());
    }
    // 一次 for-each-ref 拿全字段:%1f 作字段分隔符。
    let out = run_git(
        cwd,
        &[
            "for-each-ref",
            // 必须带 refs/tags/ 前缀:for-each-ref 的 pattern 按完整 refname
            // 匹配,裸 `zhuye-cp/*` 匹配不到任何东西(实测返回空)。
            &format!("refs/tags/{CHECKPOINT_TAG_PREFIX}*"),
            // 按被标记提交的 committer date 倒序。用 `-creatordate` 时同一秒内
            // 创建的多个 checkpoint 顺序不稳定(落到 refname 字典序),列表会乱。
            "--sort=-committerdate",
            "--format=%(objectname)%1f%(creatordate:iso-strict)%1f%(contents:subject)",
        ],
    )?;
    let mut checkpoints = Vec::new();
    for line in out.lines() {
        let Some((sha, rest)) = line.split_once('\u{1f}') else {
            continue;
        };
        let Some((committed_at, subject)) = rest.split_once('\u{1f}') else {
            continue;
        };
        let Some(label) = subject.strip_prefix(CHECKPOINT_SUBJECT_PREFIX) else {
            continue; // 只收自己创建的提交
        };
        checkpoints.push(Checkpoint {
            sha: sha.to_string(),
            subject: label.trim().to_string(),
            committed_at: committed_at.to_string(),
        });
        if checkpoints.len() >= limit {
            break;
        }
    }
    Ok(checkpoints)
}

/// 在 checkpoint 列表里按 sha(全等或短前缀)找目标。
///
/// `rewind` 用它做**白名单校验**:只接受本模块创建的 checkpoint,
/// 畸形 sha / 用户手挑的任意 commit 一律拒绝。
pub fn find_checkpoint(cwd: &Path, sha: &str, limit: usize) -> Result<Checkpoint, GitError> {
    if sha.is_empty() || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(GitError::NotACheckpoint(sha.to_string()));
    }
    let mut matches = list(cwd, limit)?
        .into_iter()
        .filter(|checkpoint| checkpoint.sha.starts_with(sha));
    let first = matches.next();
    if matches.next().is_some() {
        return Err(GitError::NotACheckpoint(sha.to_string()));
    }
    first.ok_or_else(|| GitError::NotACheckpoint(sha.to_string()))
}

/// 回退到指定 checkpoint:`git reset --hard <sha>`。
///
/// **回退前先做一次快照提交**,把当前工作区状态存下来 —— 用户反悔了还能
/// 再回退到回退前的状态。返回 `(回退到的 checkpoint, 回退前快照)`;
/// 干净树且当前提交已是 checkpoint 时快照为 `None`;否则建立可回退的快照。
pub fn rewind(cwd: &Path, sha: &str) -> Result<(Checkpoint, Option<Checkpoint>), GitError> {
    if !is_repo(cwd) {
        return Err(GitError::NotARepo);
    }
    // 校验目标确实是本模块创建的 checkpoint,避免畸形/任意 sha 被打进
    // `reset --hard`。
    let target = find_checkpoint(cwd, sha, 200)?;

    // 先存当前状态。有未提交改动时这一步尤其重要。
    let head_is_checkpoint =
        current_sha(cwd).is_some_and(|head| find_checkpoint(cwd, &head, usize::MAX).is_ok());
    let safety = create_snapshot(cwd, "pre-rewind safety snapshot", !head_is_checkpoint)?;

    run_git(cwd, &["reset", "--hard", &target.sha])?;
    Ok((target, safety))
}

/// 当前 HEAD 的 sha(非仓库 / git 缺失 → `None`)。
///
/// 供上层工具在 rewind 前记录 `from_sha`(`Rewind` rollout 记录用)。
pub fn current_sha(cwd: &Path) -> Option<String> {
    run_git(cwd, &["rev-parse", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests;
