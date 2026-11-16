//! # 外部 agent 编排(本地版 F28)
//!
//! 把 Claude Code / Codex 这类 CLI 当「外包工」调度:派活 → 收结果 →
//! 转发审批。本模块只做**协议与生命周期**,不实现具体 CLI 适配(那是
//! `integrations/` 的事)。
//!
//! ## 三块
//!
//! 1. **能力协商**([`CapabilitySet`])—— 外部 agent 启动时报告自己支持
//!    什么(流式输出 / 工具调用 / 审批请求 / 会话恢复 …)。宿主据此决定
//!    走哪条路径,而不是假设对方全支持。
//! 2. **审批桥**([`PermissionMediator`])—— 外部 agent 的审批请求转发给
//!    本机用户。四策略 + **fail-closed 超时**(超时即拒,不静默放行)。
//! 3. **会话迁移五态**([`SessionState`])—— 外部会话从 created →
//!    active → suspended / resumed → terminated 的合法转移。
//!
//! ## 为什么审批桥必须 fail-closed
//!
//! 外部 agent 请求「我要跑 rm -rf」。若桥超时后默认放行,等于外部进程
//! 替用户做了决定。默认必须是**拒绝**,并把拒绝理由回给外部 agent
//! (它会据此调整策略,而不是卡死)。
//!
//! ## 与 ACP 的关系
//!
//! ACP(Agent Client Protocol)是编辑器 ↔ agent 的标准协议。本模块的
//! 能力协商 / 审批桥形状与 ACP 对齐,便于将来直接做 ACP server/client。

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// 外部 agent 的能力项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// 流式输出(增量 token)。
    Streaming,
    /// 能调用工具。
    ToolCalls,
    /// 会发审批请求。
    ApprovalRequests,
    /// 支持会话恢复(带 session id)。
    SessionResume,
    /// 支持中断。
    Interrupt,
    /// 支持多轮对话(保留上下文)。
    MultiTurn,
    /// 支持图像输入。
    ImageInput,
    /// 支持结构化输出(JSON)。
    StructuredOutput,
    /// 支持文件系统访问声明。
    FileAccess,
    /// 支持 MCP 工具。
    McpTools,
}

impl Capability {
    pub const ALL: [Capability; 10] = [
        Capability::Streaming,
        Capability::ToolCalls,
        Capability::ApprovalRequests,
        Capability::SessionResume,
        Capability::Interrupt,
        Capability::MultiTurn,
        Capability::ImageInput,
        Capability::StructuredOutput,
        Capability::FileAccess,
        Capability::McpTools,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Capability::Streaming => "streaming",
            Capability::ToolCalls => "tool_calls",
            Capability::ApprovalRequests => "approval_requests",
            Capability::SessionResume => "session_resume",
            Capability::Interrupt => "interrupt",
            Capability::MultiTurn => "multi_turn",
            Capability::ImageInput => "image_input",
            Capability::StructuredOutput => "structured_output",
            Capability::FileAccess => "file_access",
            Capability::McpTools => "mcp_tools",
        }
    }

    pub fn parse(s: &str) -> Option<Capability> {
        Capability::ALL
            .iter()
            .copied()
            .find(|c| c.as_str() == s.trim().to_ascii_lowercase())
    }
}

/// 能力集合(协商结果)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilitySet {
    caps: BTreeSet<Capability>,
}

impl CapabilitySet {
    pub fn new() -> CapabilitySet {
        CapabilitySet::default()
    }

    /// 从能力名列表构造(未知名忽略 —— 便于向前兼容)。
    pub fn from_names(names: &[String]) -> CapabilitySet {
        let mut set = BTreeSet::new();
        for n in names {
            if let Some(c) = Capability::parse(n) {
                set.insert(c);
            }
        }
        CapabilitySet { caps: set }
    }

    /// 声明支持某能力。
    pub fn with(mut self, cap: Capability) -> CapabilitySet {
        self.caps.insert(cap);
        self
    }

    pub fn supports(&self, cap: Capability) -> bool {
        self.caps.contains(&cap)
    }

    /// 交集(宿主 ∩ 外部 agent = 实际可用能力)。
    pub fn intersect(&self, other: &CapabilitySet) -> CapabilitySet {
        CapabilitySet {
            caps: self.caps.intersection(&other.caps).copied().collect(),
        }
    }

