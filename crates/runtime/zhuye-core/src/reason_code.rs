//! # 派发 ReasonCode 与工作记忆连续性通知(F11)
//!
//! 两件相关的事:
//!
//! 1. [`ReasonCode`] —— **稳定枚举**,给客户端透传"这次派发为什么
//!    中断/被拒"。字符串码散落在各处(`"session_poisoned"` /
//!    `"token_budget_exceeded"`)时,宿主只能靠字符串匹配,改文案即破坏
//!    兼容。这里收成枚举 + 稳定 wire 名。
//!
//! 2. [`ContinuityNotice`] —— 会话退役(或被换新)时,**明确告诉用户
//!    丢了什么**。四类工作记忆:待办、目标、临时笔记、上下文摘要。
//!    静默换新会话最伤用户信任 —— 用户会以为 agent 还记得。
//!
//! ## 为什么「四类」而不是「全部」
//!
//! 这四类是**跨轮次才有意义**的状态:todo 清单、goal、note、压缩摘要。
//! 消息历史本身由会话存储负责(退役会话仍可回看),不在本通知范围。

use serde::{Deserialize, Serialize};

/// 派发中断 / 被拒的稳定原因码。
///
/// wire 名即 `snake_case` 字面量(经 `#[serde(rename_all)]`),宿主可按名
/// 匹配,不依赖人类可读 message。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    /// 会话被判定中毒并退役,任务转到新会话。
    SessionPoisoned,
    /// token 预算耗尽。
    TokenBudgetExceeded,
    /// 达到最大迭代轮数。
    MaxIterations,
    /// 用户主动打断。
    UserInterrupted,
    /// 审批被拒(用户拒绝或超时 fail-closed)。
    ApprovalDenied,
    /// 工具执行失败且不可恢复。
    ToolFailure,
    /// 上下文无法继续压缩(超限)。
    ContextExhausted,
    /// 模型 provider 全部失败(含 failover 耗尽)。
    ProviderUnavailable,
    /// 其它 / 未分类(保留位,宿主应回退到 message 文本)。
    Other,
}

impl ReasonCode {
    /// wire 名(`snake_case` 字面量)。
    pub fn as_str(self) -> &'static str {
        match self {
            ReasonCode::SessionPoisoned => "session_poisoned",
            ReasonCode::TokenBudgetExceeded => "token_budget_exceeded",
            ReasonCode::MaxIterations => "max_iterations",
            ReasonCode::UserInterrupted => "user_interrupted",
            ReasonCode::ApprovalDenied => "approval_denied",
            ReasonCode::ToolFailure => "tool_failure",
            ReasonCode::ContextExhausted => "context_exhausted",
            ReasonCode::ProviderUnavailable => "provider_unavailable",
            ReasonCode::Other => "other",
        }
    }

    /// 从已有字符串码解析(兼容尚未迁移到枚举的旧路径)。
    pub fn parse(s: &str) -> Option<ReasonCode> {
        Some(match s {
            "session_poisoned" => ReasonCode::SessionPoisoned,
            "token_budget_exceeded" => ReasonCode::TokenBudgetExceeded,
            "max_iterations" => ReasonCode::MaxIterations,
            "user_interrupted" | "interrupted" => ReasonCode::UserInterrupted,
            "approval_denied" => ReasonCode::ApprovalDenied,
            "tool_failure" => ReasonCode::ToolFailure,
            "context_exhausted" => ReasonCode::ContextExhausted,
            "provider_unavailable" => ReasonCode::ProviderUnavailable,
            "other" => ReasonCode::Other,
            _ => return None,
        })
    }

    /// 该原因码是否意味着**工作记忆丢失**(需要发连续性通知)。
    pub fn implies_memory_loss(self) -> bool {
        matches!(
            self,
            ReasonCode::SessionPoisoned | ReasonCode::ContextExhausted
        )
    }
}

/// 四类工作记忆。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// 待办清单(`TodoWrite` 槽)。
    Todos,
    /// 目标(`goal` 槽)。
    Goal,
    /// 临时笔记(`NoteStore`)。
    Notes,
    /// 上下文压缩摘要。
    CompactionSummary,
}

impl MemoryKind {
    /// 四类全集的稳定顺序。
    pub const ALL: [MemoryKind; 4] = [
        MemoryKind::Todos,
        MemoryKind::Goal,
        MemoryKind::Notes,
        MemoryKind::CompactionSummary,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            MemoryKind::Todos => "todos",
            MemoryKind::Goal => "goal",
            MemoryKind::Notes => "notes",
            MemoryKind::CompactionSummary => "compaction_summary",
        }
    }

    /// 人读名(通知文案用)。
    pub fn label(self) -> &'static str {
        match self {
            MemoryKind::Todos => "待办清单",
            MemoryKind::Goal => "目标",
            MemoryKind::Notes => "临时笔记",
            MemoryKind::CompactionSummary => "上下文摘要",
        }
    }
}

/// 一条工作记忆连续性通知。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinuityNotice {
    pub reason: ReasonCode,
    /// 本次换新会话时**丢失/未迁移**的工作记忆类型(空 = 全部已迁移)。
    pub lost: Vec<MemoryKind>,
    /// 人读一句话(含丢失项;空 lost 时说明"已全部迁移")。
    pub message: String,
}

