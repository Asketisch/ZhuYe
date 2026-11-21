//! 人机交互(审批 / 提问 / 询问 / plan)协议载荷。
//!
//! v1.12:补齐 gate 的「可查询 + 可观测」两面:
//! - [`PendingInteraction`] 是挂起交互的**轻量快照行**(`AgentThread::pending_interactions`
//!   查询 API 的返回载荷),让客户端在 webview reload / 换绑后能重新水合
//!   审批弹窗,而不必依赖前端内存投影;
//! - [`InteractionResolvedEvent`] 在交互以**任意路径**了结(应答 / TTL 过期 /
//!   取消 / 中断 / 通道关闭)时发出,让未当场应答的其他客户端(后台会话
//!   投影、多窗口)能同步清理本地挂起态,不再依赖各自的猜测。
//!
//! 挂起交互的**权威事实在引擎侧**(waiter 表 + 元数据注册表),客户端的
//! pending 状态只是投影 —— 这是与「pending 只存在于前端内存」的旧形态
//! 的本质区别。

use serde::{Deserialize, Serialize};

/// 挂起交互的类别。与 [`crate::event_msg::ApprovalKind`] 的差异:这里只带
/// 轻量标识(工具名 / hook 名 / plan id),不携带完整参数 —— 查询 API 面向
/// 「重新水合 UI」,大参数由客户端按需走既有 replay 链路取。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InteractionKind {
    /// 工具调用审批(`Op::ToolApproval` 回执)。
    Tool {
        /// 待审批的工具名。
        tool_name: String,
    },
    /// hook 决策审批(`Op::HookApproval` 回执)。
    Hook {
        /// 发起审批的 hook 名。
        hook_name: String,
    },
    /// 结构化问答(`Op::AskUserQuestionResponse` 回执)。
    Question,
    /// 自由文本询问(`Op::AskUserInputResponse` 回执)。
    AskUser,
    /// plan 审批(`Op::PlanApproval` 回执)。
    Plan {
        /// 与 `PlanRequest` / `PlanReady` 配对的 plan id。
        plan_id: String,
    },
}

/// 交互的了结路径。`Answered` 表示收到了匹配回执;其余为引擎侧 fail-closed
/// 或通道级了结,客户端应据此清理本地挂起态并(如适用)向用户提示原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionOutcome {
    /// 收到匹配回执(用户的显式决策 / 答案)。
    Answered,
    /// TTL 到期,引擎按 fail-closed 语义自动拒绝。
    Expired,
    /// 会话级取消(`Op::Shutdown` 级联 / 通道关闭)。
    Cancelled,
    /// 回合被中断(`Op::Interrupt` 取消回合级令牌)。
    Interrupted,
    /// 事件通道关闭(对端 drop / channel closed),等待方收不到任何决策。
    ChannelClosed,
}

/// 一个挂起人机交互的快照行。
///
/// 由引擎侧 `ApprovalGate` / `PlanApprovalGate` 在注册 waiter 时写入
/// 会话级注册表,在交互了结时移除。`AgentThread::pending_interactions()`
/// 返回按 `created_at_ms` 升序的当前快照,客户端用它重新水合审批 UI。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingInteraction {
    /// 唯一 id;应答回执(`Op::ToolApproval` 等)按它配对。
    pub request_id: String,
    /// 交互类别(工具 / hook / 问答 / 询问 / plan)。
    pub kind: InteractionKind,
    /// 人类可读摘要(如工具命令预览 / 询问正文截断)。长度有界,仅用于 UI 提示。
    pub summary: String,
    /// 注册时刻(unix 毫秒)。
    pub created_at_ms: u64,
    /// 预期过期时刻(unix 毫秒);`None` = 未启用 TTL(等待永不过期,
    /// 由取消 / 回执兜底)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
}

/// 一个挂起交互已了结(additive 新变体,v1.12)。
///
/// 与 `ApprovalRequest` / `AskUserQuestion` / `AskUserInput` /
/// `PlanRequest` / `PlanReady` 按 `request_id` 配对。客户端收到后应
/// **无条件**清理本地对应的挂起态 —— 即使它就是应答方(幂等确认)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionResolvedEvent {
    /// 了结的交互 id。
    pub request_id: String,
    /// 交互类别(与请求时的类别一致)。
    pub kind: InteractionKind,
    /// 了结路径。
    pub outcome: InteractionOutcome,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interaction_resolved_roundtrip() {
        let ev = EventMsg::InteractionResolved(InteractionResolvedEvent {
            request_id: "req-1".into(),
            kind: InteractionKind::Tool {
                tool_name: "bash".into(),
            },
            outcome: InteractionOutcome::Expired,
        });
        let j = serde_json::to_string(&ev).unwrap();
        assert!(j.contains(r#""type":"interaction_resolved""#), "got: {j}");
        assert!(j.contains(r#""kind":{"type":"tool""#), "got: {j}");
        assert!(j.contains(r#""outcome":"expired""#), "got: {j}");
        let back: EventMsg = serde_json::from_str(&j).unwrap();
        match back {
            EventMsg::InteractionResolved(e) => {
                assert_eq!(e.request_id, "req-1");
                assert_eq!(e.outcome, InteractionOutcome::Expired);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn pending_interaction_serde_roundtrip() {
        let p = PendingInteraction {
            request_id: "r1".into(),
            kind: InteractionKind::Plan {
                plan_id: "11111111-2222-3333-4444-555555555555".into(),
            },
            summary: "重构 auth 模块…".into(),
            created_at_ms: 1_700_000_000_000,
            expires_at_ms: Some(1_700_000_300_000),
        };
        let j = serde_json::to_string(&p).unwrap();
        let back: PendingInteraction = serde_json::from_str(&j).unwrap();
        assert_eq!(back, p);
        // expires_at_ms = None 时序列化省略(skip_serializing_if)。
        let p2 = PendingInteraction {
            expires_at_ms: None,
            ..p
        };
        let j2 = serde_json::to_string(&p2).unwrap();
        assert!(!j2.contains("expires_at_ms"), "got: {j2}");
    }

    use super::super::EventMsg;
}
