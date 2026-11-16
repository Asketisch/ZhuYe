//! # Provenance 分类与信息流 taint 追踪(F03)
//!
//! 回答一个 F01 的 prompt_guard 回答不了的问题:**这段文本从哪来**。
//!
//! - [`Provenance`] —— 单条内容的来源等级(用户手输 / 工作区文件 /
//!   工具输出 / 外部网络)。
//! - [`TaintTracker`] —— 会话级标记:一旦某次工具输出被判为**不可信
//!   且含注入迹象**,就把来源打上 taint,后续来自同源的输出升级处理。
//!
//! ## 为什么需要
//!
//! prompt_guard 是**逐条**扫描:一条干净但被"上下文投毒"的输出看不出来。
//! taint 是**跨条**的:同一 web 域 / 同一 MCP server 已经出现过注入,
//! 它后续的"看起来很干净"的输出也应降级信任 —— 这是 agno / openai-agents
//! 的 tripwire 思路在本地可闭环的部分。
//!
//! ## 三档
//!
//! [`ProvenanceMode`] 与 [`super::pii::PiiMode`] 同构(`Off` / `Observe` /
//! `Enforce`):`Observe` 只标注不改变裁决;`Enforce` 让 tainted 来源的
//! Review 升级为 Block。

use std::collections::HashSet;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// 内容来源等级。数字越大越不可信。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// 用户亲手敲的直接指令(权限层负责,不归 guard 管)。
    #[default]
    UserDirect,
    /// 工作区内文件内容(本地可信度较高,但可能被 clone 的仓库污染)。
    Workspace,
    /// 工具输出(bash 结果、read 内容等)。
    ToolOutput,
    /// 外部网络内容(web_fetch / MCP 远程 server / 第三方 API)。
    External,
}

impl Provenance {
    /// 是否属于「不可信」侧(ToolOutput / External)。
    pub fn is_untrusted(self) -> bool {
        matches!(self, Provenance::ToolOutput | Provenance::External)
    }

    /// 人读标签(审计 / 事件字段用)。
    pub fn label(self) -> &'static str {
        match self {
            Provenance::UserDirect => "user_direct",
            Provenance::Workspace => "workspace",
            Provenance::ToolOutput => "tool_output",
            Provenance::External => "external",
        }
    }
}

/// provenance 处理档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceMode {
    /// 不追踪(只记录 provenance 标签)。
    Off,
    /// 追踪 taint 并标注,但不改变裁决(默认)。
    #[default]
    Observe,
    /// tainted 来源的 Review 升级为 Block。
    Enforce,
}

impl ProvenanceMode {
    pub fn parse(s: &str) -> Option<ProvenanceMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "0" => Some(ProvenanceMode::Off),
            "observe" => Some(ProvenanceMode::Observe),
            "enforce" | "on" | "true" | "1" => Some(ProvenanceMode::Enforce),
            _ => None,
        }
    }

    /// 是否追踪 taint。
    pub fn tracks(self) -> bool {
        !matches!(self, ProvenanceMode::Off)
    }

    /// 是否升级裁决。
    pub fn escalates(self) -> bool {
        matches!(self, ProvenanceMode::Enforce)
    }
}

/// 会话级 taint 追踪器。
///
/// 以「来源键」(web 域名 / MCP server 名 / 工具名)为粒度:某键被标记后,
/// 该键的后续输出都视为 tainted。`Mutex<HashSet>` 足够 —— 写入极稀疏
/// (只有命中注入时才写),读侧一次 `contains`。
#[derive(Debug, Default)]
pub struct TaintTracker {
    tainted: Mutex<HashSet<String>>,
    /// 累计标记次数(诊断 / 统计)。
    marks: Mutex<u64>,
}

impl TaintTracker {
    pub fn new() -> TaintTracker {
        TaintTracker::default()
    }

    /// 把来源键标为 tainted。返回是否**新**标记(幂等)。
    pub fn mark(&self, source_key: &str) -> bool {
        let mut set = self.tainted.lock().unwrap_or_else(|e| e.into_inner());
        let inserted = set.insert(source_key.to_string());
        if inserted {
            let mut marks = self.marks.lock().unwrap_or_else(|e| e.into_inner());
            *marks += 1;
        }
        inserted
    }

    /// 该来源是否已 tainted。
    pub fn is_tainted(&self, source_key: &str) -> bool {
        self.tainted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(source_key)
    }

    /// 当前已标记的来源键(排序后,供 UI / 审计展示)。
    pub fn tainted_sources(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .tainted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        out.sort();
        out
    }

    /// 累计标记次数。
    pub fn mark_count(&self) -> u64 {
        *self.marks.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 清空(会话退役 / 测试用)。
    pub fn clear(&self) {
        self.tainted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        *self.marks.lock().unwrap_or_else(|e| e.into_inner()) = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_ordering() {
        assert!(Provenance::External > Provenance::ToolOutput);
        assert!(Provenance::ToolOutput > Provenance::Workspace);
        assert!(Provenance::Workspace > Provenance::UserDirect);
        assert!(!Provenance::UserDirect.is_untrusted());
        assert!(!Provenance::Workspace.is_untrusted());
        assert!(Provenance::ToolOutput.is_untrusted());
        assert!(Provenance::External.is_untrusted());
    }

    #[test]
    fn mode_semantics() {
        assert!(ProvenanceMode::Observe.tracks());
        assert!(!ProvenanceMode::Observe.escalates());
        assert!(ProvenanceMode::Enforce.escalates());
        assert!(!ProvenanceMode::Off.tracks());
        assert_eq!(
            ProvenanceMode::parse("enforce"),
            Some(ProvenanceMode::Enforce)
        );
    }

    #[test]
    fn taint_mark_is_idempotent() {
        let t = TaintTracker::new();
        assert!(t.mark("web:evil.example"));
        assert!(!t.mark("web:evil.example"), "重复标记不应计数");
        assert_eq!(t.mark_count(), 1);
        assert!(t.is_tainted("web:evil.example"));
        assert!(!t.is_tainted("web:good.example"));
    }

    #[test]
    fn tainted_sources_sorted_and_clearable() {
        let t = TaintTracker::new();
        t.mark("mcp:server-b");
        t.mark("mcp:server-a");
        assert_eq!(
            t.tainted_sources(),
            vec!["mcp:server-a".to_string(), "mcp:server-b".to_string()]
        );
        t.clear();
        assert!(t.tainted_sources().is_empty());
        assert_eq!(t.mark_count(), 0);
    }
}
