//! 挂起人机交互注册表 —— gate 真相的会话级权威面。
//!
//! v1.12:此前挂起审批 / 问答 / 询问只有 oneshot waiter 表(只有 id,没有
//! 类别与摘要),客户端 reload 后无从重新水合审批 UI,pending 事实上只
//! 存在于前端内存投影。本注册表在 waiter 之外**并排**维护一份
//! [`PendingInteraction`] 快照行(类别 + 摘要 + 注册/过期时刻),随 waiter
//! 注册 / 了结同步增删,提供:
//!
//! - `ApprovalGate::pending_snapshot()` / `PlanApprovalGate` 侧注册 ——
//!   `AgentThread::pending_interactions()` 据此回答「当前有哪些挂起交互」;
//! - `InteractionResolved` 事件与 `RolloutRecord::InteractionRequest` /
//!   `InteractionResolution` 落盘的元数据来源。
//!
//! 注册表挂在 [`super::ApprovalContext`](会话级,父子代理共享)上;条目
//! 生命周期与 waiter 严格同宽:注册即入表,任何了结路径(应答 / TTL /
//! 取消 / 中断 / 通道关闭)即出表,不存在独立于 waiter 的悬挂条目。

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use zhuye_protocol::PendingInteraction;

/// 会话级挂起交互注册表:`request_id` → 快照行。
///
/// plan 审批以 `plan_id.to_string()` 作 key(`PlanApprovalGate` 与
/// `ApprovalGate` 共用同一张表);uuid 全局唯一保证不撞 key。
pub type PendingRegistry = Arc<Mutex<HashMap<String, PendingInteraction>>>;

/// 新建空注册表。
pub fn new_pending_registry() -> PendingRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

/// 登记一条挂起交互(与 waiter 注册同点调用)。
pub fn pending_register(registry: &PendingRegistry, interaction: PendingInteraction) {
    registry
        .lock()
        .insert(interaction.request_id.clone(), interaction);
}

/// 了结一条挂起交互;返回被移除的快照行(调用方据此 emit
/// `InteractionResolved` / 落盘 resolution)。条目不存在(已被清扫 /
/// 泄漏守卫清理)时返回 `None`。
pub fn pending_resolve(registry: &PendingRegistry, request_id: &str) -> Option<PendingInteraction> {
    registry.lock().remove(request_id)
}

/// 当前挂起交互快照,按注册时刻升序(最老的在前 —— UI 渲染顺序)。
pub fn pending_snapshot(registry: &PendingRegistry) -> Vec<PendingInteraction> {
    let mut out: Vec<PendingInteraction> = registry.lock().values().cloned().collect();
    out.sort_by_key(|p| p.created_at_ms);
    out
}

/// 泄漏守卫:持有期间保证 `request_id` 从注册表移除。
///
/// 与 [`super::gate::WaiterLease`](waiter 表的 abort-safe 租约)配套:
/// 注册路径「先入 waiter 表、再入注册表」,等待 future 无论从哪条路径
/// 退出,两条表都由 Drop / 显式 resolve 双保险清理,不留占名额的死条目。
pub struct PendingLease {
    registry: PendingRegistry,
    request_id: String,
}

impl PendingLease {
    /// 登记并返回守卫;`interaction.request_id` 即守卫目标。
    pub fn register(registry: &PendingRegistry, interaction: PendingInteraction) -> Self {
        let request_id = interaction.request_id.clone();
        pending_register(registry, interaction);
        Self {
            registry: Arc::clone(registry),
            request_id,
        }
    }
}

impl Drop for PendingLease {
    fn drop(&mut self) {
        pending_resolve(&self.registry, &self.request_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zhuye_protocol::{InteractionKind, InteractionOutcome};

    fn sample(id: &str, created_at_ms: u64) -> PendingInteraction {
        PendingInteraction {
            request_id: id.into(),
            kind: InteractionKind::Question,
            summary: "测试交互".into(),
            created_at_ms,
            expires_at_ms: None,
            payload: serde_json::Value::Null,
        }
    }

    #[test]
    fn register_snapshot_resolve_roundtrip() {
        let reg = new_pending_registry();
        pending_register(&reg, sample("b", 200));
        pending_register(&reg, sample("a", 100));
        let snap = pending_snapshot(&reg);
        assert_eq!(snap.len(), 2);
        // 快照按 created_at 升序。
        assert_eq!(snap[0].request_id, "a");
        assert_eq!(snap[1].request_id, "b");
        // resolve 返回被移除的行。
        let removed = pending_resolve(&reg, "b").expect("已登记条目必须能移除");
        assert_eq!(removed.request_id, "b");
        assert!(
            pending_resolve(&reg, "b").is_none(),
            "重复 resolve 返回 None"
        );
        assert_eq!(pending_snapshot(&reg).len(), 1);
    }

    #[tokio::test]
    async fn pending_lease_drop_cleans_entry() {
        let reg = new_pending_registry();
        {
            let _lease = PendingLease::register(&reg, sample("x", 1));
            assert_eq!(pending_snapshot(&reg).len(), 1);
        }
        // Drop 兜底:守卫离开作用域即出表,不留悬挂条目。
        assert!(pending_snapshot(&reg).is_empty());
    }

    // 抑制未直接使用告警:InteractionOutcome 在本模块的公共 API 里经由
    // 调用方(gate)使用,这里引用一次以锁定导入路径。
    #[test]
    fn outcome_import_path_smoke() {
        let _ = InteractionOutcome::Answered;
    }
}
