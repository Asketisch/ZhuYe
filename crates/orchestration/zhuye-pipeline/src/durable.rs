//! # Durable execution:run 级 journal 与断点续跑(F33)
//!
//! 跑一半崩溃/重启的任务从断点继续,不用从头再来。
//!
//! ## 核心:journal(先写后做)
//!
//! 每一步执行**前**先记「我要做什么」,执行**后**记「做完了/结果是什么」。
//! 崩溃恢复时读 journal:
//!
//! - 有 `started` 无 `completed` → 该步状态**不确定**,按策略重跑或标记失败;
//! - 有 `completed` → 跳过(结果已在 journal 里)。
//!
//! 这个「先写后做」(write-ahead)顺序是关键:反过来(先做后写)在崩溃时
//! 会丢失"已经做了"的记录,恢复后会**重复执行有副作用的操作**。
//!
//! ## 为什么「不确定」不能当成成功
//!
//! 崩溃发生在「工具已执行、结果未落盘」的窗口。此时若假定成功,恢复后
//! 会跳过该步 —— 但副作用可能只做了一半。所以 [`RecoveryAction`] 对
//! 不确定步骤默认是 `Replay`(重跑),由幂等性保证安全;对非幂等步骤
//! 则要求显式标记 `Uncertain` 让人看。
//!
//! ## 步级 checkpoint
//!
//! [`Checkpoint`] 记录「审批态 + 已完成步 + 上下文摘要」,恢复时精确还原
//! 到崩溃前的状态 —— 包括**挂起的审批**(不能让用户重新批一遍)。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// journal 记录类型。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalRecord {
    /// 开始执行某步(**先写**)。
    StepStarted { step: String, at: u64 },
    /// 某步完成(**后写**)。
    StepCompleted {
        step: String,
        at: u64,
        /// 结果摘要(可序列化)。
        #[serde(default)]
        result: serde_json::Value,
    },
    /// 某步失败。
    StepFailed {
        step: String,
        at: u64,
        error: String,
    },
    /// 审批挂起(需要恢复时重新等待)。
    ApprovalPending {
        step: String,
        request_id: String,
        at: u64,
    },
    /// 审批已决。
    ApprovalResolved {
        step: String,
        request_id: String,
        approved: bool,
        at: u64,
    },
    /// 阶段摘要(便于跳过已完成阶段)。
    PhaseDigest {
        phase: String,
        digest: String,
        at: u64,
    },
}

impl JournalRecord {
    /// 涉及的步骤名(`PhaseDigest` 返回 None)。
    pub fn step(&self) -> Option<&str> {
        match self {
            JournalRecord::StepStarted { step, .. }
            | JournalRecord::StepCompleted { step, .. }
            | JournalRecord::StepFailed { step, .. }
            | JournalRecord::ApprovalPending { step, .. }
            | JournalRecord::ApprovalResolved { step, .. } => Some(step),
            JournalRecord::PhaseDigest { .. } => None,
        }
    }

    /// 时间戳。
    pub fn at(&self) -> u64 {
        match self {
            JournalRecord::StepStarted { at, .. }
            | JournalRecord::StepCompleted { at, .. }
            | JournalRecord::StepFailed { at, .. }
            | JournalRecord::ApprovalPending { at, .. }
            | JournalRecord::ApprovalResolved { at, .. }
            | JournalRecord::PhaseDigest { at, .. } => *at,
        }
    }
}

/// 单步的恢复判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAction {
    /// 已完成 → 跳过。
    Skip,
    /// 未开始 → 正常执行。
    Run,
    /// 开始但未完成 → 重跑(需幂等)。
    Replay,
    /// 开始未完成且非幂等 → 标为不确定,交人工。
    Uncertain,
}

/// 恢复计划。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryPlan {
    /// `step -> action`。
    pub actions: BTreeMap<String, RecoveryAction>,
    /// 挂起中的审批(恢复后重新等待)。
    pub pending_approvals: Vec<(String, String)>,
}

impl RecoveryPlan {
    /// 需要执行的步骤(按 `Run` + `Replay`)。
    pub fn steps_to_run(&self) -> Vec<&str> {
        self.actions
            .iter()
            .filter(|(_, a)| matches!(a, RecoveryAction::Run | RecoveryAction::Replay))
            .map(|(k, _)| k.as_str())
            .collect()
    }

