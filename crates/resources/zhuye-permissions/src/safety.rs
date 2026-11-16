//! # 安全杂项:live policy 热切换 / 动态扩权 / 子代理只读(F12)
//!
//! 三件小而硬的能力,共同点是**运行时状态**而非静态规则:
//!
//! 1. [`LivePolicy`] —— 安全策略改动**立即生效**,不需要重启进程。
//!    实现方式是「generation 计数 + 可替换配置」:每次换策略
//!    `generation` +1,读取方比对自己看到的 generation,过旧就重读。
//!    这比"直接换 Arc"好在可观测(UI 能显示"策略第 N 版"),也比
//!    "每次读都加锁"便宜(读多写极少)。
//!
//! 2. [`EscalationTicket`] —— **一次性提权**:某个操作被拒后,用户
//!    批准"就这一次",票据只对**特定工具 + 特定参数哈希**有效,用完
//!    即失效(不可重放)。
//!
//! 3. [`SubagentReadonly`] —— 子代理可被强制降级为只读:即使父会话
//!    是 `Auto`,子代理也只能读不能写。防止"并行 8 个子代理同时改文件"
//!    这类失控。
//!
//! ## 为什么放在 permissions 而非 tools
//!
//! 这三项都是**权限策略**的一部分,消费方是权限引擎与审批 gate;放在
//! tools 会让权限层反向依赖工具层。本模块只依赖 `std`,可被任意层引用。

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// 带版本号的热切换策略容器。
///
/// `T` 是策略本体(例如 `PolicyConfig`)。每次 [`swap`](Self::swap) 换入
/// 新值并递增 generation;读取方可用 [`generation`](Self::generation)
/// 判断自己缓存的那份是否过期。
#[derive(Debug)]
pub struct LivePolicy<T> {
    inner: Mutex<T>,
    generation: AtomicU64,
}

impl<T: Clone> LivePolicy<T> {
    pub fn new(initial: T) -> LivePolicy<T> {
        LivePolicy {
            inner: Mutex::new(initial),
            generation: AtomicU64::new(1),
        }
    }

