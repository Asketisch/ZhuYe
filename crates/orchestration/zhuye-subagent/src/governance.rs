//! # 子代理治理(F34)
//!
//! 子 agent 配「工牌」:持久作业记录、可恢复委派、账本轮询、并发自适应、
//! 防套娃深度限制、领域特化分型。
//!
//! ## 六件事
//!
//! 1. **作业账本**([`JobLedger`])—— 每个子代理的 status / output / error
//!    / 起止时间持久化,完成时发通知。
//! 2. **可恢复委派**([`JobLedger::resume_plan`])—— 崩溃后按 task_id 续跑。
//! 3. **账本轮询兜底**([`HeartbeatMonitor`])—— ACTIVE/IDLE 心跳,冷失败
//!    上限后判死(防子代理卡住没人管)。
//! 4. **AIMD 并发自适应**([`AimdController`])—— 成功加、失败减,
//!    不再写死 16。
//! 5. **派生深度上限**([`DepthGuard`])—— 防 A 派 B、B 派 C、C 又派 A。
//! 6. **能力分型**([`AgentKind`])—— 「这个子代理只做代码审查」。
//!
//! ## 为什么 AIMD 而不是固定并发
//!
//! 固定 16 在慢 provider 上会排队到超时,在快 provider 上又浪费。
//! AIMD(Additive Increase, Multiplicative Decrease)是 TCP 拥塞控制的
//! 同款思路:成功时缓慢加,失败时快速减 —— 自动逼近 provider 的实际
//! 承载能力,且**不会因为偶发失败就长期低并发**(加性增长会恢复)。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 子代理能力分型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    /// 通用(默认)。
    General,
    /// 只做代码审查。
    CodeReview,
    /// 只做调研 / 检索。
    Research,
    /// 只做测试编写。
    Testing,
    /// 只做文档。
    Documentation,
    /// 只做重构。
    Refactor,
}

impl AgentKind {
    pub const ALL: [AgentKind; 6] = [
        AgentKind::General,
        AgentKind::CodeReview,
        AgentKind::Research,
        AgentKind::Testing,
        AgentKind::Documentation,
        AgentKind::Refactor,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            AgentKind::General => "general",
            AgentKind::CodeReview => "code_review",
            AgentKind::Research => "research",
            AgentKind::Testing => "testing",
            AgentKind::Documentation => "documentation",
            AgentKind::Refactor => "refactor",
        }
    }

    pub fn parse(s: &str) -> Option<AgentKind> {
        AgentKind::ALL
            .iter()
            .copied()
            .find(|k| k.as_str() == s.trim().to_ascii_lowercase())
    }

    /// 该分型默认是否只读(审查 / 调研不改文件)。
    pub fn is_readonly_by_default(self) -> bool {
        matches!(self, AgentKind::CodeReview | AgentKind::Research)
    }
}

/// 作业状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// 排队中。
    Queued,
    /// 运行中。
    Running,
    /// 已完成。
    Completed,
    /// 失败。
    Failed,
    /// 已取消。
    Cancelled,
    /// 心跳丢失判死。
    Dead,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Completed => "completed",
            JobStatus::Failed => "failed",
            JobStatus::Cancelled => "cancelled",
            JobStatus::Dead => "dead",
        }
    }

    /// 是否终态。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled | JobStatus::Dead
        )
    }

    /// 是否可恢复(需要续跑)。
    pub fn is_resumable(self) -> bool {
        matches!(
            self,
            JobStatus::Running | JobStatus::Dead | JobStatus::Failed
        )
    }
}

/// 一条子代理作业记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobRecord {
    /// 作业 id(续跑用)。
    pub task_id: String,
    /// 分型。
    pub kind: AgentKind,
    /// 角色描述(如 "review the auth module")。
    pub role: String,
    pub status: JobStatus,
    /// 输出摘要(完成时)。
    #[serde(default)]
    pub output: Option<String>,
    /// 错误(失败时)。
    #[serde(default)]
    pub error: Option<String>,
    /// 派生深度(0 = 主会话直接派)。
    pub depth: u32,
    /// 创建时间。
    pub created_at: u64,
    /// 最近心跳时间。
    #[serde(default)]
    pub last_heartbeat_at: Option<u64>,
}

