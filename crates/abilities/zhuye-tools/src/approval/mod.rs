//! `ApprovalGate` —— 单 turn 内,需要 `PermissionMode::Prompt` 的工具的审批路由。
//!
//! 流程(M6):
//! 1. `ToolExecutionQueue::execute_single` 检查 tool 的 `required_permission`。
//!    若为 `Prompt` 且用户未把该工具加入本 session 白名单,queue 调用
//!    [`ApprovalGate::ask_tool`]。
//! 2. `ask_tool` 生成新的 `request_id`,注册一个 `tokio::sync::oneshot`
//!    waiter,并在单 turn 的 event channel 上发送 `EventMsg::ApprovalRequest`。
//! 3. TUI(或任意客户端)处理该事件,并发送
//!    `Op::ToolApproval { id: request_id, decision }`。
//! 4. `submission_loop` 按 `sub_id` 找到对应 gate,调用
//!    [`ApprovalGate::complete`],完成 oneshot。
//! 5. `ask_tool` 返回 `ReviewDecision`,queue 据此行动。
//!
//! `tokio::select!` 集成 cancel:token 触发 cancel 时短路等待并返回
//! `ReviewDecision::Deny`。
//!
//! ## v1.9 S6+S7:fail-closed 不变量(与 buzz 对齐)
//!
//! 审批门的所有"拿不到人工决策"的路径**一律 Deny**,不存在
//! "默认放行"的分支:
//!
//! 1. **线断即拒**:event channel 关闭 / oneshot 对端 drop → Deny。
//! 2. **abort-safe 租约**:等待方 future 被 drop(cancel / panic /
//!    abort)时,`WaiterLease` 的 Drop 保证 waiter 条目从 map 清除,
//!    不留悬挂条目(否则占挂起名额 + complete 打到死条目)。
//! 3. **全局挂起上限**:`MAX_PENDING_APPROVALS`,超限直接 Deny
//!    (fail-closed),不让 waiter map 无界增长。
//! 4. **共享绝对死线(S6 审批超时)**:gate 构造时确定
//!    `created_at + approval_ttl` 一个**绝对**死线,本 turn 全部
//!    挂起审批共享;到点统一过期 → Deny(`expire_pending_approvals`
//!    供 submission_loop 周期清扫,等待侧 select 死线兜底)。
//!    过期即裁决,裁决即 Deny —— **过期后的审批不可能再被执行**。
//!
//! 默认 TTL 5 分钟(人类响应审批的合理上限);`without_approval_ttl()`
//! 显式关闭(headless 全自动流水线等场景)。

mod gate;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::oneshot;
use tracing::warn;

use zhuye_protocol::{AskUserAnswer, ReviewDecision};

pub use gate::ApprovalGate;

/// 待处理审批的 `request_id` → oneshot waiter 映射。
pub type ApprovalWaiters = Arc<Mutex<HashMap<String, oneshot::Sender<ReviewDecision>>>>;

/// v1.9 S7:全局挂起审批上限。超过即对**新请求**直接 Deny(fail-closed)。
/// 64 远超人能处理的并发 modal 数,又能在 UI 客户端失联/死循环时熔断。
pub const MAX_PENDING_APPROVALS: usize = 64;

/// v1.9 S6:默认审批 TTL(秒)。gate 构造起算,本 turn 全部挂起审批
/// 共享同一绝对死线。
pub const DEFAULT_APPROVAL_TTL_SECS: u64 = 300;

/// S6 统一过期原因串。裁决可见:客户端收到的是带该原因的 Deny。
pub(crate) fn expired_reason() -> String {
    format!(
        "approval expired: no human decision within TTL ({}s); fail-closed deny",
        DEFAULT_APPROVAL_TTL_SECS
    )
}

/// 待处理 ask-user-question 的 `request_id` → oneshot waiter,负责把
/// 结构化的 `AskUserAnswer` 送回 `AskUserQuestionTool::execute`。
///
/// 与 `ApprovalWaiters` 平行但**独立**(不混用同一个 map,避免 `ReviewDecision` /
/// `AskUserAnswer` 类型擦除带来的混乱)。由 `submission_loop` 持全局共享,
/// per-turn `ApprovalGate` 通过 `with_state(..., Some(qw.clone()))` 共享引用;
/// `Op::AskUserQuestionResponse` 由 `complete_ask_user_question` 全局函数路由。
pub type AskUserQuestionWaiters = Arc<Mutex<HashMap<String, oneshot::Sender<AskUserAnswer>>>>;

/// 待处理 `ask_user` 的 `request_id` → oneshot waiter,负责把用户自由文本
/// 回执送到 `AskUserTool::execute`。
pub type AskUserInputWaiters = Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>;

/// 一次会话及其子代理共享的审批路由状态。
///
/// 父会话和子代理必须使用同一组 waiter / 会话许可,否则子代理发出的
/// `approval_request` 虽能显示,父会话的审批回执也无法唤醒等待中的 gate。
#[derive(Clone, Default)]
pub struct ApprovalContext {
    pub waiters: ApprovalWaiters,
    pub question_waiters: AskUserQuestionWaiters,
    pub user_input_waiters: AskUserInputWaiters,
    pub session_allow: Arc<Mutex<HashSet<String>>>,
}

/// v1.2 review P2:bug-2:`ask_user` 工具 prompt 字节上限。LLM 误发
/// 超过此长度的字符串在 `ApprovalGate::ask_user` 入口直接 `InvalidArgs`
/// 拒绝,避免 event channel 撑爆 + TUI 渲染无滚动卡死。
///
/// 16 KiB 远超实际问答 prompt(常见 1 KiB 以内),又能 OOM 之前熔断。
pub const MAX_ASK_USER_PROMPT_BYTES: usize = 16 * 1024;

pub fn complete_ask_user_input(
    waiters: &AskUserInputWaiters,
    request_id: &str,
    text: String,
) -> bool {
    let sender_opt = waiters.lock().remove(request_id);
    match sender_opt {
        Some(tx) => tx.send(text).is_ok(),
        None => {
            warn!(
                request_id = %request_id,
                "ask_user completion arrived but no waiter registered (cancelled?)"
            );
            false
        }
    }
}

/// 在共享 waiter map 上直接完成一个待处理的 ask-user-question。
/// 给 `submission_loop` 用 —— 这样它不需要为每个到来的 `Op::AskUserQuestionResponse`
/// 找到最初的那个 gate。request_id 是 uuid,在单个 session 内全局唯一。
pub fn complete_ask_user_question(
    waiters: &AskUserQuestionWaiters,
    request_id: &str,
    answers: AskUserAnswer,
) -> bool {
    let sender_opt = waiters.lock().remove(request_id);
    match sender_opt {
        Some(tx) => tx.send(answers).is_ok(),
        None => {
            warn!(
                request_id = %request_id,
                "ask_user_question completion arrived but no waiter registered (cancelled?)"
            );
            false
        }
    }
}

/// 在共享 waiter map 上直接完成一个待处理的审批。给 `submission_loop`
/// 用 —— 这样它不需要为每个到来的 `Op::ToolApproval` 找到最初的那个
/// gate。request_id 是 uuid,在单个 session 内全局唯一。
pub fn complete_approval(
    waiters: &ApprovalWaiters,
    request_id: &str,
    decision: ReviewDecision,
) -> bool {
    let sender_opt = waiters.lock().remove(request_id);
    match sender_opt {
        Some(tx) => tx.send(decision).is_ok(),
        None => {
            warn!(
                request_id = %request_id,
                "approval completion arrived but no waiter is registered (already cancelled?)"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests;
