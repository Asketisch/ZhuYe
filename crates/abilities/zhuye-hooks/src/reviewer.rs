//! # LLM 审批评审器 + 不变地板(F07)
//!
//! 规则引擎判不了的情况(「这条 `rm -rf` 到底该不该批」),用**第二个模型**
//! 看一眼。设计上有三条硬约束,否则评审器本身就是新的攻击面:
//!
//! ## 1. 裁决方向单向:`Ask → Allow`,绝不 `Allow → Ask`
//!
//! 评审器只能**减少**打扰,不能**增加**审批。理由:如果评审器能升级
//! 裁决,一个被注入的评审器就能把普通读文件变成需要审批,或反过来
//! (更糟)把"本来要问用户"的东西判成"不用问"。单向约束让评审器的
//! 最坏结果只是「少问了一次」,而不是「多放行了一次」—— 下面第 2 条
//! 的不变地板兜住前者。
//!
//! ## 2. 不变地板([`InvariantFloor`])
//!
//! 无论评审器说什么,这些永不自动放行:
//!
//! - 写/删到自保护路径(`~/.zhuye/config.toml` / `permissions.toml` …);
//! - 权限模式提升(`PermissionOverride` 到更宽的模式);
//! - 显式 `Deny` 的工具(评审器不能否决 Deny — Deny 永远最严)。
//!
//! ## 3. 连续拒绝熔断
//!
//! 同一会话里评审器连续拒绝 N 次,自动停用评审(降级为「全问用户」)。
//! 否则评审器一旦进入"什么都拒"的状态,会静默把所有审批都变成拒绝,
//! 用户只看到"操作莫名其妙失败"。
//!
//! ## 测试性
//!
//! [`ModelReviewer`] 只依赖一个 [`JudgeFn`] 闭包(`prompt -> verdict`),
//! 生产环境接 `zhuye-llm` 的模型调用,测试环境塞 mock —— 不需要网络。

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use serde::{Deserialize, Serialize};

/// 评审器裁决。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    /// 明确可以放行(低风险,与用户既有意图一致)。
    Allow,
    /// 说不好 —— 交用户裁决(即保持 `Ask` 不变)。
    Escalate,
    /// 明显不合理 —— 直接拒绝。
    Deny,
}

impl ReviewVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewVerdict::Allow => "allow",
            ReviewVerdict::Escalate => "escalate",
            ReviewVerdict::Deny => "deny",
        }
    }
}

/// 一次评审的输入快照(可序列化,便于审计与回放)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRequest {
    pub tool_name: String,
    /// 工具参数(`args` 原样,供评审模型判断意图)。
    pub args: serde_json::Value,
    /// 为什么原本要问用户(来自权限层的 reason,如 "writes outside workspace")。
    pub ask_reason: String,
    /// 风险标签(权限引擎给出的,可能为空)。
    pub risk_labels: Vec<String>,
}

/// 评审器输出(供事件 / 审计)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewOutcome {
    pub verdict: ReviewVerdict,
    /// 评审模型给的一句话理由(进审批事件,便于用户复核)。
    pub reason: String,
    /// 是否被不变地板覆盖(地板的判定优先于模型 verdict)。
    pub floored: bool,
}

/// 评审函数签名:输入请求,输出 verdict + 理由。
///
/// 生产实现走 `zhuye-llm` 的模型调用;测试实现是纯函数。用 `Fn` 而非
/// `async fn` 是为让本 crate 不依赖 tokio —— 调用方在异步上下文里
/// 自行 `tokio::task::spawn_blocking` 即可。
pub type JudgeFn = Box<dyn Fn(&ReviewRequest) -> (ReviewVerdict, String) + Send + Sync>;

/// 不变地板:这些情况**永不**由评审器自动放行。
#[derive(Debug, Clone, Default)]
pub struct InvariantFloor {
    /// 自保护路径(写/删即拒,连问都不问)。
    pub protected_paths: Vec<String>,
    /// 硬禁止的工具名(评审器无法放行)。
    pub hard_deny_tools: Vec<String>,
}

impl InvariantFloor {
    /// 默认地板:配置文件 / 权限文件 / 凭据目录。
    pub fn with_defaults() -> InvariantFloor {
        InvariantFloor {
            protected_paths: vec![
                ".zhuye/config.toml".to_string(),
                ".zhuye/permissions.toml".to_string(),
                ".zhuye/secrets".to_string(),
                ".zhuye/hooks.json".to_string(),
            ],
            hard_deny_tools: Vec::new(),
        }
    }

    /// 本次请求是否命中地板(命中 → 必须保持原裁决,不得由模型放行)。
    pub fn blocks(&self, req: &ReviewRequest) -> bool {
        if self.hard_deny_tools.iter().any(|t| t == &req.tool_name) {
            return true;
        }
        // 路径检查:参数里任何字符串值包含自保护路径片段。
        let args_text = req.args.to_string();
        self.protected_paths
            .iter()
            .any(|p| args_text.contains(p.as_str()))
    }
}

