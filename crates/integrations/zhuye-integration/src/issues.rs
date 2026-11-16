//! # 工单驱动(F36)
//!
//! 连 GitHub Issues / Linear 当任务源:新 issue 自动认领开干、依赖排序、
//! 早上给一份「站会报告」。
//!
//! ## 四块
//!
//! 1. **工单归一**([`Issue`])—— 把 GitHub / Linear 的不同形状统一成
//!    一份结构(编号 / 标题 / 正文 / 标签 / 状态 / 依赖)。
//! 2. **认领策略**([`ClaimPolicy`])—— 哪些 issue 该自动开干(有 `agent`
//!    标签?未分配?优先级?)、哪些跳过。
//! 3. **依赖排序**([`order_by_dependencies`])—— issue 之间可以 blocked-by,
//!    按拓扑序处理(与 F56 任务图同源)。
//! 4. **站会报告**([`standup_report`])—— 昨天做了什么 / 今天做什么 /
//!    卡在哪。
//!
//! ## 为什么认领要策略化
//!
//! 「新 issue 自动开干」听起来很好,但没人希望 agent 去动一个标记
//! `do-not-touch` 或已在别人手里的 issue。策略让「自动」变成**可审计的
//! 规则**,而不是"agent 想干就干"。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// 工单来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueSource {
    GitHub,
    Linear,
    Local,
}

impl IssueSource {
    pub fn as_str(self) -> &'static str {
        match self {
            IssueSource::GitHub => "github",
            IssueSource::Linear => "linear",
            IssueSource::Local => "local",
        }
    }
}

/// 工单状态(归一后)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueStatus {
    Open,
    InProgress,
    Blocked,
    Done,
    Closed,
}

impl IssueStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            IssueStatus::Open => "open",
            IssueStatus::InProgress => "in_progress",
            IssueStatus::Blocked => "blocked",
            IssueStatus::Done => "done",
            IssueStatus::Closed => "closed",
        }
    }

    /// 是否终态。
    pub fn is_terminal(self) -> bool {
        matches!(self, IssueStatus::Done | IssueStatus::Closed)
    }
}

/// 归一后的工单。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// 来源内的编号(如 `#42`)。
    pub number: String,
    pub source: IssueSource,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub labels: Vec<String>,
    pub status: IssueStatus,
    /// 被哪些 issue 阻塞。
    #[serde(default)]
    pub blocked_by: Vec<String>,
    /// 是否已分配给某人。
    #[serde(default)]
    pub assignee: Option<String>,
    /// 优先级(数字越小越急;`None` = 未标)。
    #[serde(default)]
    pub priority: Option<u8>,
}

impl Issue {
    pub fn new(number: impl Into<String>, source: IssueSource, title: impl Into<String>) -> Issue {
        Issue {
            number: number.into(),
            source,
            title: title.into(),
            body: String::new(),
            labels: Vec::new(),
            status: IssueStatus::Open,
            blocked_by: Vec::new(),
            assignee: None,
            priority: None,
        }
    }

    pub fn with_labels(mut self, labels: Vec<String>) -> Issue {
        self.labels = labels;
        self
    }

    pub fn with_blocked_by(mut self, deps: Vec<String>) -> Issue {
        self.blocked_by = deps;
        self
    }

    /// 是否有某标签(大小写不敏感)。
    pub fn has_label(&self, label: &str) -> bool {
        let needle = label.to_ascii_lowercase();
        self.labels.iter().any(|l| l.to_ascii_lowercase() == needle)
    }

    /// 简短展示(`#42 修复登录`).
    pub fn display(&self) -> String {
        format!("#{} {}", self.number, self.title)
    }
}

/// 认领策略。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimPolicy {
    /// 必须带的标签(任一即可)。空 = 不限。
    #[serde(default)]
    pub require_labels: Vec<String>,
    /// 带了这些标签就不动(优先级最高)。
    #[serde(default)]
    pub exclude_labels: Vec<String>,
    /// 是否只认领未分配的。
    #[serde(default = "default_true")]
    pub unassigned_only: bool,
    /// 最高可认领的优先级(数字越小越急;`None` = 不限)。
    #[serde(default)]
    pub max_priority: Option<u8>,
}

fn default_true() -> bool {
    true
}