    /// 是否有不确定步骤(需要人工介入)。
    pub fn has_uncertain(&self) -> bool {
        self.actions
            .values()
            .any(|a| *a == RecoveryAction::Uncertain)
    }

    /// 不确定步骤列表。
    pub fn uncertain_steps(&self) -> Vec<&str> {
        self.actions
            .iter()
            .filter(|(_, a)| **a == RecoveryAction::Uncertain)
            .map(|(k, _)| k.as_str())
            .collect()
    }
}

/// run 级 journal。
#[derive(Debug, Default)]
pub struct RunJournal {
    records: Vec<JournalRecord>,
}

impl RunJournal {
    pub fn new() -> RunJournal {
        RunJournal::default()
    }

    /// 追加记录。
    pub fn append(&mut self, record: JournalRecord) {
        self.records.push(record);
    }

    /// 全部记录。
    pub fn records(&self) -> &[JournalRecord] {
        &self.records
    }

    /// 记录数。
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// 已完成步骤 → 结果。
    pub fn completed_steps(&self) -> BTreeMap<&str, &serde_json::Value> {
        let mut out = BTreeMap::new();
        for r in &self.records {
            if let JournalRecord::StepCompleted { step, result, .. } = r {
                out.insert(step.as_str(), result);
            }
        }
        out
    }

    /// 失败步骤 → 错误。
    pub fn failed_steps(&self) -> BTreeMap<&str, &str> {
        let mut out = BTreeMap::new();
        for r in &self.records {
            if let JournalRecord::StepFailed { step, error, .. } = r {
                out.insert(step.as_str(), error.as_str());
            }
        }
        out
    }

    /// 已开始但未完成/未失败的步骤(崩溃窗口)。
    pub fn in_flight_steps(&self) -> Vec<&str> {
        let mut started = Vec::new();
        let mut settled = std::collections::HashSet::new();
        for r in &self.records {
            match r {
                JournalRecord::StepStarted { step, .. } => started.push(step.as_str()),
                JournalRecord::StepCompleted { step, .. }
                | JournalRecord::StepFailed { step, .. } => {
                    settled.insert(step.as_str());
                }
                _ => {}
            }
        }
        started
            .into_iter()
            .filter(|s| !settled.contains(s))
            .collect()
    }

    /// 挂起中的审批(`ApprovalPending` 无对应 `ApprovalResolved`)。
    pub fn pending_approvals(&self) -> Vec<(&str, &str)> {
        let mut pending: Vec<(&str, &str)> = Vec::new();
        for r in &self.records {
            match r {
                JournalRecord::ApprovalPending {
                    step, request_id, ..
                } => pending.push((step.as_str(), request_id.as_str())),
                JournalRecord::ApprovalResolved { request_id, .. } => {
                    pending.retain(|(_, id)| id != request_id);
                }
                _ => {}
            }
        }
        pending
    }

    /// 生成恢复计划。
    ///
    /// `idempotent` 声明哪些步骤可安全重跑。
    pub fn recovery_plan(&self, all_steps: &[String], idempotent: &[String]) -> RecoveryPlan {
        let completed = self.completed_steps();
        let in_flight: std::collections::HashSet<&str> =
            self.in_flight_steps().into_iter().collect();
        let mut actions = BTreeMap::new();
        for step in all_steps {
            let action = if completed.contains_key(step.as_str()) {
                RecoveryAction::Skip
            } else if in_flight.contains(step.as_str()) {
                if idempotent.iter().any(|i| i == step) {
                    RecoveryAction::Replay
                } else {
                    RecoveryAction::Uncertain
                }
            } else {
                RecoveryAction::Run
            };
            actions.insert(step.clone(), action);
        }
        RecoveryPlan {
            actions,
            pending_approvals: self
                .pending_approvals()
                .into_iter()
                .map(|(s, r)| (s.to_string(), r.to_string()))
                .collect(),
        }
    }

    /// 阶段摘要是否已存在且一致(用于跳过已完成阶段)。
    pub fn phase_digest(&self, phase: &str) -> Option<&str> {
        self.records.iter().rev().find_map(|r| match r {
            JournalRecord::PhaseDigest {
                phase: p, digest, ..
            } if p == phase => Some(digest.as_str()),
            _ => None,
        })
    }

