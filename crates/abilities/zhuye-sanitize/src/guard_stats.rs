//! # prompt_guard 裁决统计(F01)
//!
//! [`GuardStats`] 是一个四计数器原子快照结构,由 `ToolRegistry`
//! (工具定义侧扫描)与 `ToolExecutionQueue`(工具输出侧扫描)共享
//! 同一实例递增,供 `/status` / 设置页展示「本次会话 guard 拦了什么」。
//!
//! 设计约束:
//!
//! - **只增不减**:计数器只在本会话生命周期内递增,不做复位 ——
//!   统计语义是"累计发生了多少次拦截/标注",不是实时队列长度。
//! - **无锁**:四个 `AtomicU64`(`Relaxed` 序,纯计数无跨字段不变量,
//!   快照允许字段间极短暂不一致,展示层无感)。
//! - **零依赖**:叶 crate 内自足,`zhuye-core`(AgentConfig)与
//!   `zhuye-tools`(registry/queue)都能持有,不需要互相依赖。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// 四类裁决计数的只读快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct GuardStatsSnapshot {
    /// 工具定义(name/description)扫描命中 Block、注册被拒的次数。
    pub definitions_blocked: u64,
    /// 工具定义扫描命中 Review、放行但标注的次数。
    pub definitions_reviewed: u64,
    /// 工具输出扫描命中 Block、内容被替换拦截的次数。
    pub outputs_blocked: u64,
    /// 工具输出扫描命中 Review、metadata 标注的次数。
    pub outputs_reviewed: u64,
}

impl GuardStatsSnapshot {
    /// 是否发生过任何拦截/标注(全零 = guard 开着但从未命中)。
    pub fn any_hit(&self) -> bool {
        self.definitions_blocked > 0
            || self.definitions_reviewed > 0
            || self.outputs_blocked > 0
            || self.outputs_reviewed > 0
    }
}

/// 会话级 prompt_guard 裁决计数器。
///
/// 典型接线:`bootstrap_common` 建一个 `Arc<GuardStats>`,同时挂到
/// `ToolRegistry::set_prompt_guard` 与 `AgentConfig.guard_stats`(队列
/// 侧经 `AgentThread` 注入)。guard 关闭时不挂,快照查询返回 `None`
/// 区分「guard 未启用」与「启用但零命中」。
#[derive(Debug, Default)]
pub struct GuardStats {
    definitions_blocked: AtomicU64,
    definitions_reviewed: AtomicU64,
    outputs_blocked: AtomicU64,
    outputs_reviewed: AtomicU64,
}

impl GuardStats {
    pub fn new() -> GuardStats {
        GuardStats::default()
    }

    /// 工具定义 Block(注册被拒)+1。
    pub fn record_definition_blocked(&self) {
        self.definitions_blocked.fetch_add(1, Ordering::Relaxed);
    }

    /// 工具定义 Review(放行 + 标注)+1。
    pub fn record_definition_reviewed(&self) {
        self.definitions_reviewed.fetch_add(1, Ordering::Relaxed);
    }

    /// 工具输出 Block(内容被拦截替换)+1。
    pub fn record_output_blocked(&self) {
        self.outputs_blocked.fetch_add(1, Ordering::Relaxed);
    }

    /// 工具输出 Review(metadata 标注)+1。
    pub fn record_output_reviewed(&self) {
        self.outputs_reviewed.fetch_add(1, Ordering::Relaxed);
    }

    /// 当前累计快照。
    pub fn snapshot(&self) -> GuardStatsSnapshot {
        GuardStatsSnapshot {
            definitions_blocked: self.definitions_blocked.load(Ordering::Relaxed),
            definitions_reviewed: self.definitions_reviewed.load(Ordering::Relaxed),
            outputs_blocked: self.outputs_blocked.load(Ordering::Relaxed),
            outputs_reviewed: self.outputs_reviewed.load(Ordering::Relaxed),
        }
    }
}

/// 便捷别名:各持有方都以 `Arc` 共享同一实例。
pub type SharedGuardStats = Arc<GuardStats>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_increment_independently() {
        let stats = GuardStats::new();
        stats.record_definition_blocked();
        stats.record_definition_blocked();
        stats.record_definition_reviewed();
        stats.record_output_blocked();
        stats.record_output_reviewed();
        stats.record_output_reviewed();
        stats.record_output_reviewed();

        let snap = stats.snapshot();
        assert_eq!(
            snap,
            GuardStatsSnapshot {
                definitions_blocked: 2,
                definitions_reviewed: 1,
                outputs_blocked: 1,
                outputs_reviewed: 3,
            }
        );
        assert!(snap.any_hit());
    }

    #[test]
    fn zero_snapshot_means_no_hit() {
        let stats = GuardStats::new();
        assert!(!stats.snapshot().any_hit());
        assert_eq!(stats.snapshot(), GuardStatsSnapshot::default());
    }

    #[test]
    fn shared_arc_is_same_instance() {
        let shared: SharedGuardStats = SharedGuardStats::new(GuardStats::new());
        let clone = Arc::clone(&shared);
        clone.record_output_blocked();
        assert_eq!(shared.snapshot().outputs_blocked, 1);
    }

    #[test]
    fn snapshot_serializable() {
        let snap = GuardStatsSnapshot {
            definitions_blocked: 1,
            definitions_reviewed: 2,
            outputs_blocked: 3,
            outputs_reviewed: 4,
        };
        let json = serde_json::to_string(&snap).expect("serialize");
        assert!(json.contains("\"definitions_blocked\":1"));
        let back: GuardStatsSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, snap);
    }
}
