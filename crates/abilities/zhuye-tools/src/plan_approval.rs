//! `PlanApprovalGate` — Plan mode 审批路由,镜像 `ApprovalGate` 但用
//! `PlanId` 作 key。
//!
//! v1.x Plan mode 状态机:
//! 1. `submission_loop` 收到 `Op::EnterPlanMode { task }` 或 `Op::ExitPlanMode`
//! 2. 生成 `PlanId`,在 `PlanApprovalGate` 注册 oneshot
//! 3. emit `EventMsg::PlanRequest { task }` (或 `PlanReady { plan_id, markdown }`)
//! 4. 等待 TUI 通过 `Op::PlanApproval { id, decision }` 回执
//! 5. approve → flip `PermissionMode` + emit `PermissionModeChanged`;
//!    reject → emit `PlanRejected { reason }`
//!
//! `PlanApprovalGate` 与 `ApprovalGate` 的关键差异:
//! - key 用 `PlanId` 而非 `request_id: String`
//! - 不接收 `tool_name` / `args` / `risk` —— Plan 审批只关心 `plan_id`
//! - 不需要 `session_allow` 集合 —— Plan mode 切换是全局会话级状态

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::oneshot;

use zhuye_protocol::{InteractionKind, PendingInteraction, PlanApprovalChoice, PlanId};

use crate::approval::PendingRegistry;

/// 待审批 plan 的等待表:`PlanId` → oneshot 发送端。
pub type PlanApprovalWaiters = Arc<Mutex<HashMap<PlanId, oneshot::Sender<PlanApprovalChoice>>>>;

/// 把决策投递给对应 plan_id 的 waiter。`submission_loop` 在收到
/// `Op::PlanApproval` 时调用;若 waiter 已 cancel / 已完成,返回 `false`。
pub fn complete_plan_approval(
    waiters: &PlanApprovalWaiters,
    plan_id: &PlanId,
    choice: PlanApprovalChoice,
) -> bool {
    let sender_opt = waiters.lock().remove(plan_id);
    match sender_opt {
        Some(tx) => tx.send(choice).is_ok(),
        None => {
            tracing::debug!(
                plan_id = %plan_id,
                "plan approval completion arrived but no waiter (cancelled or already resolved)"
            );
            false
        }
    }
}

/// Plan mode 审批 gate。owned by `submission_loop`,不直接挂到
/// `ToolExecutionQueue`(后者是 tool-level approval,这里是 session-level
/// mode 切换)。
///
/// 与 `ApprovalGate` 不同:PlanApprovalGate **不**直接 emit 事件,由
/// submission_loop 在拿到 plan_id 后用 `turn_tx` 发 `PlanRequest` /
/// `PlanReady`,gate 只负责注册 oneshot + 等待回执 + 把决策交回
/// 调用方(caller)。
pub struct PlanApprovalGate {
    waiters: PlanApprovalWaiters,
    /// v1.12:挂起交互注册表(可选;与 `ApprovalGate` 共享会话级同一张表)。
    /// plan 审批没有 TTL,注册条目 `expires_at_ms = None`;plan 的用户可见
    /// 了结路径已有 `PlanApproved` / `PlanRejected` 事件承载,注册表只服务
    /// `pending_interactions()` 查询面,不额外广播 / 落盘。
    pending: Option<PendingRegistry>,
}

impl PlanApprovalGate {
    pub fn new() -> Self {
        Self {
            waiters: Arc::new(Mutex::new(HashMap::new())),
            pending: None,
        }
    }

    /// 共享 waiters(测试 / 高级用法)。
    pub fn with_waiters(waiters: PlanApprovalWaiters) -> Self {
        Self {
            waiters,
            pending: None,
        }
    }

    /// v1.12:注入挂起交互注册表。`submission_loop` 装配时传
    /// `cfg.approval_context.pending.clone()`。
    pub fn with_pending_registry(mut self, pending: PendingRegistry) -> Self {
        self.pending = Some(pending);
        self
    }

    pub fn waiters(&self) -> &PlanApprovalWaiters {
        &self.waiters
    }

    /// 注册一个待审批 plan,返回 oneshot receiver。caller 在 emit
    /// `PlanRequest` / `PlanReady` 后用 `rx.await` 阻塞等待用户决策
    ///(`PlanApprovalChoice`:AutoMode / ManualApprove / Revise 三种)。
    pub fn register(&self, plan_id: PlanId) -> oneshot::Receiver<PlanApprovalChoice> {
        self.register_with_summary(plan_id, "")
    }