impl JobRecord {
    pub fn new(
        task_id: impl Into<String>,
        kind: AgentKind,
        role: impl Into<String>,
        depth: u32,
        created_at: u64,
    ) -> JobRecord {
        JobRecord {
            task_id: task_id.into(),
            kind,
            role: role.into(),
            status: JobStatus::Queued,
            output: None,
            error: None,
            depth,
            created_at,
            last_heartbeat_at: None,
        }
    }

    /// 运行时长(到 `now`)。
    pub fn running_secs(&self, now: u64) -> u64 {
        now.saturating_sub(self.created_at)
    }

    /// 心跳是否超时。
    pub fn heartbeat_stale(&self, now: u64, timeout_secs: u64) -> bool {
        match self.last_heartbeat_at {
            Some(last) => now.saturating_sub(last) > timeout_secs,
            None => self.status == JobStatus::Running,
        }
    }
}

/// 作业账本。
#[derive(Debug, Default)]
pub struct JobLedger {
    jobs: BTreeMap<String, JobRecord>,
}

impl JobLedger {
    pub fn new() -> JobLedger {
        JobLedger::default()
    }

    /// 登记作业。
    pub fn register(&mut self, job: JobRecord) {
        self.jobs.insert(job.task_id.clone(), job);
    }

    /// 查作业。
    pub fn get(&self, task_id: &str) -> Option<&JobRecord> {
        self.jobs.get(task_id)
    }

    /// 状态转移。
    pub fn transition(&mut self, task_id: &str, status: JobStatus) -> Result<(), LedgerError> {
        let job = self
            .jobs
            .get_mut(task_id)
            .ok_or_else(|| LedgerError::UnknownJob(task_id.to_string()))?;
        if job.status.is_terminal() {
            return Err(LedgerError::AlreadyTerminal {
                task_id: task_id.to_string(),
                status: job.status,
            });
        }
        job.status = status;
        Ok(())
    }

    /// 完成作业(带输出)。
    pub fn complete(
        &mut self,
        task_id: &str,
        output: impl Into<String>,
    ) -> Result<(), LedgerError> {
        self.transition(task_id, JobStatus::Completed)?;
        if let Some(job) = self.jobs.get_mut(task_id) {
            job.output = Some(output.into());
        }
        Ok(())
    }

    /// 失败作业。
    pub fn fail(&mut self, task_id: &str, error: impl Into<String>) -> Result<(), LedgerError> {
        self.transition(task_id, JobStatus::Failed)?;
        if let Some(job) = self.jobs.get_mut(task_id) {
            job.error = Some(error.into());
        }
        Ok(())
    }

    /// 心跳。
    pub fn heartbeat(&mut self, task_id: &str, at: u64) {
        if let Some(job) = self.jobs.get_mut(task_id) {
            job.last_heartbeat_at = Some(at);
        }
    }

    /// 全部作业(按创建时间排序)。
    pub fn all(&self) -> Vec<&JobRecord> {
        let mut out: Vec<&JobRecord> = self.jobs.values().collect();
        out.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.task_id.cmp(&b.task_id))
        });
        out
    }

    /// 按状态筛选。
    pub fn by_status(&self, status: JobStatus) -> Vec<&JobRecord> {
        self.all()
            .into_iter()
            .filter(|j| j.status == status)
            .collect()
    }

    /// 生成续跑计划:可恢复的作业列表。
    pub fn resume_plan(&self) -> Vec<&JobRecord> {
        self.all()
            .into_iter()
            .filter(|j| j.status.is_resumable())
            .collect()
    }

    /// 统计。
    pub fn counts(&self) -> BTreeMap<&'static str, usize> {
        let mut out = BTreeMap::new();
        for job in self.jobs.values() {
            *out.entry(job.status.as_str()).or_insert(0) += 1;
        }
        out
    }

    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }
}

