//! # 缓存与成本护栏(F44 / F55)
//!
//! ## F44:USD 预算账本
//!
//! 现有 token 预算只数 token(抽象计数),用户不知道「这个月花了多少钱」。
//! [`SpendLedger`] 按美元记账:
//!
//! - **多级预算**(session / turn / monthly),任一超限即停;
//! - **reservation 语义**:发请求前先预留,完成后按实际结算 —— 防止
//!   并发请求同时通过预算检查后一起超支;
//! - **80% 告警**:到达 80% 时给一次提醒(不要等到爆了才说)。
//!
//! ## F55:按步骤 / 子 agent / 文件归因
//!
//! [`CostAttribution`] 记录每笔花费的**归属**(哪一步 / 哪个子 agent /
//! 哪个文件),让「钱花哪了」可回答。
//!
//! ## 为什么 reservation 不是可选
//!
//! 两个并发请求各自看到「还剩 $1」,都发出去各花 $0.8 → 超支 $0.6。
//! 预留机制让第一个请求先占住额度,第二个看到「只剩 $0.2」就拒绝。
//! 这是预算护栏的**正确性**前提,不是优化。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 预算档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetScope {
    /// 单次请求。
    Turn,
    /// 整个会话。
    Session,
    /// 自然月。
    Monthly,
}

impl BudgetScope {
    pub fn as_str(self) -> &'static str {
        match self {
            BudgetScope::Turn => "turn",
            BudgetScope::Session => "session",
            BudgetScope::Monthly => "monthly",
        }
    }
}

/// 超限行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverBudgetAction {
    /// 拒绝新请求(默认,最安全)。
    #[default]
    Pause,
    /// 回落到更便宜的模型(调用方实现)。
    FallbackModel,
    /// 仅告警不阻断。
    Warn,
}

/// 一次花费记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpendEntry {
    /// 归属标签(`step:3` / `subagent:reviewer` / `file:src/lib.rs`)。
    pub attribution: String,
    /// 美元金额。
    pub usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 是否命中缓存(命中部分通常更便宜)。
    #[serde(default)]
    pub cached: bool,
}

/// 预算检查结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetCheck {
    pub allowed: bool,
    /// 是否触及 80% 告警阈值(应提示用户)。
    pub warn: bool,
    /// 说明。
    pub reason: String,
    /// 触发的档位(`None` = 未超)。
    pub breached_scope: Option<BudgetScope>,
}

impl BudgetCheck {
    fn ok() -> BudgetCheck {
        BudgetCheck {
            allowed: true,
            warn: false,
            reason: String::new(),
            breached_scope: None,
        }
    }
}

/// 花费账本。
#[derive(Debug)]
pub struct SpendLedger {
    /// 各档位上限(USD)。`None` = 不限。
    limits: BTreeMap<BudgetScope, Option<f64>>,
    /// 已结算金额(按档位)。
    spent: BTreeMap<BudgetScope, f64>,
    /// 已预留但未结算金额(按档位)。
    reserved: BTreeMap<BudgetScope, f64>,
    /// 归因明细。
    entries: Vec<SpendEntry>,
    /// 已发过 80% 告警的档位(每档只告警一次)。
    warned: BTreeMap<BudgetScope, bool>,
    /// 超限行为。
    action: OverBudgetAction,
}

impl Default for SpendLedger {
    fn default() -> Self {
        SpendLedger::new()
    }
}

impl SpendLedger {
    pub fn new() -> SpendLedger {
        SpendLedger {
            limits: BTreeMap::new(),
            spent: BTreeMap::new(),
            reserved: BTreeMap::new(),
            entries: Vec::new(),
            warned: BTreeMap::new(),
            action: OverBudgetAction::Pause,
        }
    }

    /// 设置某档位上限。
    pub fn set_limit(&mut self, scope: BudgetScope, usd: Option<f64>) {
        self.limits.insert(scope, usd);
    }

    /// 设置超限行为。
    pub fn set_action(&mut self, action: OverBudgetAction) {
        self.action = action;
    }

    pub fn action(&self) -> OverBudgetAction {
        self.action
    }

    /// 某档位已用(含预留)。
    pub fn used(&self, scope: BudgetScope) -> f64 {
        self.spent.get(&scope).copied().unwrap_or(0.0)
            + self.reserved.get(&scope).copied().unwrap_or(0.0)
    }

    /// 某档位剩余(`None` 上限 → `None`)。
    pub fn remaining(&self, scope: BudgetScope) -> Option<f64> {
        self.limits
            .get(&scope)
            .copied()
            .flatten()
            .map(|limit| (limit - self.used(scope)).max(0.0))
    }

