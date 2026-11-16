//! # 决策级审计(F09)
//!
//! 普通审计只记「谁在何时做了什么」(subject + outcome)。决策级审计多记
//! 两样:**为什么这么判**,以及**否决了哪些其他可能**。
//!
//! 为什么这很重要:出事后复盘"为什么这条 `rm -rf` 被放行了",只看
//! outcome 是答不出来的。有了 `reasoning` + `alternatives`,能回答:
//!
//! - 这条走了哪条规则(规则 id / 来源层);
//! - 有哪些更严的裁决被评估过、为什么没选(例如"本应 Deny 但因为
//!   用户在 session 白名单里,降级为 Allow");
//! - 风险标签是什么。
//!
//! ## 与 AuditEntry 的关系
//!
//! [`DecisionAudit`] 是 `AuditEntry` 的**补充载荷** —— 通过
//! [`DecisionAudit::to_detail`] 生成 `detail` JSON,直接塞进
//! `AuditEntry.detail`,复用既有的递归脱敏 / 指纹 / 同事务提交能力。
//! 这样不需要改 `zhuye-audit` 的写盘路径,也不会让 detail 形状失控。

use serde::{Deserialize, Serialize};

/// 一个被否决的备选裁决。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedAlternative {
    /// 备选裁决(如 `"deny"` / `"ask"`)。
    pub action: String,
    /// 为什么没选它(规则 id / 优先级说明)。
    pub why_not: String,
}

/// 决策级审计载荷。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionAudit {
    /// 最终裁决(`"allow"` / `"ask"` / `"deny"`)。
    pub decision: String,
    /// 人读理由(权限引擎的 `DecisionReason` 文本)。
    pub reasoning: String,
    /// 命中的规则 id(可能为空 —— 缺省裁决没有规则)。
    pub matched_rule: Option<String>,
    /// 风险标签(权限引擎给出)。
    pub risk_labels: Vec<String>,
    /// 被否决的备选(按评估顺序)。
    pub alternatives: Vec<RejectedAlternative>,
    /// 裁决来源(`"user"` / `"policy"` / `"reviewer"` / `"session_allow"` / …)。
    pub decided_by: String,
}

impl DecisionAudit {
    /// 构造:最小必需字段 + 默认空的可选字段。
    pub fn new(
        decision: impl Into<String>,
        reasoning: impl Into<String>,
        decided_by: impl Into<String>,
    ) -> DecisionAudit {
        DecisionAudit {
            decision: decision.into(),
            reasoning: reasoning.into(),
            matched_rule: None,
            risk_labels: Vec::new(),
            alternatives: Vec::new(),
            decided_by: decided_by.into(),
        }
    }

    pub fn with_rule(mut self, rule: impl Into<String>) -> DecisionAudit {
        self.matched_rule = Some(rule.into());
        self
    }

    pub fn with_risk_labels(mut self, labels: Vec<String>) -> DecisionAudit {
        self.risk_labels = labels;
        self
    }

    pub fn reject(
        mut self,
        action: impl Into<String>,
        why_not: impl Into<String>,
    ) -> DecisionAudit {
        self.alternatives.push(RejectedAlternative {
            action: action.into(),
            why_not: why_not.into(),
        });
        self
    }

    /// 渲染为 `AuditEntry.detail` 的 JSON。
    ///
    /// 形状固定为 `{ "decision_audit": { … } }`,给未来的字段扩展留位置
    /// (detail 里可以再塞别的键,不冲突)。
    pub fn to_detail(&self) -> serde_json::Value {
        serde_json::json!({ "decision_audit": self })
    }

    /// 一条人读摘要(日志 / UI 一行用)。
    pub fn summary(&self) -> String {
        let mut out = format!(
            "{} (by {}) · {}",
            self.decision, self.decided_by, self.reasoning
        );
        if let Some(rule) = &self.matched_rule {
            out.push_str(&format!(" · rule={rule}"));
        }
        if !self.risk_labels.is_empty() {
            out.push_str(&format!(" · risk={}", self.risk_labels.join(",")));
        }
        if !self.alternatives.is_empty() {
            let alts = self
                .alternatives
                .iter()
                .map(|a| format!("{}({})", a.action, a.why_not))
                .collect::<Vec<_>>()
                .join(" | ");
            out.push_str(&format!(" · rejected: {alts}"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_decision_audit() {
        let d = DecisionAudit::new("allow", "read-only command", "policy");
        assert_eq!(d.decision, "allow");
        assert!(d.matched_rule.is_none());
        assert!(d.alternatives.is_empty());
        assert!(d.summary().contains("allow"));
        assert!(d.summary().contains("read-only command"));
    }

    #[test]
    fn full_decision_audit_with_alternatives() {
        let d = DecisionAudit::new("allow", "user approved for session", "session_allow")
            .with_rule("session-allow:bash:git-status")
            .with_risk_labels(vec!["medium".to_string()])
            .reject("deny", "not in hard-deny set")
            .reject("ask", "user already approved this command in session");
        assert_eq!(
            d.matched_rule.as_deref(),
            Some("session-allow:bash:git-status")
        );
        assert_eq!(d.alternatives.len(), 2);
        let summary = d.summary();
        assert!(summary.contains("rule=session-allow"), "{summary}");
        assert!(summary.contains("risk=medium"), "{summary}");
        assert!(summary.contains("rejected: deny"), "{summary}");
        assert!(summary.contains("ask("), "{summary}");
    }

    #[test]
    fn detail_wraps_under_decision_audit_key() {
        let d = DecisionAudit::new("deny", "writes protected config", "policy")
            .reject("allow", "self-protection floor");
        let detail = d.to_detail();
        assert!(detail.get("decision_audit").is_some());
        assert_eq!(detail["decision_audit"]["decision"], "deny");
        assert_eq!(detail["decision_audit"]["decided_by"], "policy");
        assert_eq!(
            detail["decision_audit"]["alternatives"][0]["action"],
            "allow"
        );
    }

    #[test]
    fn decision_audit_roundtrips_json() {
        let d = DecisionAudit::new("ask", "outside workspace", "policy")
            .with_rule("workspace-boundary")
            .reject("allow", "not in allowlist");
        let json = serde_json::to_string(&d).expect("serialize");
        let back: DecisionAudit = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, d);
    }

    #[test]
    fn rejected_alternative_roundtrip() {
        let a = RejectedAlternative {
            action: "deny".into(),
            why_not: "no deny rule matched".into(),
        };
        let json = serde_json::to_string(&a).expect("serialize");
        let back: RejectedAlternative = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, a);
    }
}