impl Default for ClaimPolicy {
    fn default() -> Self {
        ClaimPolicy {
            require_labels: vec!["agent".to_string()],
            exclude_labels: vec!["do-not-touch".to_string(), "wontfix".to_string()],
            unassigned_only: true,
            max_priority: None,
        }
    }
}

impl ClaimPolicy {
    /// 该 issue 是否应被认领。
    pub fn should_claim(&self, issue: &Issue) -> ClaimDecision {
        // ① 排除标签优先(即使也带了 require 标签)。
        for label in &self.exclude_labels {
            if issue.has_label(label) {
                return ClaimDecision {
                    claim: false,
                    reason: format!("含排除标签 {label}"),
                };
            }
        }
        // ② 已分配
        if self.unassigned_only && issue.assignee.is_some() {
            return ClaimDecision {
                claim: false,
                reason: "已分配给他人".to_string(),
            };
        }
        // ③ 终态
        if issue.status.is_terminal() {
            return ClaimDecision {
                claim: false,
                reason: format!("已是终态 {}", issue.status.as_str()),
            };
        }
        // ④ 必须标签
        if !self.require_labels.is_empty()
            && !self.require_labels.iter().any(|l| issue.has_label(l))
        {
            return ClaimDecision {
                claim: false,
                reason: format!("缺少必需标签(需其一:{:?})", self.require_labels),
            };
        }
        // ⑤ 优先级
        if let (Some(max), Some(priority)) = (self.max_priority, issue.priority)
            && priority > max
        {
            return ClaimDecision {
                claim: false,
                reason: format!("优先级 {priority} 低于阈值 {max}"),
            };
        }
        ClaimDecision {
            claim: true,
            reason: "满足认领条件".to_string(),
        }
    }

    /// 筛选可认领的 issue。
    pub fn claimable<'a>(&self, issues: &'a [Issue]) -> Vec<&'a Issue> {
        issues
            .iter()
            .filter(|i| self.should_claim(i).claim)
            .collect()
    }
}

/// 认领决定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimDecision {
    pub claim: bool,
    pub reason: String,
}

/// 按依赖排序(拓扑序)。
///
/// 返回 `(顺序, 环)` —— 有环时顺序为可排序部分,环单独返回。
pub fn order_by_dependencies(issues: &[Issue]) -> (Vec<&Issue>, Option<Vec<String>>) {
    let by_number: BTreeMap<&str, &Issue> = issues.iter().map(|i| (i.number.as_str(), i)).collect();

    let mut indegree: BTreeMap<&str, usize> = BTreeMap::new();
    let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for issue in issues {
        indegree.entry(issue.number.as_str()).or_insert(0);
        for dep in &issue.blocked_by {
            if by_number.contains_key(dep.as_str()) {
                children
                    .entry(dep.as_str())
                    .or_default()
                    .push(issue.number.as_str());
                *indegree.entry(issue.number.as_str()).or_insert(0) += 1;
            }
        }
    }

    let mut ready: Vec<&str> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(n, _)| *n)
        .collect();
    ready.sort_unstable();

    let mut order: Vec<&Issue> = Vec::new();
    let mut queue: std::collections::VecDeque<&str> = ready.into();
    while let Some(number) = queue.pop_front() {
        if let Some(issue) = by_number.get(number) {
            order.push(*issue);
        }
        if let Some(kids) = children.get(number) {
            let mut next: Vec<&str> = Vec::new();
            for kid in kids {
                if let Some(d) = indegree.get_mut(kid) {
                    *d -= 1;
                    if *d == 0 {
                        next.push(kid);
                    }
                }
            }
            next.sort_unstable();
            for n in next {
                queue.push_back(n);
            }
        }
    }

    let cycle = if order.len() < issues.len() {
        let ordered: BTreeSet<&str> = order.iter().map(|i| i.number.as_str()).collect();
        Some(
            issues
                .iter()
                .filter(|i| !ordered.contains(i.number.as_str()))
                .map(|i| i.number.clone())
                .collect(),
        )
    } else {
        None
    };

    (order, cycle)
}

/// 站会报告。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StandupReport {
    /// 已完成(自上次报告)。
    pub done: Vec<String>,
    /// 进行中。
    pub in_progress: Vec<String>,
    /// 阻塞中(带原因)。
    pub blocked: Vec<(String, String)>,
    /// 接下来要做的。
    pub next: Vec<String>,
}