    /// 检查能否花 `estimate_usd`。
    ///
    /// 任一档位超限 → 拒绝(除非 action = Warn)。
    pub fn check(&self, estimate_usd: f64) -> BudgetCheck {
        let mut check = BudgetCheck::ok();
        for scope in [
            BudgetScope::Turn,
            BudgetScope::Session,
            BudgetScope::Monthly,
        ] {
            let Some(Some(limit)) = self.limits.get(&scope) else {
                continue;
            };
            let used = self.used(scope);
            let projected = used + estimate_usd;
            if projected > *limit {
                if self.action == OverBudgetAction::Warn {
                    check.warn = true;
                    check.reason = format!(
                        "{} budget exceeded (${:.4} > ${:.2}), warn-only",
                        scope.as_str(),
                        projected,
                        limit
                    );
                    continue;
                }
                return BudgetCheck {
                    allowed: false,
                    warn: true,
                    reason: format!(
                        "{} budget exceeded: projected ${:.4} > limit ${:.2}",
                        scope.as_str(),
                        projected,
                        limit
                    ),
                    breached_scope: Some(scope),
                };
            }
            // 80% 告警
            if projected >= limit * 0.8 && !self.warned.get(&scope).copied().unwrap_or(false) {
                check.warn = true;
                check.reason = format!(
                    "{} budget at {:.0}% (${:.4}/${:.2})",
                    scope.as_str(),
                    projected / limit * 100.0,
                    projected,
                    limit
                );
            }
        }
        check
    }

    /// 预留额度。成功返回 `true`(调用方必须随后 `settle` 或 `release`)。
    pub fn reserve(&mut self, estimate_usd: f64) -> bool {
        let check = self.check(estimate_usd);
        if !check.allowed {
            return false;
        }
        if check.warn {
            for scope in [
                BudgetScope::Turn,
                BudgetScope::Session,
                BudgetScope::Monthly,
            ] {
                if let Some(Some(limit)) = self.limits.get(&scope)
                    && self.used(scope) + estimate_usd >= limit * 0.8
                {
                    self.warned.insert(scope, true);
                }
            }
        }
        for scope in [
            BudgetScope::Turn,
            BudgetScope::Session,
            BudgetScope::Monthly,
        ] {
            *self.reserved.entry(scope).or_insert(0.0) += estimate_usd;
        }
        true
    }

    /// 结算:释放预留,记实际花费 + 归因。
    ///
    /// `actual_usd` 可与预留不同(实际通常更少)。
    pub fn settle(&mut self, reserved_usd: f64, entry: SpendEntry) {
        for scope in [
            BudgetScope::Turn,
            BudgetScope::Session,
            BudgetScope::Monthly,
        ] {
            let r = self.reserved.entry(scope).or_insert(0.0);
            *r = (*r - reserved_usd).max(0.0);
            *self.spent.entry(scope).or_insert(0.0) += entry.usd;
        }
        self.entries.push(entry);
    }

    /// 释放预留(请求失败 / 取消)。
    pub fn release(&mut self, reserved_usd: f64) {
        for scope in [
            BudgetScope::Turn,
            BudgetScope::Session,
            BudgetScope::Monthly,
        ] {
            let r = self.reserved.entry(scope).or_insert(0.0);
            *r = (*r - reserved_usd).max(0.0);
        }
    }

    /// 重置回合级计数(新 turn 开始时)。
    pub fn reset_turn(&mut self) {
        self.spent.insert(BudgetScope::Turn, 0.0);
        self.reserved.insert(BudgetScope::Turn, 0.0);
        self.warned.remove(&BudgetScope::Turn);
    }

    /// 总花费。
    pub fn total_spent(&self) -> f64 {
        self.spent
            .get(&BudgetScope::Session)
            .copied()
            .unwrap_or(0.0)
    }

    /// 花费明细。
    pub fn entries(&self) -> &[SpendEntry] {
        &self.entries
    }

    /// 按归因聚合(`attribution -> usd`),降序。
    pub fn by_attribution(&self) -> Vec<(String, f64)> {
        let mut map: BTreeMap<String, f64> = BTreeMap::new();
        for e in &self.entries {
            *map.entry(e.attribution.clone()).or_insert(0.0) += e.usd;
        }
        let mut out: Vec<(String, f64)> = map.into_iter().collect();
        out.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        out
    }

    /// 按归因前缀聚合(`step:*` / `subagent:*` / `file:*`)。
    pub fn by_prefix(&self) -> BTreeMap<String, f64> {
        let mut out: BTreeMap<String, f64> = BTreeMap::new();
        for e in &self.entries {
            let prefix = e
                .attribution
                .split(':')
                .next()
                .unwrap_or(&e.attribution)
                .to_string();
            *out.entry(prefix).or_insert(0.0) += e.usd;
        }
        out
    }

