//! # 长程自治(F35)
//!
//! 「丢一个目标自己跑到完成」的 Ralph 循环 + 多目标账本 + 卡死看门狗。
//!
//! ## 四块
//!
//! 1. **Ralph 循环**([`RalphLoop`])—— 目标 → 迭代 → 直到完成/达上限。
//!    每次迭代带**预算**与**停滞检测**,不会无限空转。
//! 2. **多目标账本**([`GoalLedger`])—— 多个目标并行推进,各自状态独立。
//! 3. **舰队看门狗**([`FleetWatchdog`])—— 长跑时健康度可被发现
//!    (心跳 + 停滞 + 预算三重)。
//! 4. **ship gate**([`ShipGate`])—— 完成前检查清单(测试过了吗?没留垃圾?),
//!    不过关不宣布完成。
//!
//! ## 为什么需要「停滞检测」而不是只看「跑完没」
//!
//! 长程自治最典型的失败模式不是崩溃,是**卡在原地反复做无用功**。
//! 只看"完成/未完成"永远发现不了。停滞检测看的是「最近 N 次迭代有没有
//! 产出变化」—— 与 F10 循环守卫同源思路,但作用在目标级。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 目标状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RalphStatus {
    /// 待开始。
    Pending,
    /// 进行中。
    Active,
    /// 已完成。
    Done,
    /// 停滞(多次迭代无进展)。
    Stalled,
    /// 预算耗尽。
    BudgetExhausted,
    /// 被取消。
    Cancelled,
}

impl RalphStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RalphStatus::Pending => "pending",
            RalphStatus::Active => "active",
            RalphStatus::Done => "done",
            RalphStatus::Stalled => "stalled",
            RalphStatus::BudgetExhausted => "budget_exhausted",
            RalphStatus::Cancelled => "cancelled",
        }
    }

    /// 是否终态。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RalphStatus::Done
                | RalphStatus::Stalled
                | RalphStatus::BudgetExhausted
                | RalphStatus::Cancelled
        )
    }
}

/// 一次迭代的记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Iteration {
    pub number: u32,
    /// 这次迭代的产出指纹(相同 = 无进展)。
    pub output_fingerprint: String,
    /// 是否达成了目标。
    pub achieved: bool,
    /// 消耗 token。
    #[serde(default)]
    pub tokens_used: u64,
}

/// Ralph 循环配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RalphConfig {
    /// 最大迭代次数。
    pub max_iterations: u32,
    /// 连续多少次迭代产出相同即判停滞。
    pub stall_threshold: u32,
    /// token 总预算。
    pub token_budget: u64,
}

impl Default for RalphConfig {
    fn default() -> Self {
        RalphConfig {
            max_iterations: 50,
            stall_threshold: 3,
            token_budget: 1_000_000,
        }
    }
}

/// Ralph 循环状态。
#[derive(Debug)]
pub struct RalphLoop {
    goal: String,
    config: RalphConfig,
    status: RalphStatus,
    iterations: Vec<Iteration>,
    tokens_used: u64,
}

impl RalphLoop {
    pub fn new(goal: impl Into<String>, config: RalphConfig) -> RalphLoop {
        RalphLoop {
            goal: goal.into(),
            config,
            status: RalphStatus::Pending,
            iterations: Vec::new(),
            tokens_used: 0,
        }
    }

    pub fn goal(&self) -> &str {
        &self.goal
    }

    pub fn status(&self) -> RalphStatus {
        self.status
    }

    pub fn iterations(&self) -> &[Iteration] {
        &self.iterations
    }

    pub fn tokens_used(&self) -> u64 {
        self.tokens_used
    }

    /// 开始(从 Pending → Active)。
    pub fn start(&mut self) -> bool {
        if self.status == RalphStatus::Pending {
            self.status = RalphStatus::Active;
            true
        } else {
            false
        }
    }