    /// 序列化成 JSONL(可逐行追加落盘)。
    pub fn to_jsonl(&self) -> String {
        self.records
            .iter()
            .filter_map(|r| serde_json::to_string(r).ok())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 从 JSONL 恢复。
    pub fn from_jsonl(s: &str) -> RunJournal {
        let mut journal = RunJournal::new();
        for line in s.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(record) = serde_json::from_str::<JournalRecord>(line) {
                journal.append(record);
            }
        }
        journal
    }
}

/// 步级 checkpoint(含审批态)。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// 已完成步骤。
    pub completed: Vec<String>,
    /// 挂起审批(`step -> request_id`)。
    pub pending_approvals: BTreeMap<String, String>,
    /// 上下文摘要(可任意 JSON)。
    #[serde(default)]
    pub context: serde_json::Value,
    /// 检查点时间戳。
    pub at: u64,
}

impl Checkpoint {
    /// 从 journal 构造检查点。
    pub fn from_journal(journal: &RunJournal, at: u64) -> Checkpoint {
        Checkpoint {
            completed: journal
                .completed_steps()
                .keys()
                .map(|s| (*s).to_string())
                .collect(),
            pending_approvals: journal
                .pending_approvals()
                .into_iter()
                .map(|(s, r)| (s.to_string(), r.to_string()))
                .collect(),
            context: serde_json::Value::Null,
            at,
        }
    }

    /// 是否某步已完成。
    pub fn is_completed(&self, step: &str) -> bool {
        self.completed.iter().any(|s| s == step)
    }