    /// 缓存命中节省估算(命中部分按 `discount` 比例折算)。
    pub fn cache_savings(&self, discount: f64) -> f64 {
        self.entries
            .iter()
            .filter(|e| e.cached)
            .map(|e| e.usd * discount)
            .sum()
    }
}

/// 成本归因报告(F55)。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CostAttribution {
    /// `attribution -> usd`(降序)。
    pub rows: Vec<(String, f64)>,
    pub total_usd: f64,
    /// 按前缀汇总。
    pub by_prefix: BTreeMap<String, f64>,
}

impl CostAttribution {
    /// 从账本生成报告。
    pub fn from_ledger(ledger: &SpendLedger) -> CostAttribution {
        CostAttribution {
            rows: ledger.by_attribution(),
            total_usd: ledger.total_spent(),
            by_prefix: ledger.by_prefix(),
        }
    }

    /// 某归因占比(0.0–1.0)。
    pub fn share_of(&self, attribution: &str) -> f64 {
        if self.total_usd <= 0.0 {
            return 0.0;
        }
        self.rows
            .iter()
            .find(|(k, _)| k == attribution)
            .map_or(0.0, |(_, v)| v / self.total_usd)
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(attr: &str, usd: f64, cached: bool) -> SpendEntry {
        SpendEntry {
            attribution: attr.to_string(),
            usd,
            input_tokens: 100,
            output_tokens: 50,
            cached,
        }
    }

    #[test]
    fn no_limit_allows_everything() {
        let ledger = SpendLedger::new();
        let check = ledger.check(1000.0);
        assert!(check.allowed);
        assert!(!check.warn);
    }

    #[test]
    fn session_limit_blocks() {
        let mut ledger = SpendLedger::new();
        ledger.set_limit(BudgetScope::Session, Some(1.0));
        ledger.settle(0.0, entry("step:1", 0.9, false));
        let check = ledger.check(0.2);
        assert!(!check.allowed, "0.9 + 0.2 > 1.0");
        assert_eq!(check.breached_scope, Some(BudgetScope::Session));
        assert!(check.reason.contains("session budget exceeded"));
    }

    #[test]
    fn turn_limit_blocks() {
        let mut ledger = SpendLedger::new();
        ledger.set_limit(BudgetScope::Turn, Some(0.5));
        ledger.settle(0.0, entry("step:1", 0.4, false));
        assert!(!ledger.check(0.2).allowed);
    }

    #[test]
    fn monthly_limit_blocks() {
        let mut ledger = SpendLedger::new();
        ledger.set_limit(BudgetScope::Monthly, Some(10.0));
        ledger.settle(0.0, entry("step:1", 9.5, false));
        let check = ledger.check(1.0);
        assert!(!check.allowed);
        assert_eq!(check.breached_scope, Some(BudgetScope::Monthly));
    }

    #[test]
    fn warn_at_80_percent() {
        let mut ledger = SpendLedger::new();
        ledger.set_limit(BudgetScope::Session, Some(1.0));
        ledger.settle(0.0, entry("step:1", 0.7, false));
        let check = ledger.check(0.15); // 0.85 ≥ 80%
        assert!(check.allowed, "未超限仍允许");
        assert!(check.warn);
        assert!(check.reason.contains("85%"), "{}", check.reason);
    }

    #[test]
    fn warn_only_fires_once() {
        let mut ledger = SpendLedger::new();
        ledger.set_limit(BudgetScope::Session, Some(1.0));
        assert!(ledger.reserve(0.85), "预留成功");
        // 第二次检查不应再告警(warned 已置位)。
        let check = ledger.check(0.01);
        assert!(!check.warn, "80% 告警每档只发一次");
    }

    #[test]
    fn reservation_prevents_concurrent_overspend() {
        let mut ledger = SpendLedger::new();
        ledger.set_limit(BudgetScope::Session, Some(1.0));
        // 第一个请求预留 0.8 → 成功
        assert!(ledger.reserve(0.8));
        // 第二个请求想预留 0.8 → 应失败(0.8 已占)
        assert!(!ledger.reserve(0.8), "预留应防并发超支");
    }

    #[test]
    fn settle_releases_reservation_and_records_spend() {
        let mut ledger = SpendLedger::new();
        ledger.set_limit(BudgetScope::Session, Some(1.0));
        assert!(ledger.reserve(0.5));
        assert_eq!(ledger.used(BudgetScope::Session), 0.5);
        ledger.settle(0.5, entry("step:1", 0.3, false));
        // 预留释放,实际花费 0.3
        assert!((ledger.used(BudgetScope::Session) - 0.3).abs() < 1e-9);
        assert!((ledger.total_spent() - 0.3).abs() < 1e-9);
    }

    #[test]
    fn release_frees_reservation() {
        let mut ledger = SpendLedger::new();
        ledger.set_limit(BudgetScope::Session, Some(1.0));
        assert!(ledger.reserve(0.9));
        ledger.release(0.9);
        assert!((ledger.used(BudgetScope::Session)).abs() < 1e-9);
        assert!(ledger.reserve(0.9), "释放后可再次预留");
    }

    #[test]
    fn reset_turn_clears_turn_only() {
        let mut ledger = SpendLedger::new();
        ledger.set_limit(BudgetScope::Turn, Some(1.0));
        ledger.set_limit(BudgetScope::Session, Some(10.0));
        ledger.settle(0.0, entry("step:1", 0.5, false));
        ledger.reset_turn();
        assert_eq!(ledger.used(BudgetScope::Turn), 0.0, "turn 清零");
        assert!(
            (ledger.used(BudgetScope::Session) - 0.5).abs() < 1e-9,
            "session 保留"
        );
    }

    #[test]
    fn warn_action_allows_over_budget() {
        let mut ledger = SpendLedger::new();
        ledger.set_limit(BudgetScope::Session, Some(1.0));
        ledger.set_action(OverBudgetAction::Warn);
        ledger.settle(0.0, entry("step:1", 1.5, false));
        let check = ledger.check(0.1);
        assert!(check.allowed, "Warn 档不阻断");
        assert!(check.warn);
    }

    #[test]
    fn attribution_aggregation() {
        let mut ledger = SpendLedger::new();
        ledger.settle(0.0, entry("step:1", 0.1, false));
        ledger.settle(0.0, entry("step:1", 0.2, false));
        ledger.settle(0.0, entry("subagent:reviewer", 0.5, false));
        let rows = ledger.by_attribution();
        assert_eq!(rows[0].0, "subagent:reviewer");
        assert!((rows[0].1 - 0.5).abs() < 1e-9);
        assert!((rows[1].1 - 0.3).abs() < 1e-9);
    }

    #[test]
    fn attribution_by_prefix() {
        let mut ledger = SpendLedger::new();
        ledger.settle(0.0, entry("step:1", 0.1, false));
        ledger.settle(0.0, entry("step:2", 0.2, false));
        ledger.settle(0.0, entry("file:a.rs", 0.4, false));
        let prefixes = ledger.by_prefix();
        assert!((prefixes["step"] - 0.3).abs() < 1e-9);
        assert!((prefixes["file"] - 0.4).abs() < 1e-9);
    }

    #[test]
    fn cost_attribution_report() {
        let mut ledger = SpendLedger::new();
        ledger.settle(0.0, entry("step:1", 0.25, false));
        ledger.settle(0.0, entry("subagent:x", 0.75, false));
        let report = CostAttribution::from_ledger(&ledger);
        assert!((report.total_usd - 1.0).abs() < 1e-9);
        assert!((report.share_of("step:1") - 0.25).abs() < 1e-9);
        assert!((report.share_of("subagent:x") - 0.75).abs() < 1e-9);
        assert!(!report.is_empty());
    }

    #[test]
    fn share_of_unknown_is_zero() {
        let report = CostAttribution::default();
        assert_eq!(report.share_of("ghost"), 0.0);
        assert!(report.is_empty());
    }

    #[test]
    fn cache_savings_estimate() {
        let mut ledger = SpendLedger::new();
        ledger.settle(0.0, entry("step:1", 0.1, true));
        ledger.settle(0.0, entry("step:2", 0.2, false));
        let savings = ledger.cache_savings(0.9);
        assert!((savings - 0.09).abs() < 1e-9, "只有命中缓存的 0.1 打折");
    }

    #[test]
    fn remaining_calculation() {
        let mut ledger = SpendLedger::new();
        ledger.set_limit(BudgetScope::Session, Some(2.0));
        ledger.settle(0.0, entry("step:1", 0.5, false));
        let remaining = ledger.remaining(BudgetScope::Session).expect("limited");
        assert!((remaining - 1.5).abs() < 1e-9);
    }

    #[test]
    fn remaining_none_when_unlimited() {
        let ledger = SpendLedger::new();
        assert!(ledger.remaining(BudgetScope::Session).is_none());
    }

    #[test]
    fn remaining_never_negative() {
        let mut ledger = SpendLedger::new();
        ledger.set_limit(BudgetScope::Session, Some(1.0));
        ledger.settle(0.0, entry("step:1", 5.0, false));
        assert_eq!(ledger.remaining(BudgetScope::Session), Some(0.0));
    }
}