    /// 当前策略快照。
    pub fn snapshot(&self) -> T {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 换入新策略,返回新 generation。
    ///
    /// **立即生效**:下一次 `snapshot()` 就拿到新值 —— 不经过重启或
    /// 重连。这是 F12「安全策略改动立即生效」的实现点。
    pub fn swap(&self, next: T) -> u64 {
        {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            *guard = next;
        }
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// 当前 generation。0 保留为「从未初始化」。
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
}

/// 一次性提权票据。
///
/// 绑定「工具名 + 参数规范化哈希」,`consume` 消费后失效 —— 用户批准
/// 的「就这一次」不会被重放(audit / 回放场景下尤其重要)。
#[derive(Debug, Default)]
pub struct EscalationTickets {
    /// key = `tool\0args_hash`;值 = 剩余可用次数。
    ///
    /// 用计数而非 bool:同一操作在一次 turn 内被重试 2 次是常见的
    /// (瞬时失败重试),票据按次扣减更符合用户"就这一次"的直觉。
    tickets: Mutex<HashMap<String, u32>>,
}

impl EscalationTickets {
    pub fn new() -> EscalationTickets {
        EscalationTickets::default()
    }

    fn key(tool: &str, args_hash: &str) -> String {
        format!("{tool}\0{args_hash}")
    }

    /// 发放一张票据(默认可用 1 次)。
    pub fn issue(&self, tool: &str, args_hash: &str) {
        self.issue_uses(tool, args_hash, 1);
    }

    /// 发放可用 `uses` 次的票据。
    pub fn issue_uses(&self, tool: &str, args_hash: &str, uses: u32) {
        if uses == 0 {
            return;
        }
        let mut map = self.tickets.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(Self::key(tool, args_hash)).or_insert(0);
        *entry = entry.saturating_add(uses);
    }

    /// 消费一张票据。返回 `true` = 本次提权有效(并已扣减)。
    pub fn consume(&self, tool: &str, args_hash: &str) -> bool {
        let mut map = self.tickets.lock().unwrap_or_else(|e| e.into_inner());
        let k = Self::key(tool, args_hash);
        match map.get_mut(&k) {
            Some(n) if *n > 0 => {
                *n -= 1;
                let exhausted = *n == 0;
                if exhausted {
                    map.remove(&k);
                }
                true
            }
            _ => false,
        }
    }

    /// 剩余票据数(诊断用)。
    pub fn outstanding(&self) -> usize {
        self.tickets.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// 清空(会话结束 / 测试)。
    pub fn clear(&self) {
        self.tickets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

/// 子代理只读降级策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentReadonly {
    /// 不降级,子代理与父会话同权(默认)。
    #[default]
    Off,
    /// 全部子代理强制只读。
    Always,
    /// 仅写类工具(名含 write/edit/delete/patch)被拒绝,读/搜索照常。
    WritesOnly,
}

impl SubagentReadonly {
    pub fn parse(s: &str) -> Option<SubagentReadonly> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "0" => Some(SubagentReadonly::Off),
            "always" | "on" | "true" | "1" => Some(SubagentReadonly::Always),
            "writes_only" | "writes" => Some(SubagentReadonly::WritesOnly),
            _ => None,
        }
    }

    /// 该工具在只读策略下是否应被拒绝。
    pub fn blocks(self, tool_name: &str) -> bool {
        match self {
            SubagentReadonly::Off => false,
            SubagentReadonly::Always => true,
            SubagentReadonly::WritesOnly => {
                let lower = tool_name.to_ascii_lowercase();
                ["write", "edit", "delete", "patch", "create", "rm"]
                    .iter()
                    .any(|needle| lower.contains(needle))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    struct FakePolicy {
        deny_all: bool,
    }

    #[test]
    fn live_policy_swap_bumps_generation_and_applies_immediately() {
        let p = LivePolicy::new(FakePolicy { deny_all: false });
        assert_eq!(p.generation(), 1);
        assert!(!p.snapshot().deny_all);
        let generation = p.swap(FakePolicy { deny_all: true });
        assert_eq!(generation, 2);
        assert_eq!(p.generation(), 2);
        assert!(p.snapshot().deny_all, "换入后必须立即生效");
    }

    #[test]
    fn live_policy_multiple_swaps_increment() {
        let p = LivePolicy::new(1);
        assert_eq!(p.swap(2), 2);
        assert_eq!(p.swap(3), 3);
        assert_eq!(p.snapshot(), 3);
    }

    #[test]
    fn escalation_ticket_is_single_use() {
        let t = EscalationTickets::new();
        t.issue("bash", "h1");
        assert!(t.consume("bash", "h1"), "首次应有效");
        assert!(!t.consume("bash", "h1"), "用完即失效(不可重放)");
        assert_eq!(t.outstanding(), 0);
    }

    #[test]
    fn escalation_ticket_binds_tool_and_args_hash() {
        let t = EscalationTickets::new();
        t.issue("bash", "h1");
        assert!(!t.consume("bash", "h2"), "参数哈希不同不共享票据");
        assert!(!t.consume("write", "h1"), "工具不同不共享票据");
        assert!(t.consume("bash", "h1"));
    }

    #[test]
    fn escalation_ticket_multi_use() {
        let t = EscalationTickets::new();
        t.issue_uses("bash", "h", 2);
        assert!(t.consume("bash", "h"));
        assert!(t.consume("bash", "h"));
        assert!(!t.consume("bash", "h"));
    }

    #[test]
    fn escalation_clear() {
        let t = EscalationTickets::new();
        t.issue("bash", "h1");
        t.issue("read", "h2");
        assert_eq!(t.outstanding(), 2);
        t.clear();
        assert_eq!(t.outstanding(), 0);
        assert!(!t.consume("bash", "h1"));
    }

    #[test]
    fn subagent_readonly_modes() {
        assert!(!SubagentReadonly::Off.blocks("write"));
        assert!(SubagentReadonly::Always.blocks("read"));
        assert!(SubagentReadonly::Always.blocks("write"));
        assert!(SubagentReadonly::WritesOnly.blocks("write_file"));
        assert!(SubagentReadonly::WritesOnly.blocks("edit"));
        assert!(!SubagentReadonly::WritesOnly.blocks("read"));
        assert!(!SubagentReadonly::WritesOnly.blocks("grep"));
    }

    #[test]
    fn subagent_readonly_parse() {
        assert_eq!(
            SubagentReadonly::parse("always"),
            Some(SubagentReadonly::Always)
        );
        assert_eq!(
            SubagentReadonly::parse("writes_only"),
            Some(SubagentReadonly::WritesOnly)
        );
        assert_eq!(SubagentReadonly::parse("off"), Some(SubagentReadonly::Off));
        assert_eq!(SubagentReadonly::parse("bogus"), None);
    }
}