    /// 列出能力名(排序,便于快照)。
    pub fn names(&self) -> Vec<&'static str> {
        self.caps.iter().map(|c| c.as_str()).collect()
    }

    pub fn len(&self) -> usize {
        self.caps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.caps.is_empty()
    }
}

/// 审批桥策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediatorPolicy {
    /// 全部转发给用户(最安全)。
    #[default]
    ForwardAll,
    /// 只转发高危,低危自动批准。
    ForwardRisky,
    /// 只转发首次,后续同类自动记住。
    ForwardFirstOnly,
    /// 全部自动拒绝(无人值守 / 演示)。
    DenyAll,
}

/// 桥的裁决。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediatorDecision {
    pub approved: bool,
    /// 理由(回给外部 agent,便于它调整)。
    pub reason: String,
    /// 是否由超时触发(超时 → fail-closed 拒绝)。
    pub timed_out: bool,
}

impl MediatorDecision {
    pub fn approve(reason: impl Into<String>) -> MediatorDecision {
        MediatorDecision {
            approved: true,
            reason: reason.into(),
            timed_out: false,
        }
    }

    pub fn deny(reason: impl Into<String>) -> MediatorDecision {
        MediatorDecision {
            approved: false,
            reason: reason.into(),
            timed_out: false,
        }
    }

    /// 超时 → fail-closed 拒绝。
    pub fn timeout() -> MediatorDecision {
        MediatorDecision {
            approved: false,
            reason: "approval bridge timeout: fail-closed deny (no human decision)".to_string(),
            timed_out: true,
        }
    }
}

/// 审批请求(来自外部 agent)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalApprovalRequest {
    /// 外部 agent 的请求 id(回执时带回)。
    pub request_id: String,
    /// 要执行的操作描述(如 `bash: rm -rf build`)。
    pub action: String,
    /// 是否高危。
    pub risky: bool,
}

/// 审批桥。
#[derive(Debug)]
pub struct PermissionMediator {
    policy: MediatorPolicy,
    /// `ForwardFirstOnly` 记住的 action 前缀(已批准过的)。
    approved_prefixes: std::collections::HashSet<String>,
}

impl PermissionMediator {
    pub fn new(policy: MediatorPolicy) -> PermissionMediator {
        PermissionMediator {
            policy,
            approved_prefixes: std::collections::HashSet::new(),
        }
    }

    pub fn policy(&self) -> MediatorPolicy {
        self.policy
    }

    /// 决定是否**需要**问用户(不需要问时直接返回裁决)。
    ///
    /// `None` = 需要转发给用户;`Some(decision)` = 桥自己定了。
    pub fn pre_decide(&mut self, req: &ExternalApprovalRequest) -> Option<MediatorDecision> {
        match self.policy {
            MediatorPolicy::DenyAll => {
                Some(MediatorDecision::deny("approval bridge policy = deny_all"))
            }
            MediatorPolicy::ForwardAll => None,
            MediatorPolicy::ForwardRisky => {
                if req.risky {
                    None
                } else {
                    Some(MediatorDecision::approve(
                        "low-risk action auto-approved (policy = forward_risky)",
                    ))
                }
            }
            MediatorPolicy::ForwardFirstOnly => {
                let key = action_prefix(&req.action);
                if self.approved_prefixes.contains(&key) {
                    Some(MediatorDecision::approve(format!(
                        "already approved this session (prefix = {key})"
                    )))
                } else {
                    None
                }
            }
        }
    }

    /// 用户裁决后记录(供 `ForwardFirstOnly` 记忆)。
    pub fn record_approval(&mut self, req: &ExternalApprovalRequest) {
        if self.policy == MediatorPolicy::ForwardFirstOnly {
            self.approved_prefixes.insert(action_prefix(&req.action));
        }
    }

    /// 已记忆的批准前缀数。
    pub fn remembered(&self) -> usize {
        self.approved_prefixes.len()
    }
}

/// 取 action 的前缀(第一个词),用于 `ForwardFirstOnly` 记忆。
fn action_prefix(action: &str) -> String {
    action
        .split([':', ' ', '\t'])
        .next()
        .unwrap_or(action)
        .to_string()
}