    /// v1.12:带摘要的注册(注册表条目可读)。摘要取规划任务 / plan 预览。
    pub fn register_with_summary(
        &self,
        plan_id: PlanId,
        summary: impl Into<String>,
    ) -> oneshot::Receiver<PlanApprovalChoice> {
        let (tx, rx) = oneshot::channel::<PlanApprovalChoice>();
        self.waiters.lock().insert(plan_id, tx);
        if let Some(reg) = &self.pending {
            let created_at_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or_default();
            crate::approval::pending_register(
                reg,
                PendingInteraction {
                    request_id: plan_id.to_string(),
                    kind: InteractionKind::Plan {
                        plan_id: plan_id.to_string(),
                    },
                    summary: summary.into(),
                    created_at_ms,
                    expires_at_ms: None,
                },
            );
        }
        rx
    }

    /// 完成一个待审批 plan(实例方法版,镜像 `ApprovalGate::complete`)。
    pub fn complete(&self, plan_id: PlanId, choice: PlanApprovalChoice) -> bool {
        if let Some(reg) = &self.pending {
            crate::approval::pending_resolve(reg, &plan_id.to_string());
        }
        complete_plan_approval(&self.waiters, &plan_id, choice)
    }

    /// 取消所有 pending plan approval(用于 session 关闭 / cancel token 触发)。
    pub fn cancel_all(&self) {
        if let Some(reg) = &self.pending {
            let ids: Vec<String> = self
                .waiters
                .lock()
                .keys()
                .map(|id| id.to_string())
                .collect();
            for id in ids {
                crate::approval::pending_resolve(reg, &id);
            }
        }
        let mut guard = self.waiters.lock();
        for (_, tx) in guard.drain() {
            // Receiver dropped → `rx.await` returns `Err`,caller 视作 deny。
            drop(tx);
        }
    }
}

impl Default for PlanApprovalGate {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn register_then_complete_returns_decision() {
        let gate = PlanApprovalGate::new();
        let plan_id = PlanId::new();
        let rx = gate.register(plan_id);
        assert!(gate.complete(plan_id, PlanApprovalChoice::AutoMode));
        let d = timeout(Duration::from_secs(1), rx).await.unwrap().unwrap();
        assert_eq!(d, PlanApprovalChoice::AutoMode);
    }

    #[tokio::test]
    async fn complete_unknown_plan_returns_false() {
        let gate = PlanApprovalGate::new();
        assert!(!gate.complete(PlanId::new(), PlanApprovalChoice::Revise));
    }

    #[tokio::test]
    async fn cancel_all_drops_pending_receivers() {
        let gate = PlanApprovalGate::new();
        let plan_id = PlanId::new();
        let rx = gate.register(plan_id);
        gate.cancel_all();
        // rx.await 现在返回 Err(canceled) → caller 视作 deny。
        let r = timeout(Duration::from_secs(1), rx).await.unwrap();
        assert!(r.is_err(), "cancelled waiter must surface Err");
        // Waiter 必须已被清空。
        assert!(gate.waiters().lock().is_empty());
    }

    #[tokio::test]
    async fn complete_plan_approval_helper_resolves_waiter() {
        let waiters: PlanApprovalWaiters = Arc::new(Mutex::new(HashMap::new()));
        let plan_id = PlanId::new();
        let (tx, rx) = oneshot::channel();
        waiters.lock().insert(plan_id, tx);
        assert!(complete_plan_approval(
            &waiters,
            &plan_id,
            PlanApprovalChoice::ManualApprove
        ));
        let d = timeout(Duration::from_secs(1), rx).await.unwrap().unwrap();
        assert_eq!(d, PlanApprovalChoice::ManualApprove);
    }

    #[tokio::test]
    async fn complete_plan_approval_helper_unknown_returns_false() {
        let waiters: PlanApprovalWaiters = Arc::new(Mutex::new(HashMap::new()));
        assert!(!complete_plan_approval(
            &waiters,
            &PlanId::new(),
            PlanApprovalChoice::AutoMode
        ));
    }

    #[test]
    fn default_gate_is_empty() {
        let gate = PlanApprovalGate::default();
        assert!(gate.waiters().lock().is_empty());
    }

    #[test]
    fn with_waiters_shares_state() {
        let waiters: PlanApprovalWaiters = Arc::new(Mutex::new(HashMap::new()));
        let gate = PlanApprovalGate::with_waiters(waiters.clone());
        let plan_id = PlanId::new();
        let _rx = gate.register(plan_id);
        // 共享 waiters 的 map 必须能看见新条目。
        assert!(waiters.lock().contains_key(&plan_id));
    }
}