impl StandupReport {
    /// 渲染成 Markdown。
    pub fn render(&self) -> String {
        let mut out = String::from("# 站会报告\n\n");
        out.push_str(&format!("## 已完成({})\n", self.done.len()));
        for d in &self.done {
            out.push_str(&format!("- {d}\n"));
        }
        out.push_str(&format!("\n## 进行中({})\n", self.in_progress.len()));
        for i in &self.in_progress {
            out.push_str(&format!("- {i}\n"));
        }
        out.push_str(&format!("\n## 阻塞({})\n", self.blocked.len()));
        for (item, reason) in &self.blocked {
            out.push_str(&format!("- {item} —— {reason}\n"));
        }
        out.push_str(&format!("\n## 下一步({})\n", self.next.len()));
        for n in &self.next {
            out.push_str(&format!("- {n}\n"));
        }
        out
    }

    /// 是否全部为空。
    pub fn is_empty(&self) -> bool {
        self.done.is_empty()
            && self.in_progress.is_empty()
            && self.blocked.is_empty()
            && self.next.is_empty()
    }
}

/// 生成站会报告。
pub fn standup_report(issues: &[Issue]) -> StandupReport {
    let mut report = StandupReport::default();
    for issue in issues {
        let display = issue.display();
        match issue.status {
            IssueStatus::Done => report.done.push(display),
            IssueStatus::InProgress => report.in_progress.push(display),
            IssueStatus::Blocked => {
                let reason = if issue.blocked_by.is_empty() {
                    "未标注阻塞原因".to_string()
                } else {
                    format!("被 {} 阻塞", issue.blocked_by.join(", "))
                };
                report.blocked.push((display, reason));
            }
            IssueStatus::Open => {
                // 未被阻塞的 open issue 进"下一步"
                if issue.blocked_by.is_empty() {
                    report.next.push(display);
                }
            }
            IssueStatus::Closed => {}
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(number: &str, title: &str) -> Issue {
        Issue::new(number, IssueSource::GitHub, title)
    }

    #[test]
    fn issue_label_matching() {
        let i = issue("1", "t").with_labels(vec!["Agent".into(), "bug".into()]);
        assert!(i.has_label("agent"), "大小写不敏感");
        assert!(i.has_label("BUG"));
        assert!(!i.has_label("feature"));
    }

    #[test]
    fn issue_display() {
        assert_eq!(issue("42", "修复登录").display(), "#42 修复登录");
    }

    #[test]
    fn policy_claims_agent_labeled_unassigned() {
        let policy = ClaimPolicy::default();
        let i = issue("1", "t").with_labels(vec!["agent".into()]);
        let d = policy.should_claim(&i);
        assert!(d.claim, "{}", d.reason);
    }

    #[test]
    fn policy_rejects_missing_required_label() {
        let policy = ClaimPolicy::default();
        let i = issue("1", "t").with_labels(vec!["bug".into()]);
        let d = policy.should_claim(&i);
        assert!(!d.claim);
        assert!(d.reason.contains("必需标签"), "{}", d.reason);
    }

    #[test]
    fn policy_exclude_label_wins() {
        let policy = ClaimPolicy::default();
        let i = issue("1", "t").with_labels(vec!["agent".into(), "do-not-touch".into()]);
        let d = policy.should_claim(&i);
        assert!(!d.claim, "排除标签优先于必需标签");
        assert!(d.reason.contains("do-not-touch"));
    }

    #[test]
    fn policy_rejects_assigned() {
        let policy = ClaimPolicy::default();
        let mut i = issue("1", "t").with_labels(vec!["agent".into()]);
        i.assignee = Some("alice".into());
        let d = policy.should_claim(&i);
        assert!(!d.claim);
        assert!(d.reason.contains("已分配"));
    }

    #[test]
    fn policy_allows_assigned_when_configured() {
        let policy = ClaimPolicy {
            unassigned_only: false,
            ..Default::default()
        };
        let mut i = issue("1", "t").with_labels(vec!["agent".into()]);
        i.assignee = Some("alice".into());
        assert!(policy.should_claim(&i).claim);
    }

    #[test]
    fn policy_rejects_terminal() {
        let policy = ClaimPolicy::default();
        let mut i = issue("1", "t").with_labels(vec!["agent".into()]);
        i.status = IssueStatus::Done;
        assert!(!policy.should_claim(&i).claim);
    }

    #[test]
    fn policy_priority_threshold() {
        let policy = ClaimPolicy {
            max_priority: Some(2),
            ..Default::default()
        };
        let mut urgent = issue("1", "t").with_labels(vec!["agent".into()]);
        urgent.priority = Some(1);
        assert!(policy.should_claim(&urgent).claim);

        let mut low = issue("2", "t").with_labels(vec!["agent".into()]);
        low.priority = Some(5);
        let d = policy.should_claim(&low);
        assert!(!d.claim);
        assert!(d.reason.contains("优先级"));
    }

    #[test]
    fn policy_claimable_filters() {
        let policy = ClaimPolicy::default();
        let issues = vec![
            issue("1", "a").with_labels(vec!["agent".into()]),
            issue("2", "b").with_labels(vec!["bug".into()]),
            issue("3", "c").with_labels(vec!["agent".into(), "wontfix".into()]),
        ];
        let claimable = policy.claimable(&issues);
        assert_eq!(claimable.len(), 1);
        assert_eq!(claimable[0].number, "1");
    }

    #[test]
    fn dependency_ordering() {
        let issues = vec![
            issue("3", "third").with_blocked_by(vec!["2".into()]),
            issue("1", "first"),
            issue("2", "second").with_blocked_by(vec!["1".into()]),
        ];
        let (order, cycle) = order_by_dependencies(&issues);
        assert!(cycle.is_none());
        let numbers: Vec<&str> = order.iter().map(|i| i.number.as_str()).collect();
        assert_eq!(numbers, vec!["1", "2", "3"]);
    }

    #[test]
    fn dependency_cycle_detected() {
        let issues = vec![
            issue("1", "a").with_blocked_by(vec!["2".into()]),
            issue("2", "b").with_blocked_by(vec!["1".into()]),
        ];
        let (order, cycle) = order_by_dependencies(&issues);
        assert!(order.is_empty());
        let cycle = cycle.expect("应检测到环");
        assert!(cycle.contains(&"1".to_string()));
        assert!(cycle.contains(&"2".to_string()));
    }

    #[test]
    fn dependency_ignores_unknown_refs() {
        let issues = vec![issue("1", "a").with_blocked_by(vec!["999".into()])];
        let (order, cycle) = order_by_dependencies(&issues);
        assert_eq!(order.len(), 1, "未知依赖忽略,不阻塞");
        assert!(cycle.is_none());
    }

    #[test]
    fn standup_classifies_issues() {
        let mut done = issue("1", "done thing");
        done.status = IssueStatus::Done;
        let mut wip = issue("2", "wip thing");
        wip.status = IssueStatus::InProgress;
        let mut blocked = issue("3", "blocked thing").with_blocked_by(vec!["1".into()]);
        blocked.status = IssueStatus::Blocked;
        let open = issue("4", "open thing");

        let report = standup_report(&[done, wip, blocked, open]);
        assert_eq!(report.done.len(), 1);
        assert_eq!(report.in_progress.len(), 1);
        assert_eq!(report.blocked.len(), 1);
        assert_eq!(report.next.len(), 1);
        assert!(report.blocked[0].1.contains("1"), "{:?}", report.blocked);
    }

    #[test]
    fn standup_render_markdown() {
        let mut done = issue("1", "done thing");
        done.status = IssueStatus::Done;
        let report = standup_report(&[done]);
        let md = report.render();
        assert!(md.contains("# 站会报告"), "{md}");
        assert!(md.contains("## 已完成(1)"), "{md}");
        assert!(md.contains("done thing"), "{md}");
    }

    #[test]
    fn standup_empty() {
        let report = standup_report(&[]);
        assert!(report.is_empty());
    }

    #[test]
    fn issue_serializes() {
        let i = issue("1", "t")
            .with_labels(vec!["a".into()])
            .with_blocked_by(vec!["2".into()]);
        let json = serde_json::to_string(&i).expect("serialize");
        let back: Issue = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, i);
    }
}
