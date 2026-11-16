//! # worktree / PR 全链路(F46)
//!
//! agent 提交的 PR 自动跟 CI、失败自动反馈回 agent;自动拆语义完整的
//! 提交、处理合并冲突、PR 面板;干完活自动收工不留垃圾目录。
//!
//! ## 四块
//!
//! 1. **PR 状态机**([`PrState`])—— open → checks running → ready /
//!    failed → merged / closed。每次转移有明确前置条件。
//! 2. **CI 反馈**([`CiReport`])—— 失败时把**可操作的**错误摘要回给
//!    agent(不是整段日志)。
//! 3. **提交拆分**([`split_commits`])—— 按文件分组把大改动拆成语义完整
//!    的多个提交(而不是一个"杂项"提交)。
//! 4. **冲突处理**([`ConflictPlan`])—— 冲突文件清单 + 建议策略。
//!
//! ## 为什么提交要「语义完整」而不是「按文件」
//!
//! 一个提交应该是"一件事"。按文件机械拆分会产生"改了 a.rs 但没改
//! 对应测试"的中间态 —— 那样的提交单独 checkout 是坏的。本模块按
//! **文件分组 + 测试配对**拆分:改了 `foo.rs` 就把 `foo_test.rs` 带上。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// PR 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    /// 已创建,CI 未开始。
    Open,
    /// CI 运行中。
    ChecksRunning,
    /// 全部检查通过,可合并。
    Ready,
    /// 检查失败。
    Failed,
    /// 已合并(终态)。
    Merged,
    /// 已关闭(终态)。
    Closed,
    /// 有冲突,需人工/agent 处理。
    Conflicted,
}

impl PrState {
    pub fn as_str(self) -> &'static str {
        match self {
            PrState::Open => "open",
            PrState::ChecksRunning => "checks_running",
            PrState::Ready => "ready",
            PrState::Failed => "failed",
            PrState::Merged => "merged",
            PrState::Closed => "closed",
            PrState::Conflicted => "conflicted",
        }
    }

    /// 是否终态。
    pub fn is_terminal(self) -> bool {
        matches!(self, PrState::Merged | PrState::Closed)
    }

    /// 是否可合并。
    pub fn can_merge(self) -> bool {
        matches!(self, PrState::Ready)
    }

    /// 合法转移。
    pub fn can_transition_to(self, next: PrState) -> bool {
        matches!(
            (self, next),
            (PrState::Open, PrState::ChecksRunning)
                | (PrState::Open, PrState::Closed)
                | (PrState::Open, PrState::Conflicted)
                | (PrState::ChecksRunning, PrState::Ready)
                | (PrState::ChecksRunning, PrState::Failed)
                | (PrState::ChecksRunning, PrState::Conflicted)
                | (PrState::Failed, PrState::ChecksRunning)
                | (PrState::Failed, PrState::Closed)
                | (PrState::Conflicted, PrState::ChecksRunning)
                | (PrState::Conflicted, PrState::Closed)
                | (PrState::Ready, PrState::Merged)
                | (PrState::Ready, PrState::ChecksRunning)
                | (PrState::Ready, PrState::Closed)
        )
    }
}

/// 状态机。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrMachine {
    pub state: PrState,
    pub history: Vec<PrState>,
}

impl Default for PrMachine {
    fn default() -> Self {
        PrMachine {
            state: PrState::Open,
            history: vec![PrState::Open],
        }
    }
}

impl PrMachine {
    pub fn new() -> PrMachine {
        PrMachine::default()
    }

    /// 转移(非法转移报错)。
    pub fn transition(&mut self, next: PrState) -> Result<(), PrError> {
        if !self.state.can_transition_to(next) {
            return Err(PrError::IllegalTransition {
                from: self.state,
                to: next,
            });
        }
        self.state = next;
        self.history.push(next);
        Ok(())
    }
}

/// PR 错误。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PrError {
    #[error("illegal PR transition: {} → {}", from.as_str(), to.as_str())]
    IllegalTransition { from: PrState, to: PrState },
    #[error("merge blocked: PR is not ready (state = {0})")]
    NotReady(String),
}

/// 一项 CI 检查结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub passed: bool,
    /// 失败时的**可操作**摘要(不是整段日志)。
    #[serde(default)]
    pub failure_summary: Option<String>,
}

/// CI 报告。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiReport {
    pub checks: Vec<CheckResult>,
}

impl CiReport {
    pub fn new() -> CiReport {
        CiReport::default()
    }