/// 账本错误。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LedgerError {
    #[error("unknown job: {0}")]
    UnknownJob(String),
    #[error("job {task_id} is already terminal ({status:?})")]
    AlreadyTerminal { task_id: String, status: JobStatus },
}

/// 心跳监控(冷失败上限判死)。
#[derive(Debug, Clone)]
pub struct HeartbeatMonitor {
    /// 心跳超时(秒)。
    pub timeout_secs: u64,
    /// 连续超时多少次判死。
    pub max_stale_checks: u32,
}

impl Default for HeartbeatMonitor {
    fn default() -> Self {
        HeartbeatMonitor {
            timeout_secs: 60,
            max_stale_checks: 3,
        }
    }
}

impl HeartbeatMonitor {
    /// 检查一批作业,返回判死的 task_id。
    ///
    /// `stale_counts` 是「已连续超时次数」的累计表(调用方持有)。
    pub fn check(
        &self,
        jobs: &[&JobRecord],
        now: u64,
        stale_counts: &mut BTreeMap<String, u32>,
    ) -> Vec<String> {
        let mut dead = Vec::new();
        for job in jobs {
            if job.status != JobStatus::Running {
                continue;
            }
            if job.heartbeat_stale(now, self.timeout_secs) {
                let count = stale_counts.entry(job.task_id.clone()).or_insert(0);
                *count += 1;
                if *count >= self.max_stale_checks {
                    dead.push(job.task_id.clone());
                }
            } else {
                stale_counts.remove(&job.task_id);
            }
        }
        dead
    }
}

/// AIMD 并发控制器。
#[derive(Debug, Clone)]
pub struct AimdController {
    current: u32,
    min: u32,
    max: u32,
    /// 每次成功的加性增量。
    additive_step: u32,
    /// 每次失败的乘性因子(0.5 = 减半)。
    multiplicative_factor: f64,
}

impl AimdController {
    pub fn new(initial: u32, min: u32, max: u32) -> AimdController {
        let clamped = initial.clamp(min.max(1), max.max(min.max(1)));
        AimdController {
            current: clamped,
            min: min.max(1),
            max: max.max(min.max(1)),
            additive_step: 1,
            multiplicative_factor: 0.5,
        }
    }

    /// 当前并发度。
    pub fn current(&self) -> u32 {
        self.current
    }

    /// 成功 → 加性增长。
    pub fn on_success(&mut self) -> u32 {
        self.current = (self.current + self.additive_step).min(self.max);
        self.current
    }

    /// 失败 → 乘性减少。
    pub fn on_failure(&mut self) -> u32 {
        let reduced = (f64::from(self.current) * self.multiplicative_factor).floor() as u32;
        self.current = reduced.max(self.min);
        self.current
    }

    /// 是否处于上限。
    pub fn at_max(&self) -> bool {
        self.current >= self.max
    }

    /// 是否处于下限。
    pub fn at_min(&self) -> bool {
        self.current <= self.min
    }
}

/// 派生深度守卫。
#[derive(Debug, Clone)]
pub struct DepthGuard {
    /// 最大深度。
    pub max_depth: u32,
}

impl Default for DepthGuard {
    fn default() -> Self {
        DepthGuard { max_depth: 3 }
    }
}

impl DepthGuard {
    pub fn new(max_depth: u32) -> DepthGuard {
        DepthGuard { max_depth }
    }

    /// 能否从 `parent_depth` 再派一层。
    pub fn can_spawn(&self, parent_depth: u32) -> bool {
        parent_depth + 1 <= self.max_depth
    }

    /// 校验派生(返回子代理应有的深度)。
    pub fn check_spawn(&self, parent_depth: u32) -> Result<u32, DepthError> {
        if !self.can_spawn(parent_depth) {
            return Err(DepthError {
                parent_depth,
                max: self.max_depth,
            });
        }
        Ok(parent_depth + 1)
    }
}