    /// 记录一次迭代,返回新的状态。
    ///
    /// 判定顺序:达成 → Done;预算耗尽 → BudgetExhausted;
    /// 迭代上限 → Stalled;连续同产出 → Stalled。
    pub fn record_iteration(
        &mut self,
        output_fingerprint: impl Into<String>,
        achieved: bool,
        tokens_used: u64,
    ) -> RalphStatus {
        if self.status != RalphStatus::Active {
            return self.status;
        }
        let number = self.iterations.len() as u32 + 1;
        self.iterations.push(Iteration {
            number,
            output_fingerprint: output_fingerprint.into(),
            achieved,
            tokens_used,
        });
        self.tokens_used = self.tokens_used.saturating_add(tokens_used);

        if achieved {
            self.status = RalphStatus::Done;
            return self.status;
        }
        if self.tokens_used >= self.config.token_budget {
            self.status = RalphStatus::BudgetExhausted;
            return self.status;
        }
        if number >= self.config.max_iterations {
            self.status = RalphStatus::Stalled;
            return self.status;
        }
        if self.consecutive_same_output() >= self.config.stall_threshold {
            self.status = RalphStatus::Stalled;
        }
        self.status
    }

    /// 末尾连续相同产出指纹的迭代数。
    fn consecutive_same_output(&self) -> u32 {
        let Some(last) = self.iterations.last() else {
            return 0;
        };
        self.iterations
            .iter()
            .rev()
            .take_while(|i| i.output_fingerprint == last.output_fingerprint)
            .count() as u32
    }

    /// 取消。
    pub fn cancel(&mut self) {
        if !self.status.is_terminal() {
            self.status = RalphStatus::Cancelled;
        }
    }

    /// 是否应该继续迭代。
    pub fn should_continue(&self) -> bool {
        self.status == RalphStatus::Active
    }

    /// 剩余预算。
    pub fn remaining_tokens(&self) -> u64 {
        self.config.token_budget.saturating_sub(self.tokens_used)
    }
}

/// 多目标账本。
#[derive(Debug, Default)]
pub struct GoalLedger {
    goals: BTreeMap<String, RalphLoop>,
}

impl GoalLedger {
    pub fn new() -> GoalLedger {
        GoalLedger::default()
    }

    /// 添加目标(自动 start)。
    pub fn add(&mut self, goal: impl Into<String>, config: RalphConfig) {
        let mut loop_ = RalphLoop::new(goal, config);
        loop_.start();
        self.goals.insert(loop_.goal().to_string(), loop_);
    }

    pub fn get(&self, goal: &str) -> Option<&RalphLoop> {
        self.goals.get(goal)
    }

    pub fn get_mut(&mut self, goal: &str) -> Option<&mut RalphLoop> {
        self.goals.get_mut(goal)
    }

    /// 活跃目标。
    pub fn active(&self) -> Vec<&RalphLoop> {
        self.goals
            .values()
            .filter(|g| g.status() == RalphStatus::Active)
            .collect()
    }

    /// 按状态统计。
    pub fn counts(&self) -> BTreeMap<&'static str, usize> {
        let mut out = BTreeMap::new();
        for g in self.goals.values() {
            *out.entry(g.status().as_str()).or_insert(0) += 1;
        }
        out
    }

    pub fn len(&self) -> usize {
        self.goals.len()
    }

    pub fn is_empty(&self) -> bool {
        self.goals.is_empty()
    }

    /// 全部已完成。
    pub fn all_done(&self) -> bool {
        !self.goals.is_empty() && self.goals.values().all(|g| g.status() == RalphStatus::Done)
    }
}

/// 舰队看门狗健康报告。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetHealth {
    /// 活跃目标数。
    pub active: usize,
    /// 停滞目标。
    pub stalled: Vec<String>,
    /// 预算耗尽目标。
    pub budget_exhausted: Vec<String>,
    /// 总消耗 token。
    pub total_tokens: u64,
}

impl FleetHealth {
    /// 是否需要人工介入。
    pub fn needs_attention(&self) -> bool {
        !self.stalled.is_empty() || !self.budget_exhausted.is_empty()
    }

    /// 人读摘要。
    pub fn summary(&self) -> String {
        if !self.needs_attention() {
            return format!("fleet: {} active, healthy", self.active);
        }
        format!(
            "fleet: {} active · {} stalled · {} over budget",
            self.active,
            self.stalled.len(),
            self.budget_exhausted.len()
        )
    }
}

/// 舰队看门狗。
#[derive(Debug, Default)]
pub struct FleetWatchdog;

impl FleetWatchdog {
    pub fn new() -> FleetWatchdog {
        FleetWatchdog
    }