/// 外部会话状态(五态)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// 刚创建,尚未开始。
    Created,
    /// 运行中。
    Active,
    /// 已挂起(可恢复)。
    Suspended,
    /// 已恢复(重新运行中)。
    Resumed,
    /// 已终止(终态)。
    Terminated,
}

impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionState::Created => "created",
            SessionState::Active => "active",
            SessionState::Suspended => "suspended",
            SessionState::Resumed => "resumed",
            SessionState::Terminated => "terminated",
        }
    }

    /// 是否终态。
    pub fn is_terminal(self) -> bool {
        matches!(self, SessionState::Terminated)
    }

    /// 从当前状态转移到 `next` 是否合法。
    ///
    /// 合法转移:
    /// `Created → Active | Terminated`
    /// `Active → Suspended | Terminated`
    /// `Suspended → Resumed | Terminated`
    /// `Resumed → Suspended | Terminated`
    /// `Terminated → (无)`
    pub fn can_transition_to(self, next: SessionState) -> bool {
        matches!(
            (self, next),
            (SessionState::Created, SessionState::Active)
                | (SessionState::Created, SessionState::Terminated)
                | (SessionState::Active, SessionState::Suspended)
                | (SessionState::Active, SessionState::Terminated)
                | (SessionState::Suspended, SessionState::Resumed)
                | (SessionState::Suspended, SessionState::Terminated)
                | (SessionState::Resumed, SessionState::Suspended)
                | (SessionState::Resumed, SessionState::Terminated)
        )
    }
}

/// 会话状态机。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMachine {
    pub state: SessionState,
    /// 已发生的转移历史(诊断)。
    pub history: Vec<SessionState>,
}

impl Default for SessionMachine {
    fn default() -> Self {
        SessionMachine {
            state: SessionState::Created,
            history: vec![SessionState::Created],
        }
    }
}

impl SessionMachine {
    pub fn new() -> SessionMachine {
        SessionMachine::default()
    }

    /// 尝试转移。非法转移返回错误(不静默改状态)。
    pub fn transition(&mut self, next: SessionState) -> Result<(), TransitionError> {
        if !self.state.can_transition_to(next) {
            return Err(TransitionError {
                from: self.state,
                to: next,
            });
        }
        self.state = next;
        self.history.push(next);
        Ok(())
    }

    /// 是否已终止。
    pub fn is_terminated(&self) -> bool {
        self.state.is_terminal()
    }
}

/// 非法状态转移。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("illegal session transition: {} → {}", from.as_str(), to.as_str())]
pub struct TransitionError {
    pub from: SessionState,
    pub to: SessionState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_roundtrip() {
        for c in Capability::ALL {
            assert_eq!(Capability::parse(c.as_str()), Some(c));
        }
        assert_eq!(Capability::parse("bogus"), None);
    }

    #[test]
    fn capability_set_from_names_ignores_unknown() {
        let set = CapabilitySet::from_names(&[
            "streaming".into(),
            "tool_calls".into(),
            "future_feature".into(),
        ]);
        assert_eq!(set.len(), 2);
        assert!(set.supports(Capability::Streaming));
        assert!(!set.supports(Capability::ImageInput));
    }

    #[test]
    fn capability_intersection() {
        let host = CapabilitySet::new()
            .with(Capability::Streaming)
            .with(Capability::ToolCalls)
            .with(Capability::Interrupt);
        let external = CapabilitySet::new()
            .with(Capability::Streaming)
            .with(Capability::Interrupt)
            .with(Capability::McpTools);
        let common = host.intersect(&external);
        assert_eq!(
            common.names(),
            vec!["streaming", "interrupt"],
            "BTreeSet 按枚举声明序"
        );
        assert!(!common.supports(Capability::ToolCalls));
    }