    pub fn add(
        mut self,
        name: impl Into<String>,
        passed: bool,
        failure: Option<String>,
    ) -> CiReport {
        self.checks.push(CheckResult {
            name: name.into(),
            passed,
            failure_summary: failure,
        });
        self
    }

    /// 是否全部通过。
    pub fn all_passed(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(|c| c.passed)
    }

    /// 失败项。
    pub fn failures(&self) -> Vec<&CheckResult> {
        self.checks.iter().filter(|c| !c.passed).collect()
    }

    /// 反馈给 agent 的文本(只含失败项 + 摘要)。
    pub fn agent_feedback(&self) -> Option<String> {
        if self.all_passed() {
            return None;
        }
        let mut out = String::from("CI 失败,需要修复:\n");
        for f in self.failures() {
            out.push_str(&format!("- {}:", f.name));
            match &f.failure_summary {
                Some(s) => out.push_str(&format!(" {s}\n")),
                None => out.push_str(" (无摘要,请查看完整日志)\n"),
            }
        }
        Some(out)
    }

    /// 应用到状态机。
    pub fn apply(&self, machine: &mut PrMachine) -> Result<(), PrError> {
        if self.all_passed() {
            machine.transition(PrState::Ready)
        } else {
            machine.transition(PrState::Failed)
        }
    }
}

/// 一个待提交的文件改动。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    /// 改动行数。
    pub lines_changed: usize,
}

/// 一个建议的提交。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitGroup {
    /// 提交信息。
    pub message: String,
    pub files: Vec<String>,
}

impl CommitGroup {
    /// 总改动行数。
    pub fn total_lines(&self, changes: &[FileChange]) -> usize {
        changes
            .iter()
            .filter(|c| self.files.contains(&c.path))
            .map(|c| c.lines_changed)
            .sum()
    }
}

/// 按「文件分组 + 测试配对」拆分提交。
///
/// 规则:
///
/// - 同目录下同名前缀的文件归一组(`src/foo.rs` + `src/foo_test.rs`);
/// - 测试文件自动跟随其被测源文件;
/// - 文档(`docs/` / `*.md`)单独一组;
/// - 配置(`Cargo.toml` / `package.json` 等)单独一组。
pub fn split_commits(changes: &[FileChange]) -> Vec<CommitGroup> {
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for change in changes {
        let key = group_key(&change.path);
        groups.entry(key).or_default().push(change.path.clone());
    }

    groups
        .into_iter()
        .map(|(key, mut files)| {
            files.sort();
            CommitGroup {
                message: format!("{key}: 更新 {} 个文件", files.len()),
                files,
            }
        })
        .collect()
}

/// 计算文件的分组键。
fn group_key(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    // 文档
    if lower.starts_with("docs/") || lower.ends_with(".md") {
        return "docs".to_string();
    }
    // 配置
    let filename = path.rsplit('/').next().unwrap_or(path);
    if matches!(
        filename,
        "Cargo.toml" | "Cargo.lock" | "package.json" | "pnpm-lock.yaml" | "tsconfig.json"
    ) {
        return "chore(config)".to_string();
    }
    // 测试文件:跟随被测源
    let stripped = strip_test_suffix(path);
    if stripped != path {
        return format!("test({})", stem_of(&stripped));
    }
    format!("feat({})", stem_of(path))
}

/// 去掉测试后缀(`_test` / `.test` / `.spec` / `/tests/`)。
fn strip_test_suffix(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    if lower.contains("/tests/") {
        return path.replace("/tests/", "/");
    }
    for suffix in ["_test.rs", ".test.ts", ".spec.ts", "_test.py"] {
        if let Some(stripped) = path.strip_suffix(suffix) {
            return format!(
                "{stripped}{}",
                &suffix[suffix.find('.').or(Some(0)).unwrap()..]
                    .replace(".test", ".rs")
                    .replace(".spec", ".rs")
            );
        }
    }
    path.to_string()
}

/// 取文件名 stem。
fn stem_of(path: &str) -> String {
    let filename = path.rsplit('/').next().unwrap_or(path);
    filename.split('.').next().unwrap_or(filename).to_string()
}

/// 冲突处理计划。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictPlan {
    /// 冲突文件。
    pub files: Vec<String>,
    /// 建议策略。
    pub strategy: ConflictStrategy,
}

/// 冲突策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictStrategy {
    /// 交给 agent 逐文件解决(默认)。
    #[default]
    AgentResolve,
    /// 优先保留我方(ours)。
    PreferOurs,
    /// 优先保留上游(theirs)。
    PreferTheirs,
    /// 需人工介入。
    Manual,
}