    /// 检查账本健康度。
    pub fn check(&self, ledger: &GoalLedger) -> FleetHealth {
        let mut health = FleetHealth {
            active: 0,
            stalled: Vec::new(),
            budget_exhausted: Vec::new(),
            total_tokens: 0,
        };
        for (name, goal) in &ledger.goals {
            health.total_tokens = health.total_tokens.saturating_add(goal.tokens_used());
            match goal.status() {
                RalphStatus::Active => health.active += 1,
                RalphStatus::Stalled => health.stalled.push(name.clone()),
                RalphStatus::BudgetExhausted => health.budget_exhausted.push(name.clone()),
                _ => {}
            }
        }
        health.stalled.sort();
        health.budget_exhausted.sort();
        health
    }
}

/// ship gate 检查项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShipCheck {
    pub name: String,
    pub passed: bool,
    /// 失败时的说明。
    #[serde(default)]
    pub detail: Option<String>,
}

/// ship gate。
#[derive(Debug, Default)]
pub struct ShipGate {
    checks: Vec<ShipCheck>,
}

impl ShipGate {
    pub fn new() -> ShipGate {
        ShipGate::default()
    }

    /// 加一项检查。
    pub fn check(
        mut self,
        name: impl Into<String>,
        passed: bool,
        detail: Option<String>,
    ) -> ShipGate {
        self.checks.push(ShipCheck {
            name: name.into(),
            passed,
            detail,
        });
        self
    }

    /// 是否全部通过。
    pub fn all_passed(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(|c| c.passed)
    }

    /// 失败项。
    pub fn failures(&self) -> Vec<&ShipCheck> {
        self.checks.iter().filter(|c| !c.passed).collect()
    }