/// 深度超限。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("spawn depth exceeded: parent at {parent_depth}, max {max}")]
pub struct DepthError {
    pub parent_depth: u32,
    pub max: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str, depth: u32) -> JobRecord {
        JobRecord::new(id, AgentKind::General, "role", depth, 100)
    }

    #[test]
    fn agent_kind_roundtrip() {
        for k in AgentKind::ALL {
            assert_eq!(AgentKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(AgentKind::parse("bogus"), None);
    }

    #[test]
    fn readonly_kinds() {
        assert!(AgentKind::CodeReview.is_readonly_by_default());
        assert!(AgentKind::Research.is_readonly_by_default());
        assert!(!AgentKind::General.is_readonly_by_default());
        assert!(!AgentKind::Refactor.is_readonly_by_default());
    }

    #[test]
    fn job_status_terminal_and_resumable() {
        assert!(JobStatus::Completed.is_terminal());
        assert!(!JobStatus::Running.is_terminal());
        assert!(JobStatus::Running.is_resumable());
        assert!(JobStatus::Dead.is_resumable());
        assert!(!JobStatus::Completed.is_resumable());
        assert!(!JobStatus::Queued.is_resumable());
    }

    #[test]
    fn ledger_register_and_get() {
        let mut l = JobLedger::new();
        l.register(job("t1", 1));
        assert_eq!(l.len(), 1);
        assert_eq!(l.get("t1").unwrap().depth, 1);
        assert!(l.get("ghost").is_none());
    }

    #[test]
    fn ledger_complete_records_output() {
        let mut l = JobLedger::new();
        l.register(job("t1", 1));
        l.transition("t1", JobStatus::Running).expect("run");
        l.complete("t1", "done!").expect("complete");
        let j = l.get("t1").unwrap();
        assert_eq!(j.status, JobStatus::Completed);
        assert_eq!(j.output.as_deref(), Some("done!"));
    }

    #[test]
    fn ledger_fail_records_error() {
        let mut l = JobLedger::new();
        l.register(job("t1", 1));
        l.transition("t1", JobStatus::Running).unwrap();
        l.fail("t1", "boom").unwrap();
        assert_eq!(l.get("t1").unwrap().error.as_deref(), Some("boom"));
    }

    #[test]
    fn terminal_job_cannot_transition() {
        let mut l = JobLedger::new();
        l.register(job("t1", 1));
        l.transition("t1", JobStatus::Running).unwrap();
        l.complete("t1", "x").unwrap();
        let err = l.transition("t1", JobStatus::Running).unwrap_err();
        assert!(matches!(err, LedgerError::AlreadyTerminal { .. }));
    }

    #[test]
    fn unknown_job_transition_errors() {
        let mut l = JobLedger::new();
        assert!(matches!(
            l.transition("ghost", JobStatus::Running),
            Err(LedgerError::UnknownJob(_))
        ));
    }

    #[test]
    fn resume_plan_lists_resumable() {
        let mut l = JobLedger::new();
        l.register(job("running", 0));
        l.transition("running", JobStatus::Running).unwrap();
        l.register(job("done", 0));
        l.transition("done", JobStatus::Running).unwrap();
        l.complete("done", "x").unwrap();
        l.register(job("queued", 0));
        let plan = l.resume_plan();
        let ids: Vec<&str> = plan.iter().map(|j| j.task_id.as_str()).collect();
        assert!(ids.contains(&"running"));
        assert!(!ids.contains(&"done"));
        assert!(!ids.contains(&"queued"), "queued 未开始不算续跑");
    }

    #[test]
    fn ledger_counts() {
        let mut l = JobLedger::new();
        l.register(job("a", 0));
        l.register(job("b", 0));
        l.transition("b", JobStatus::Running).unwrap();
        let counts = l.counts();
        assert_eq!(counts["queued"], 1);
        assert_eq!(counts["running"], 1);
    }

    #[test]
    fn heartbeat_stale_detection() {
        let mut j = job("t", 0);
        j.status = JobStatus::Running;
        assert!(j.heartbeat_stale(100, 60), "无心跳的 running 视为超时");
        j.last_heartbeat_at = Some(90);
        assert!(!j.heartbeat_stale(100, 60), "10s 前心跳,未超时");
        assert!(j.heartbeat_stale(200, 60), "110s 前心跳,已超时");
    }

    #[test]
    fn monitor_kills_after_consecutive_stale() {
        let mut j = job("t", 0);
        j.status = JobStatus::Running;
        j.last_heartbeat_at = Some(0);
        let jobs = vec![&j];
        let monitor = HeartbeatMonitor {
            timeout_secs: 10,
            max_stale_checks: 3,
        };
        let mut counts = BTreeMap::new();
        assert!(monitor.check(&jobs, 100, &mut counts).is_empty(), "第 1 次");
        assert!(monitor.check(&jobs, 101, &mut counts).is_empty(), "第 2 次");
        let dead = monitor.check(&jobs, 102, &mut counts);
        assert_eq!(dead, vec!["t"], "第 3 次判死");
    }

    #[test]
    fn monitor_resets_on_fresh_heartbeat() {
        let mut j = job("t", 0);
        j.status = JobStatus::Running;
        j.last_heartbeat_at = Some(0);
        let monitor = HeartbeatMonitor {
            timeout_secs: 10,
            max_stale_checks: 2,
        };
        let mut counts = BTreeMap::new();
        monitor.check(&[&j], 100, &mut counts);
        // 心跳恢复
        j.last_heartbeat_at = Some(101);
        monitor.check(&[&j], 102, &mut counts);
        assert!(counts.is_empty(), "新鲜心跳应清零计数");
    }

    #[test]
    fn monitor_ignores_non_running() {
        let j = job("t", 0); // Queued
        let monitor = HeartbeatMonitor::default();
        let mut counts = BTreeMap::new();
        assert!(monitor.check(&[&j], 10_000, &mut counts).is_empty());
    }

    #[test]
    fn aimd_additive_increase() {
        let mut c = AimdController::new(2, 1, 10);
        assert_eq!(c.current(), 2);
        assert_eq!(c.on_success(), 3);
        assert_eq!(c.on_success(), 4);
    }

    #[test]
    fn aimd_clamps_at_max() {
        let mut c = AimdController::new(9, 1, 10);
        assert_eq!(c.on_success(), 10);
        assert_eq!(c.on_success(), 10, "不超上限");
        assert!(c.at_max());
    }

    #[test]
    fn aimd_multiplicative_decrease() {
        let mut c = AimdController::new(8, 1, 16);
        assert_eq!(c.on_failure(), 4, "8 × 0.5");
        assert_eq!(c.on_failure(), 2);
        assert_eq!(c.on_failure(), 1);
    }

    #[test]
    fn aimd_clamps_at_min() {
        let mut c = AimdController::new(2, 2, 10);
        assert_eq!(c.on_failure(), 2, "不低于下限");
        assert!(c.at_min());
    }

    #[test]
    fn aimd_initial_clamped_to_range() {
        let c = AimdController::new(100, 2, 16);
        assert_eq!(c.current(), 16, "初始值超上限应夹紧");
        let c = AimdController::new(0, 2, 16);
        assert_eq!(c.current(), 2, "初始值低于下限应夹紧");
    }

    #[test]
    fn depth_guard_allows_within_limit() {
        let g = DepthGuard::new(3);
        assert!(g.can_spawn(0));
        assert!(g.can_spawn(2));
        assert_eq!(g.check_spawn(0), Ok(1));
        assert_eq!(g.check_spawn(2), Ok(3));
    }

    #[test]
    fn depth_guard_rejects_at_limit() {
        let g = DepthGuard::new(3);
        assert!(!g.can_spawn(3));
        let err = g.check_spawn(3).unwrap_err();
        assert_eq!(err.parent_depth, 3);
        assert_eq!(err.max, 3);
    }

    #[test]
    fn job_record_serializes() {
        let j = job("t", 1);
        let json = serde_json::to_string(&j).expect("serialize");
        let back: JobRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, j);
    }

    #[test]
    fn running_secs() {
        let j = job("t", 0);
        assert_eq!(j.running_secs(150), 50);
        assert_eq!(j.running_secs(50), 0, "不早于创建时间");
    }
}