    #[test]
    fn capability_set_serializes() {
        let set = CapabilitySet::new().with(Capability::Streaming);
        let json = serde_json::to_string(&set).expect("serialize");
        let back: CapabilitySet = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, set);
    }

    #[test]
    fn mediator_deny_all() {
        let mut m = PermissionMediator::new(MediatorPolicy::DenyAll);
        let req = ExternalApprovalRequest {
            request_id: "r1".into(),
            action: "bash: ls".into(),
            risky: false,
        };
        let d = m.pre_decide(&req).expect("decided");
        assert!(!d.approved);
        assert!(d.reason.contains("deny_all"));
    }

    #[test]
    fn mediator_forward_all_always_asks() {
        let mut m = PermissionMediator::new(MediatorPolicy::ForwardAll);
        let req = ExternalApprovalRequest {
            request_id: "r1".into(),
            action: "bash: ls".into(),
            risky: false,
        };
        assert!(m.pre_decide(&req).is_none(), "ForwardAll 应转发");
    }

    #[test]
    fn mediator_forward_risky_auto_approves_low_risk() {
        let mut m = PermissionMediator::new(MediatorPolicy::ForwardRisky);
        let low = ExternalApprovalRequest {
            request_id: "r1".into(),
            action: "read: file".into(),
            risky: false,
        };
        let high = ExternalApprovalRequest {
            request_id: "r2".into(),
            action: "bash: rm -rf".into(),
            risky: true,
        };
        assert!(m.pre_decide(&low).expect("auto").approved);
        assert!(m.pre_decide(&high).is_none(), "高危应转发");
    }

    #[test]
    fn mediator_forward_first_only_remembers() {
        let mut m = PermissionMediator::new(MediatorPolicy::ForwardFirstOnly);
        let req = ExternalApprovalRequest {
            request_id: "r1".into(),
            action: "bash: npm test".into(),
            risky: false,
        };
        assert!(m.pre_decide(&req).is_none(), "首次应问");
        m.record_approval(&req);
        assert_eq!(m.remembered(), 1);
        // 同前缀再来 → 自动批准。
        let again = ExternalApprovalRequest {
            request_id: "r2".into(),
            action: "bash: npm build".into(),
            risky: false,
        };
        let d = m.pre_decide(&again).expect("auto");
        assert!(d.approved);
        assert!(d.reason.contains("already approved"), "{}", d.reason);
    }

    #[test]
    fn mediator_timeout_is_fail_closed() {
        let d = MediatorDecision::timeout();
        assert!(!d.approved, "超时必须拒绝");
        assert!(d.timed_out);
        assert!(d.reason.contains("fail-closed"));
    }

    #[test]
    fn session_happy_path() {
        let mut m = SessionMachine::new();
        assert_eq!(m.state, SessionState::Created);
        m.transition(SessionState::Active).expect("active");
        m.transition(SessionState::Suspended).expect("suspended");
        m.transition(SessionState::Resumed).expect("resumed");
        m.transition(SessionState::Terminated).expect("terminated");
        assert!(m.is_terminated());
        assert_eq!(m.history.len(), 5);
    }

    #[test]
    fn session_illegal_transition_rejected() {
        let mut m = SessionMachine::new();
        // Created 不能直接到 Suspended
        assert!(m.transition(SessionState::Suspended).is_err());
        assert_eq!(m.state, SessionState::Created, "非法转移不改状态");
    }

    #[test]
    fn session_terminated_is_final() {
        let mut m = SessionMachine::new();
        m.transition(SessionState::Terminated).expect("terminate");
        for next in [
            SessionState::Created,
            SessionState::Active,
            SessionState::Suspended,
            SessionState::Resumed,
        ] {
            assert!(
                m.transition(next).is_err(),
                "终态后不得转移到 {}",
                next.as_str()
            );
        }
    }

    #[test]
    fn session_resumed_can_suspend_again() {
        let mut m = SessionMachine::new();
        m.transition(SessionState::Active).unwrap();
        m.transition(SessionState::Suspended).unwrap();
        m.transition(SessionState::Resumed).unwrap();
        m.transition(SessionState::Suspended).unwrap();
        assert_eq!(m.state, SessionState::Suspended);
    }

    #[test]
    fn transition_matrix_complete() {
        // 逐条验证 can_transition_to 的合法性表。
        assert!(SessionState::Created.can_transition_to(SessionState::Active));
        assert!(SessionState::Active.can_transition_to(SessionState::Suspended));
        assert!(SessionState::Suspended.can_transition_to(SessionState::Resumed));
        assert!(!SessionState::Created.can_transition_to(SessionState::Resumed));
        assert!(!SessionState::Active.can_transition_to(SessionState::Active));
        assert!(!SessionState::Terminated.can_transition_to(SessionState::Terminated));
    }
}