    /// 人读报告。
    pub fn report(&self) -> String {
        if self.checks.is_empty() {
            return "ship gate: no checks".to_string();
        }
        if self.all_passed() {
            return format!("ship gate: {} checks passed", self.checks.len());
        }
        let failed: Vec<&str> = self.failures().iter().map(|c| c.name.as_str()).collect();
        format!("ship gate: BLOCKED by {}", failed.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loop_with(config: RalphConfig) -> RalphLoop {
        let mut l = RalphLoop::new("fix the bug", config);
        l.start();
        l
    }

    #[test]
    fn start_transitions_pending_to_active() {
        let mut l = RalphLoop::new("g", RalphConfig::default());
        assert_eq!(l.status(), RalphStatus::Pending);
        assert!(l.start());
        assert_eq!(l.status(), RalphStatus::Active);
        assert!(!l.start(), "重复 start 无效");
    }

    #[test]
    fn achieved_marks_done() {
        let mut l = loop_with(RalphConfig::default());
        let s = l.record_iteration("fp1", true, 100);
        assert_eq!(s, RalphStatus::Done);
        assert!(!l.should_continue());
    }

    #[test]
    fn iteration_limit_marks_stalled() {
        let mut l = loop_with(RalphConfig {
            max_iterations: 2,
            stall_threshold: 100, // 关掉停滞检测,专测上限
            token_budget: u64::MAX,
        });
        assert_eq!(l.record_iteration("a", false, 1), RalphStatus::Active);
        assert_eq!(l.record_iteration("b", false, 1), RalphStatus::Stalled);
    }

    #[test]
    fn stall_detection_on_repeated_output() {
        let mut l = loop_with(RalphConfig {
            max_iterations: 100,
            stall_threshold: 3,
            token_budget: u64::MAX,
        });
        assert_eq!(l.record_iteration("same", false, 1), RalphStatus::Active);
        assert_eq!(l.record_iteration("same", false, 1), RalphStatus::Active);
        assert_eq!(
            l.record_iteration("same", false, 1),
            RalphStatus::Stalled,
            "连续 3 次相同产出应判停滞"
        );
    }

    #[test]
    fn changing_output_does_not_stall() {
        let mut l = loop_with(RalphConfig {
            max_iterations: 100,
            stall_threshold: 3,
            token_budget: u64::MAX,
        });
        for i in 0..10 {
            assert_eq!(
                l.record_iteration(format!("fp{i}"), false, 1),
                RalphStatus::Active,
                "产出在变,不算停滞"
            );
        }
    }

    #[test]
    fn budget_exhaustion() {
        let mut l = loop_with(RalphConfig {
            max_iterations: 100,
            stall_threshold: 100,
            token_budget: 100,
        });
        l.record_iteration("a", false, 60);
        let s = l.record_iteration("b", false, 60);
        assert_eq!(s, RalphStatus::BudgetExhausted);
        assert_eq!(l.remaining_tokens(), 0);
    }

    #[test]
    fn achieved_wins_over_budget() {
        let mut l = loop_with(RalphConfig {
            max_iterations: 100,
            stall_threshold: 100,
            token_budget: 10,
        });
        // 达成优先于预算(先判 achieved)。
        let s = l.record_iteration("a", true, 1000);
        assert_eq!(s, RalphStatus::Done);
    }

    #[test]
    fn record_ignored_when_not_active() {
        let mut l = RalphLoop::new("g", RalphConfig::default());
        // 未 start
        assert_eq!(l.record_iteration("a", false, 1), RalphStatus::Pending);
        assert!(l.iterations().is_empty(), "非 active 不记录迭代");
    }

    #[test]
    fn cancel_sets_terminal() {
        let mut l = loop_with(RalphConfig::default());
        l.cancel();
        assert_eq!(l.status(), RalphStatus::Cancelled);
        assert!(l.status().is_terminal());
    }

    #[test]
    fn cancel_does_not_override_done() {
        let mut l = loop_with(RalphConfig::default());
        l.record_iteration("a", true, 1);
        l.cancel();
        assert_eq!(l.status(), RalphStatus::Done, "已完成不能被取消覆盖");
    }

    #[test]
    fn ledger_manages_multiple_goals() {
        let mut ledger = GoalLedger::new();
        ledger.add("goal-a", RalphConfig::default());
        ledger.add("goal-b", RalphConfig::default());
        assert_eq!(ledger.len(), 2);
        assert_eq!(ledger.active().len(), 2);
        assert!(!ledger.all_done());
    }

    #[test]
    fn ledger_all_done() {
        let mut ledger = GoalLedger::new();
        ledger.add("g", RalphConfig::default());
        ledger
            .get_mut("g")
            .unwrap()
            .record_iteration("fp", true, 10);
        assert!(ledger.all_done());
    }

    #[test]
    fn ledger_counts() {
        let mut ledger = GoalLedger::new();
        ledger.add("a", RalphConfig::default());
        ledger.add("b", RalphConfig::default());
        ledger.get_mut("b").unwrap().cancel();
        let counts = ledger.counts();
        assert_eq!(counts["active"], 1);
        assert_eq!(counts["cancelled"], 1);
    }

    #[test]
    fn watchdog_reports_health() {
        let mut ledger = GoalLedger::new();
        ledger.add("healthy", RalphConfig::default());
        ledger.add(
            "stalling",
            RalphConfig {
                max_iterations: 100,
                stall_threshold: 2,
                token_budget: u64::MAX,
            },
        );
        let stalling = ledger.get_mut("stalling").unwrap();
        stalling.record_iteration("same", false, 1);
        stalling.record_iteration("same", false, 1);

        let watchdog = FleetWatchdog::new();
        let health = watchdog.check(&ledger);
        assert_eq!(health.active, 1);
        assert_eq!(health.stalled, vec!["stalling"]);
        assert!(health.needs_attention());
        assert!(
            health.summary().contains("1 stalled"),
            "{}",
            health.summary()
        );
    }

    #[test]
    fn watchdog_healthy_fleet() {
        let mut ledger = GoalLedger::new();
        ledger.add("g", RalphConfig::default());
        let health = FleetWatchdog::new().check(&ledger);
        assert!(!health.needs_attention());
        assert!(health.summary().contains("healthy"), "{}", health.summary());
    }

    #[test]
    fn ship_gate_all_passed() {
        let gate = ShipGate::new()
            .check("tests", true, None)
            .check("no-temp-files", true, None);
        assert!(gate.all_passed());
        assert!(gate.report().contains("2 checks passed"));
    }

    #[test]
    fn ship_gate_blocked() {
        let gate = ShipGate::new()
            .check("tests", false, Some("3 failures".to_string()))
            .check("lint", true, None);
        assert!(!gate.all_passed());
        assert_eq!(gate.failures().len(), 1);
        assert!(
            gate.report().contains("BLOCKED by tests"),
            "{}",
            gate.report()
        );
    }

    #[test]
    fn ship_gate_empty_not_passed() {
        // 空 gate 不应视为"通过"(没检查 ≠ 没问题)。
        let gate = ShipGate::new();
        assert!(!gate.all_passed());
        assert!(gate.report().contains("no checks"));
    }

    #[test]
    fn iteration_serializes() {
        let it = Iteration {
            number: 1,
            output_fingerprint: "fp".into(),
            achieved: false,
            tokens_used: 100,
        };
        let json = serde_json::to_string(&it).expect("serialize");
        let back: Iteration = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, it);
    }
}