impl ContinuityNotice {
    /// 按原因码与丢失项构造。`lost` 会按 [`MemoryKind::ALL`] 的稳定顺序排序。
    pub fn new(reason: ReasonCode, mut lost: Vec<MemoryKind>) -> ContinuityNotice {
        lost.sort_by_key(|k| {
            MemoryKind::ALL
                .iter()
                .position(|x| x == k)
                .unwrap_or(usize::MAX)
        });
        lost.dedup();
        let message = if lost.is_empty() {
            format!(
                "会话已换新({})。待办 / 目标 / 笔记 / 上下文摘要均已迁移,工作可继续。",
                reason.as_str()
            )
        } else {
            let names = lost
                .iter()
                .map(|k| k.label())
                .collect::<Vec<_>>()
                .join("、");
            format!(
                "会话已换新({})。以下工作记忆未被迁移,请重新提供:{names}。",
                reason.as_str()
            )
        };
        ContinuityNotice {
            reason,
            lost,
            message,
        }
    }

    /// 是否确实丢了东西。
    pub fn has_loss(&self) -> bool {
        !self.lost.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_code_wire_names_are_stable() {
        // wire 名是宿主匹配契约,改动即破坏兼容 —— 这里锁死字面量。
        assert_eq!(ReasonCode::SessionPoisoned.as_str(), "session_poisoned");
        assert_eq!(
            ReasonCode::TokenBudgetExceeded.as_str(),
            "token_budget_exceeded"
        );
        assert_eq!(ReasonCode::MaxIterations.as_str(), "max_iterations");
        assert_eq!(ReasonCode::UserInterrupted.as_str(), "user_interrupted");
        assert_eq!(ReasonCode::ApprovalDenied.as_str(), "approval_denied");
        assert_eq!(ReasonCode::ToolFailure.as_str(), "tool_failure");
        assert_eq!(ReasonCode::ContextExhausted.as_str(), "context_exhausted");
        assert_eq!(
            ReasonCode::ProviderUnavailable.as_str(),
            "provider_unavailable"
        );
        assert_eq!(ReasonCode::Other.as_str(), "other");
    }

    #[test]
    fn reason_code_roundtrip_via_parse() {
        for code in [
            ReasonCode::SessionPoisoned,
            ReasonCode::TokenBudgetExceeded,
            ReasonCode::MaxIterations,
            ReasonCode::UserInterrupted,
            ReasonCode::ApprovalDenied,
            ReasonCode::ToolFailure,
            ReasonCode::ContextExhausted,
            ReasonCode::ProviderUnavailable,
            ReasonCode::Other,
        ] {
            assert_eq!(ReasonCode::parse(code.as_str()), Some(code));
        }
        assert_eq!(
            ReasonCode::parse("interrupted"),
            Some(ReasonCode::UserInterrupted)
        );
        assert_eq!(ReasonCode::parse("nonsense"), None);
    }

    #[test]
    fn serde_is_snake_case() {
        let json = serde_json::to_string(&ReasonCode::SessionPoisoned).expect("serialize");
        assert_eq!(json, "\"session_poisoned\"");
        let back: ReasonCode = serde_json::from_str("\"approval_denied\"").expect("deserialize");
        assert_eq!(back, ReasonCode::ApprovalDenied);
    }

    #[test]
    fn only_poisoned_and_context_imply_memory_loss() {
        assert!(ReasonCode::SessionPoisoned.implies_memory_loss());
        assert!(ReasonCode::ContextExhausted.implies_memory_loss());
        assert!(!ReasonCode::UserInterrupted.implies_memory_loss());
        assert!(!ReasonCode::ApprovalDenied.implies_memory_loss());
    }

    #[test]
    fn continuity_notice_sorts_and_labels() {
        let n = ContinuityNotice::new(
            ReasonCode::SessionPoisoned,
            vec![MemoryKind::Notes, MemoryKind::Todos],
        );
        // 按 ALL 稳定顺序:Todos 在 Notes 前。
        assert_eq!(n.lost, vec![MemoryKind::Todos, MemoryKind::Notes]);
        assert!(n.has_loss());
        assert!(n.message.contains("待办清单"), "{}", n.message);
        assert!(n.message.contains("临时笔记"), "{}", n.message);
        assert!(n.message.contains("session_poisoned"), "{}", n.message);
    }

    #[test]
    fn continuity_notice_no_loss_says_migrated() {
        let n = ContinuityNotice::new(ReasonCode::SessionPoisoned, vec![]);
        assert!(!n.has_loss());
        assert!(n.message.contains("均已迁移"), "{}", n.message);
    }

    #[test]
    fn continuity_notice_dedups() {
        let n = ContinuityNotice::new(
            ReasonCode::ContextExhausted,
            vec![MemoryKind::Goal, MemoryKind::Goal],
        );
        assert_eq!(n.lost, vec![MemoryKind::Goal]);
    }

    #[test]
    fn memory_kind_all_is_stable_order() {
        assert_eq!(
            MemoryKind::ALL.map(|k| k.as_str()),
            ["todos", "goal", "notes", "compaction_summary"]
        );
    }
}