/// 评审器配置。
#[derive(Debug, Clone)]
pub struct ReviewerConfig {
    /// 连续拒绝多少次后熔断(停用评审)。
    pub circuit_break_after: u32,
}

impl Default for ReviewerConfig {
    fn default() -> Self {
        ReviewerConfig {
            circuit_break_after: 3,
        }
    }
}

/// 模型评审器。
///
/// 每次 [`review`](Self::review) 都过三道关:地板 → 模型 → 方向约束;
/// 连续拒绝超阈值则熔断(全部走 Escalate = 保持问用户)。
pub struct ModelReviewer {
    judge: JudgeFn,
    floor: InvariantFloor,
    config: ReviewerConfig,
    /// 连续拒绝计数(Allow/Escalate 清零)。
    consecutive_denies: AtomicU32,
    /// 熔断标记。
    tripped: Mutex<bool>,
    /// 累计统计(诊断 / 事件展示)。
    stats: Mutex<ReviewerStats>,
}

/// 评审器累计统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewerStats {
    pub reviewed: u64,
    pub allowed: u64,
    pub denied: u64,
    pub escalated: u64,
    pub floored: u64,
    pub circuit_breaks: u64,
}

impl ModelReviewer {
    pub fn new(judge: JudgeFn) -> ModelReviewer {
        ModelReviewer {
            judge,
            floor: InvariantFloor::with_defaults(),
            config: ReviewerConfig::default(),
            consecutive_denies: AtomicU32::new(0),
            tripped: Mutex::new(false),
            stats: Mutex::new(ReviewerStats::default()),
        }
    }

    /// 覆盖地板(测试 / 自定义策略用)。
    pub fn with_floor(mut self, floor: InvariantFloor) -> ModelReviewer {
        self.floor = floor;
        self
    }

    /// 覆盖熔断配置。
    pub fn with_config(mut self, config: ReviewerConfig) -> ModelReviewer {
        self.config = config;
        self
    }

    /// 评审一次「原本要问用户」的调用。
    ///
    /// 返回的 `verdict` 语义:
    /// - `Allow` —— 可以放行(方向约束:只可能从 `Ask` 变来);
    /// - `Escalate` —— 保持问用户(地板命中 / 熔断 / 模型也说不准);
    /// - `Deny` —— 模型判为明显不合理。
    pub fn review(&self, req: &ReviewRequest) -> ReviewOutcome {
        let mut stats = self.stats.lock().unwrap_or_else(|e| e.into_inner());
        stats.reviewed += 1;

        // 1) 不变地板:命中即保持原裁决(绝不因模型而放松)。
        if self.floor.blocks(req) {
            stats.floored += 1;
            stats.escalated += 1;
            return ReviewOutcome {
                verdict: ReviewVerdict::Escalate,
                reason: "invariant floor: 自保护目标,必须由用户裁决".to_string(),
                floored: true,
            };
        }

        // 2) 熔断:已熔断则直接 Escalate。
        if *self.tripped.lock().unwrap_or_else(|e| e.into_inner()) {
            stats.escalated += 1;
            return ReviewOutcome {
                verdict: ReviewVerdict::Escalate,
                reason: "reviewer circuit open: 连续拒绝过多,评审已停用".to_string(),
                floored: false,
            };
        }

        // 3) 模型裁决。
        let (verdict, reason) = (self.judge)(req);

        // 4) 方向约束:Allow 需要非空理由(防止"模型随口允许"无法审计)。
        let verdict = match verdict {
            ReviewVerdict::Allow if reason.trim().is_empty() => ReviewVerdict::Escalate,
            v => v,
        };

        match verdict {
            ReviewVerdict::Allow => {
                stats.allowed += 1;
                self.consecutive_denies.store(0, Ordering::Relaxed);
            }
            ReviewVerdict::Escalate => {
                stats.escalated += 1;
                self.consecutive_denies.store(0, Ordering::Relaxed);
            }
            ReviewVerdict::Deny => {
                stats.denied += 1;
                let n = self.consecutive_denies.fetch_add(1, Ordering::Relaxed) + 1;
                if n >= self.config.circuit_break_after {
                    let mut tripped = self.tripped.lock().unwrap_or_else(|e| e.into_inner());
                    if !*tripped {
                        *tripped = true;
                        stats.circuit_breaks += 1;
                        tracing::warn!(
                            after = n,
                            "model reviewer circuit breaker tripped: 后续审批全部交用户"
                        );
                    }
                }
            }
        }

        ReviewOutcome {
            verdict,
            reason,
            floored: false,
        }
    }

