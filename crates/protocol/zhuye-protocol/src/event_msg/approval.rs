//! 审批 + 权限冒泡(payload)载荷。

use serde::{Deserialize, Serialize};

use crate::item::{PlanId, RiskLevel};

/// 审批请求:工具调用或 hook 决策需用户裁决时发出,客户端以同一
/// `request_id` 回执(`Op::ToolApproval` / `Op::HookApproval`)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequestEvent {
    /// 唯一 id;客户端会在 `Op::ToolApproval` / `Op::HookApproval` 中回传。
    pub request_id: String,
    /// 待审批的对象(工具调用或 hook 决策)。
    pub kind: ApprovalKind,
    /// 风险等级(仅供 UI 提示,不强制)。
    #[serde(default)]
    pub risk: RiskLevel,
}

/// 被审批的对象。工具调用携带工具名 + 参数;hook 审批携带 hook 名 + 决策载荷的
/// 简短可读预览。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApprovalKind {
    /// 工具调用审批。
    Tool {
        /// 待审批的工具名。
        tool_name: String,
        /// 工具调用参数(JSON 原样)。
        args: serde_json::Value,
    },
    /// hook 决策审批(如 PreToolUse hook 返回 Ask)。
    Hook {
        /// 发起审批的 hook 名(如 `"pre_tool_use"`)。
        hook_name: String,
        /// hook 决策载荷的简短可读预览(如 hook 给出的询问理由)。
        decision_preview: String,
    },
    /// v1.x Plan mode: 用户审批 plan markdown。`summary` 是 plan 的
    /// 简短预览(首 100 字符),TUI modal 用作预览文字;
    /// `plan_id` 与 `PlanReady` / `PlanApproved` / `PlanRejected` 的 id
    /// 对应,便于前端把 approval 与具体 plan 配对渲染。
    Plan {
        /// 与 `PlanReady` / `PlanApproved` / `PlanRejected` 配对的 plan id。
        plan_id: PlanId,
        /// plan markdown 的简短预览(首 100 字符)。
        summary: String,
    },
}

/// Bubble 权限模式下的非阻塞工具执行通知。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionBubbleEvent {
    /// 被执行的工具名。
    pub tool_name: String,
    /// 参数预览(取前 3 个顶层字段,`key=value` 以逗号分隔);参数非对象或
    /// 为空时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_preview: Option<String>,
    /// 风险等级(仅供 UI 提示,不强制)。
    #[serde(default)]
    pub risk: RiskLevel,
}
