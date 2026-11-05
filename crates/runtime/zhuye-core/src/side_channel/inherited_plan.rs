//! 侧通道实时遵守父会话的 Plan 只读边界。

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use zhuye_hooks::{Hook, HookDecision, HookError, HookEvent, HookEventKind};
use zhuye_protocol::PermissionMode;

/// 不共享可写模式状态,只在父会话处于 Plan 时叠加其只读限制。
pub struct InheritedPlanGate {
    parent_mode: Arc<RwLock<PermissionMode>>,
    plan_gate: zhuye_hooks::builtins::PlanModeGate,
}

impl InheritedPlanGate {
    pub fn new(parent_mode: Arc<RwLock<PermissionMode>>) -> Self {
        Self {
            parent_mode,
            plan_gate: zhuye_hooks::builtins::PlanModeGate::default_mode(),
        }
    }
}

#[async_trait]
impl Hook for InheritedPlanGate {
    fn name(&self) -> &str {
        "side_channel_inherited_plan_gate"
    }

    fn events(&self) -> &[HookEventKind] {
        &[HookEventKind::PreToolUse]
    }

    async fn handle(&self, event: &HookEvent) -> Result<HookDecision, HookError> {
        let mut effective = event.clone();
        if *self.parent_mode.read() == PermissionMode::Plan
            && let HookEvent::PreToolUse { ctx, .. } = &mut effective
        {
            ctx.permission_mode = PermissionMode::Plan;
        }
        self.plan_gate.handle(&effective).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zhuye_hooks::HookContext;
    use zhuye_protocol::{ThreadId, TurnId};

    #[tokio::test]
    async fn parent_mode_is_checked_each_time_without_overwriting_child_mode() {
        let parent = Arc::new(RwLock::new(PermissionMode::Auto));
        let gate = InheritedPlanGate::new(parent.clone());
        let mut event = HookEvent::PreToolUse {
            tool: "write".into(),
            args: serde_json::json!({"path":"file.txt", "content":"x"}),
            ctx: HookContext {
                session_id: ThreadId::new(),
                turn_id: TurnId::new(),
                workspace: ".".into(),
                permission_mode: PermissionMode::Auto,
            },
        };
        assert!(matches!(
            gate.handle(&event).await.unwrap(),
            HookDecision::Allow
        ));
        *parent.write() = PermissionMode::Plan;
        assert!(matches!(
            gate.handle(&event).await.unwrap(),
            HookDecision::Deny { .. }
        ));
        *parent.write() = PermissionMode::Auto;
        assert!(matches!(
            gate.handle(&event).await.unwrap(),
            HookDecision::Allow
        ));
        if let HookEvent::PreToolUse { ctx, .. } = &mut event {
            ctx.permission_mode = PermissionMode::Plan;
        }
        assert!(matches!(
            gate.handle(&event).await.unwrap(),
            HookDecision::Deny { .. }
        ));
        assert_eq!(*parent.read(), PermissionMode::Auto);
    }
}