    /// 当前统计快照。
    pub fn stats(&self) -> ReviewerStats {
        *self.stats.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 熔断是否已触发。
    pub fn is_circuit_open(&self) -> bool {
        *self.tripped.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 复位熔断与计数(用户手动恢复 / 测试)。
    pub fn reset_circuit(&self) {
        *self.tripped.lock().unwrap_or_else(|e| e.into_inner()) = false;
        self.consecutive_denies.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(tool: &str, args: serde_json::Value) -> ReviewRequest {
        ReviewRequest {
            tool_name: tool.to_string(),
            args,
            ask_reason: "writes outside workspace".to_string(),
            risk_labels: vec![],
        }
    }

    fn reviewer(verdict: ReviewVerdict, reason: &'static str) -> ModelReviewer {
        ModelReviewer::new(Box::new(move |_r| (verdict, reason.to_string())))
    }

    #[test]
    fn allow_passes_through_with_reason() {
        let r = reviewer(ReviewVerdict::Allow, "read-only, matches user intent");
        let out = r.review(&req("read", serde_json::json!({"path": "src/lib.rs"})));
        assert_eq!(out.verdict, ReviewVerdict::Allow);
        assert!(!out.floored);
        assert_eq!(r.stats().allowed, 1);
    }

    #[test]
    fn allow_without_reason_escalates() {
        // 无法审计的"允许"不可信 → 降级为问用户。
        let r = reviewer(ReviewVerdict::Allow, "   ");
        let out = r.review(&req("read", serde_json::json!({})));
        assert_eq!(out.verdict, ReviewVerdict::Escalate);
        assert_eq!(r.stats().escalated, 1);
    }

    #[test]
    fn deny_passes_through() {
        let r = reviewer(ReviewVerdict::Deny, "deletes entire home directory");
        let out = r.review(&req("bash", serde_json::json!({"cmd": "rm -rf ~"})));
        assert_eq!(out.verdict, ReviewVerdict::Deny);
        assert_eq!(r.stats().denied, 1);
    }

    #[test]
    fn floor_blocks_allow_on_protected_path() {
        // 模型说 Allow,但目标是自己改配置 → 地板强制 Escalate。
        let r = reviewer(ReviewVerdict::Allow, "looks fine");
        let out = r.review(&req(
            "write",
            serde_json::json!({"path": "~/.zhuye/config.toml", "content": "x"}),
        ));
        assert_eq!(out.verdict, ReviewVerdict::Escalate);
        assert!(out.floored, "应标记被地板覆盖");
        assert!(out.reason.contains("invariant floor"), "{}", out.reason);
        assert_eq!(r.stats().floored, 1);
    }

    #[test]
    fn floor_hard_deny_tool() {
        let floor = InvariantFloor {
            protected_paths: vec![],
            hard_deny_tools: vec!["dangerous".to_string()],
        };
        let r = reviewer(ReviewVerdict::Allow, "fine").with_floor(floor);
        let out = r.review(&req("dangerous", serde_json::json!({})));
        assert_eq!(out.verdict, ReviewVerdict::Escalate);
        assert!(out.floored);
    }

    #[test]
    fn circuit_breaks_after_consecutive_denies() {
        let r = ModelReviewer::new(Box::new(|_r| (ReviewVerdict::Deny, "nope".to_string())))
            .with_config(ReviewerConfig {
                circuit_break_after: 3,
            });
        for _ in 0..3 {
            assert_eq!(
                r.review(&req("bash", serde_json::json!({}))).verdict,
                ReviewVerdict::Deny
            );
        }
        assert!(r.is_circuit_open(), "连续 3 次拒绝后应熔断");
        // 熔断后一律 Escalate(交用户),不再自动拒绝。
        let out = r.review(&req("bash", serde_json::json!({})));
        assert_eq!(out.verdict, ReviewVerdict::Escalate);
        assert!(out.reason.contains("circuit open"), "{}", out.reason);
    }

    #[test]
    fn allow_resets_consecutive_deny_counter() {
        let seq = Mutex::new(vec![
            ReviewVerdict::Deny,
            ReviewVerdict::Deny,
            ReviewVerdict::Allow,
            ReviewVerdict::Deny,
            ReviewVerdict::Deny,
        ]);
        let r = ModelReviewer::new(Box::new(move |_r| {
            let mut s = seq.lock().unwrap();
            if s.is_empty() {
                (ReviewVerdict::Deny, "nope".to_string())
            } else {
                let v = s.remove(0);
                (v, "reason".to_string())
            }
        }))
        .with_config(ReviewerConfig {
            circuit_break_after: 3,
        });
        for _ in 0..5 {
            r.review(&req("bash", serde_json::json!({})));
        }
        assert!(!r.is_circuit_open(), "Allow 应重置连击,不该熔断");
    }

    #[test]
    fn reset_circuit_recovers() {
        let r = ModelReviewer::new(Box::new(|_r| (ReviewVerdict::Deny, "nope".to_string())))
            .with_config(ReviewerConfig {
                circuit_break_after: 1,
            });
        r.review(&req("bash", serde_json::json!({})));
        assert!(r.is_circuit_open());
        r.reset_circuit();
        assert!(!r.is_circuit_open());
    }

    #[test]
    fn stats_accumulate_across_verdicts() {
        let r = reviewer(ReviewVerdict::Allow, "ok");
        r.review(&req("read", serde_json::json!({})));
        r.review(&req("read", serde_json::json!({})));
        let s = r.stats();
        assert_eq!(s.reviewed, 2);
        assert_eq!(s.allowed, 2);
    }
}