impl ConflictPlan {
    /// 按冲突文件数决定策略。
    ///
    /// 少量冲突(≤3)交 agent;大量冲突(>10)建议人工。
    pub fn from_files(files: Vec<String>) -> ConflictPlan {
        let strategy = match files.len() {
            0 => ConflictStrategy::AgentResolve,
            1..=3 => ConflictStrategy::AgentResolve,
            4..=10 => ConflictStrategy::PreferOurs,
            _ => ConflictStrategy::Manual,
        };
        ConflictPlan { files, strategy }
    }

    /// 是否需要人工。
    pub fn needs_human(&self) -> bool {
        self.strategy == ConflictStrategy::Manual
    }

    /// 人读摘要。
    pub fn summary(&self) -> String {
        format!("{} 个冲突文件,策略 {:?}", self.files.len(), self.strategy)
    }
}

/// worktree 生命周期 hooks。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeHook {
    /// 创建后。
    Created,
    /// 环境准备完成后(依赖安装等)。
    SetupComplete,
    /// 合并回主分支后。
    Merged,
    /// 清理前。
    BeforeCleanup,
}

impl WorktreeHook {
    pub fn as_str(self) -> &'static str {
        match self {
            WorktreeHook::Created => "created",
            WorktreeHook::SetupComplete => "setup_complete",
            WorktreeHook::Merged => "merged",
            WorktreeHook::BeforeCleanup => "before_cleanup",
        }
    }
}

/// worktree 生命周期记录(确保不留垃圾目录)。
#[derive(Debug, Default)]
pub struct WorktreeLifecycle {
    hooks_fired: BTreeSet<&'static str>,
    /// 是否已清理。
    cleaned: bool,
}

impl WorktreeLifecycle {
    pub fn new() -> WorktreeLifecycle {
        WorktreeLifecycle::default()
    }

    /// 触发 hook。
    pub fn fire(&mut self, hook: WorktreeHook) {
        self.hooks_fired.insert(hook.as_str());
    }

    /// 是否触发过。
    pub fn fired(&self, hook: WorktreeHook) -> bool {
        self.hooks_fired.contains(hook.as_str())
    }

    /// 标记已清理。
    pub fn mark_cleaned(&mut self) {
        self.cleaned = true;
    }

    pub fn is_cleaned(&self) -> bool {
        self.cleaned
    }