    /// 是否有挂起审批。
    pub fn has_pending_approval(&self, step: &str) -> bool {
        self.pending_approvals.contains_key(step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(step: &str, at: u64) -> JournalRecord {
        JournalRecord::StepStarted {
            step: step.into(),
            at,
        }
    }

    fn completed(step: &str, at: u64) -> JournalRecord {
        JournalRecord::StepCompleted {
            step: step.into(),
            at,
            result: serde_json::json!({"ok": true}),
        }
    }

    #[test]
    fn completed_steps_tracked() {
        let mut j = RunJournal::new();
        j.append(started("a", 1));
        j.append(completed("a", 2));
        let done = j.completed_steps();
        assert!(done.contains_key("a"));
        assert_eq!(done.len(), 1);
    }

    #[test]
    fn in_flight_detected() {
        let mut j = RunJournal::new();
        j.append(started("a", 1));
        j.append(completed("a", 2));
        j.append(started("b", 3)); // 崩在这里
        assert_eq!(j.in_flight_steps(), vec!["b"]);
    }

    #[test]
    fn failed_step_not_in_flight() {
        let mut j = RunJournal::new();
        j.append(started("a", 1));
        j.append(JournalRecord::StepFailed {
            step: "a".into(),
            at: 2,
            error: "boom".into(),
        });
        assert!(j.in_flight_steps().is_empty(), "失败已结算");
        assert_eq!(j.failed_steps().get("a"), Some(&"boom"));
    }

    #[test]
    fn recovery_plan_all_actions() {
        let mut j = RunJournal::new();
        j.append(started("done", 1));
        j.append(completed("done", 2));
        j.append(started("idem", 3)); // 崩溃窗口 + 幂等
        j.append(started("nonidem", 4)); // 崩溃窗口 + 非幂等
        // "fresh" 从未开始
        let steps: Vec<String> = ["done", "idem", "nonidem", "fresh"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let plan = j.recovery_plan(&steps, &["idem".to_string()]);
        assert_eq!(plan.actions["done"], RecoveryAction::Skip);
        assert_eq!(plan.actions["idem"], RecoveryAction::Replay);
        assert_eq!(plan.actions["nonidem"], RecoveryAction::Uncertain);
        assert_eq!(plan.actions["fresh"], RecoveryAction::Run);
    }

    #[test]
    fn plan_steps_to_run() {
        let mut j = RunJournal::new();
        j.append(started("a", 1));
        j.append(completed("a", 2));
        j.append(started("b", 3));
        let steps = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let plan = j.recovery_plan(&steps, &["b".to_string()]);
        let to_run = plan.steps_to_run();
        assert!(to_run.contains(&"b"));
        assert!(to_run.contains(&"c"));
        assert!(!to_run.contains(&"a"));
    }

    #[test]
    fn uncertain_detection() {
        let mut j = RunJournal::new();
        j.append(started("x", 1));
        let steps = vec!["x".to_string()];
        let plan = j.recovery_plan(&steps, &[]); // 非幂等
        assert!(plan.has_uncertain());
        assert_eq!(plan.uncertain_steps(), vec!["x"]);
    }

    #[test]
    fn pending_approvals_tracked() {
        let mut j = RunJournal::new();
        j.append(JournalRecord::ApprovalPending {
            step: "deploy".into(),
            request_id: "req-1".into(),
            at: 1,
        });
        assert_eq!(j.pending_approvals(), vec![("deploy", "req-1")]);
        j.append(JournalRecord::ApprovalResolved {
            step: "deploy".into(),
            request_id: "req-1".into(),
            approved: true,
            at: 2,
        });
        assert!(j.pending_approvals().is_empty());
    }

    #[test]
    fn pending_approval_survives_crash() {
        // 崩溃时审批仍挂起 → 恢复计划应带上它。
        let mut j = RunJournal::new();
        j.append(JournalRecord::ApprovalPending {
            step: "deploy".into(),
            request_id: "req-9".into(),
            at: 1,
        });
        let plan = j.recovery_plan(&["deploy".to_string()], &[]);
        assert_eq!(plan.pending_approvals.len(), 1);
        assert_eq!(plan.pending_approvals[0].1, "req-9");
    }

    #[test]
    fn jsonl_roundtrip() {
        let mut j = RunJournal::new();
        j.append(started("a", 1));
        j.append(completed("a", 2));
        j.append(JournalRecord::PhaseDigest {
            phase: "build".into(),
            digest: "abc".into(),
            at: 3,
        });
        let jsonl = j.to_jsonl();
        assert_eq!(jsonl.lines().count(), 3);
        let back = RunJournal::from_jsonl(&jsonl);
        assert_eq!(back.len(), 3);
        assert_eq!(back.phase_digest("build"), Some("abc"));
    }

    #[test]
    fn jsonl_skips_corrupt_lines() {
        let jsonl = "{\"kind\":\"step_started\",\"step\":\"a\",\"at\":1}\nnot json\n";
        let journal = RunJournal::from_jsonl(jsonl);
        assert_eq!(journal.len(), 1, "坏行应跳过,不整份丢弃");
    }

    #[test]
    fn phase_digest_latest_wins() {
        let mut j = RunJournal::new();
        j.append(JournalRecord::PhaseDigest {
            phase: "p".into(),
            digest: "old".into(),
            at: 1,
        });
        j.append(JournalRecord::PhaseDigest {
            phase: "p".into(),
            digest: "new".into(),
            at: 2,
        });
        assert_eq!(j.phase_digest("p"), Some("new"));
        assert_eq!(j.phase_digest("missing"), None);
    }

    #[test]
    fn checkpoint_from_journal() {
        let mut j = RunJournal::new();
        j.append(started("a", 1));
        j.append(completed("a", 2));
        j.append(JournalRecord::ApprovalPending {
            step: "b".into(),
            request_id: "r".into(),
            at: 3,
        });
        let cp = Checkpoint::from_journal(&j, 10);
        assert!(cp.is_completed("a"));
        assert!(!cp.is_completed("b"));
        assert!(cp.has_pending_approval("b"));
        assert_eq!(cp.at, 10);
    }

    #[test]
    fn checkpoint_serializes() {
        let cp = Checkpoint {
            completed: vec!["a".into()],
            pending_approvals: BTreeMap::new(),
            context: serde_json::json!({"k": "v"}),
            at: 5,
        };
        let json = serde_json::to_string(&cp).expect("serialize");
        let back: Checkpoint = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, cp);
    }

    #[test]
    fn record_step_and_at() {
        let r = started("x", 7);
        assert_eq!(r.step(), Some("x"));
        assert_eq!(r.at(), 7);
        let digest = JournalRecord::PhaseDigest {
            phase: "p".into(),
            digest: "d".into(),
            at: 9,
        };
        assert_eq!(digest.step(), None);
        assert_eq!(digest.at(), 9);
    }

    #[test]
    fn empty_journal() {
        let j = RunJournal::new();
        assert!(j.is_empty());
        let plan = j.recovery_plan(&["a".to_string()], &[]);
        assert_eq!(plan.actions["a"], RecoveryAction::Run);
    }
}