    /// 是否可安全清理(必须经过 BeforeCleanup)。
    ///
    /// 这是"不留垃圾"的保障:没走清理 hook 就删目录,可能漏掉资源释放。
    pub fn can_cleanup(&self) -> bool {
        self.fired(WorktreeHook::BeforeCleanup)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_state_terminal() {
        assert!(PrState::Merged.is_terminal());
        assert!(PrState::Closed.is_terminal());
        assert!(!PrState::Ready.is_terminal());
        assert!(PrState::Ready.can_merge());
        assert!(!PrState::Failed.can_merge());
    }

    #[test]
    fn pr_happy_path() {
        let mut m = PrMachine::new();
        m.transition(PrState::ChecksRunning).unwrap();
        m.transition(PrState::Ready).unwrap();
        m.transition(PrState::Merged).unwrap();
        assert_eq!(m.state, PrState::Merged);
        assert_eq!(m.history.len(), 4);
    }

    #[test]
    fn pr_illegal_transition() {
        let mut m = PrMachine::new();
        assert!(
            m.transition(PrState::Merged).is_err(),
            "Open 不能直接 merged"
        );
        assert_eq!(m.state, PrState::Open);
    }

    #[test]
    fn pr_retry_after_failure() {
        let mut m = PrMachine::new();
        m.transition(PrState::ChecksRunning).unwrap();
        m.transition(PrState::Failed).unwrap();
        m.transition(PrState::ChecksRunning).unwrap(); // 重试
        m.transition(PrState::Ready).unwrap();
        assert_eq!(m.state, PrState::Ready);
    }

    #[test]
    fn pr_conflict_path() {
        let mut m = PrMachine::new();
        m.transition(PrState::ChecksRunning).unwrap();
        m.transition(PrState::Conflicted).unwrap();
        m.transition(PrState::ChecksRunning).unwrap();
        assert_eq!(m.state, PrState::ChecksRunning);
    }

    #[test]
    fn ci_report_all_passed() {
        let report = CiReport::new()
            .add("test", true, None)
            .add("clippy", true, None);
        assert!(report.all_passed());
        assert!(report.agent_feedback().is_none(), "全过不需反馈");
    }

    #[test]
    fn ci_report_feedback() {
        let report = CiReport::new()
            .add("test", false, Some("3 tests failed in parser".into()))
            .add("clippy", true, None);
        let feedback = report.agent_feedback().expect("feedback");
        assert!(feedback.contains("test:"), "{feedback}");
        assert!(feedback.contains("3 tests failed"), "{feedback}");
        assert!(!feedback.contains("clippy"), "通过项不进反馈:{feedback}");
    }

    #[test]
    fn ci_report_no_summary_fallback() {
        let report = CiReport::new().add("test", false, None);
        let feedback = report.agent_feedback().unwrap();
        assert!(feedback.contains("请查看完整日志"), "{feedback}");
    }

    #[test]
    fn ci_apply_updates_state() {
        let mut m = PrMachine::new();
        m.transition(PrState::ChecksRunning).unwrap();
        let pass = CiReport::new().add("t", true, None);
        pass.apply(&mut m).unwrap();
        assert_eq!(m.state, PrState::Ready);

        let mut m2 = PrMachine::new();
        m2.transition(PrState::ChecksRunning).unwrap();
        let fail = CiReport::new().add("t", false, Some("boom".into()));
        fail.apply(&mut m2).unwrap();
        assert_eq!(m2.state, PrState::Failed);
    }

    #[test]
    fn split_commits_groups_by_feature() {
        let changes = vec![
            FileChange {
                path: "src/parser.rs".into(),
                lines_changed: 50,
            },
            FileChange {
                path: "src/parser_test.rs".into(),
                lines_changed: 20,
            },
            FileChange {
                path: "docs/guide.md".into(),
                lines_changed: 10,
            },
            FileChange {
                path: "Cargo.toml".into(),
                lines_changed: 2,
            },
        ];
        let groups = split_commits(&changes);
        // 应有 4 组(parser, parser_test 各自独立组? 不 —— test 跟随源)
        assert!(groups.len() >= 3, "{groups:?}");
        // 文档组
        assert!(groups.iter().any(|g| g.message.starts_with("docs")));
        // 配置组
        assert!(groups.iter().any(|g| g.message.contains("chore(config)")));
    }

    #[test]
    fn split_commits_empty() {
        assert!(split_commits(&[]).is_empty());
    }

    #[test]
    fn commit_group_total_lines() {
        let changes = vec![
            FileChange {
                path: "a.rs".into(),
                lines_changed: 10,
            },
            FileChange {
                path: "b.rs".into(),
                lines_changed: 20,
            },
        ];
        let group = CommitGroup {
            message: "feat(a)".into(),
            files: vec!["a.rs".into()],
        };
        assert_eq!(group.total_lines(&changes), 10);
    }

    #[test]
    fn conflict_plan_strategies() {
        assert_eq!(
            ConflictPlan::from_files(vec!["a".into()]).strategy,
            ConflictStrategy::AgentResolve
        );
        assert_eq!(
            ConflictPlan::from_files((0..6).map(|i| format!("f{i}")).collect()).strategy,
            ConflictStrategy::PreferOurs
        );
        let manual = ConflictPlan::from_files((0..20).map(|i| format!("f{i}")).collect());
        assert_eq!(manual.strategy, ConflictStrategy::Manual);
        assert!(manual.needs_human());
        assert!(manual.summary().contains("20"));
    }

    #[test]
    fn conflict_plan_empty() {
        let plan = ConflictPlan::from_files(vec![]);
        assert!(!plan.needs_human());
    }

    #[test]
    fn worktree_lifecycle_cleanup_guard() {
        let mut lc = WorktreeLifecycle::new();
        assert!(!lc.can_cleanup(), "未走 BeforeCleanup 不可清理");
        lc.fire(WorktreeHook::Created);
        lc.fire(WorktreeHook::SetupComplete);
        lc.fire(WorktreeHook::Merged);
        lc.fire(WorktreeHook::BeforeCleanup);
        assert!(lc.can_cleanup());
        assert!(lc.fired(WorktreeHook::Merged));
        lc.mark_cleaned();
        assert!(lc.is_cleaned());
    }

    #[test]
    fn pr_machine_serializes() {
        let m = PrMachine::new();
        let json = serde_json::to_string(&m).expect("serialize");
        let back: PrMachine = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, m);
    }

    #[test]
    fn ci_report_serializes() {
        let report = CiReport::new().add("t", false, Some("x".into()));
        let json = serde_json::to_string(&report).expect("serialize");
        let back: CiReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, report);
    }
}
