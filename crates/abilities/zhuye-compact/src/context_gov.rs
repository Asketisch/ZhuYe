//! # 上下文治理:可搜索溢出句柄 + 输出预算自适应(F26)
//!
//! 长会话不爆的两件事:
//!
//! 1. **溢出存盘 + 可搜索句柄**([`SpillStore`])—— 超大工具输出不截断,
//!    而是存盘并给模型一个**可检索的回执条**。模型需要细节时用
//!    [`SpillStore::search`] 查回相关行,而不是让原始 100KB 占满上下文。
//!    截断会丢信息;句柄保留了取回能力。
//! 2. **输出上限自适应**([`adaptive_output_cap`])—— 上下文快满时收紧
//!    单次输出上限,而不是等爆了再压缩。
//!
//! ## 为什么「可检索」比「截断」好
//!
//! 截断后模型只知道"输出很长",无法取回需要的那部分 —— 它会重跑命令,
//! 更浪费。句柄 + 检索让模型能定向取回(「给我含 ERROR 的行」),一次
//! 命中就够。
//!
//! ## 与压缩的关系
//!
//! 压缩是**事后**减量(已进上下文的内容被摘要);spill 是**事前**减量
//! (大输出压根不整段进上下文)。两者互补:spill 挡住新的,压缩清理旧的。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// 一条溢出记录的回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpillHandle {
    /// 句柄 id(模型用它来检索/取回)。
    pub id: String,
    /// 原始字节数。
    pub bytes: usize,
    /// 总行数。
    pub lines: usize,
    /// 回执文本(进模型上下文,替代原始输出)。
    pub receipt: String,
}

impl SpillHandle {
    /// 渲染成可读回执(含检索提示)。
    pub fn render_receipt(id: &str, bytes: usize, lines: usize) -> String {
        format!(
            "[输出已溢出存盘 · id={id} · {bytes} 字节 / {lines} 行]\n\
             原始内容不在上下文中。需要细节时用 `ctx_search(id=\"{id}\", query=\"...\")` \
             检索相关行,或用 `ctx_resume(id=\"{id}\", from_line=N, count=M)` 取回片段。"
        )
    }
}

/// 溢出存储。
#[derive(Debug, Default)]
pub struct SpillStore {
    entries: HashMap<String, SpilledEntry>,
    next_id: usize,
}

#[derive(Debug, Clone)]
struct SpilledEntry {
    lines: Vec<String>,
}

impl SpillStore {
    pub fn new() -> SpillStore {
        SpillStore::default()
    }

    /// 存入一份超大输出,返回句柄。
    ///
    /// `threshold_bytes` 以下的内容**不**溢出(返回 `None`,调用方原样
    /// 保留)。这样小输出不额外绕一圈。
    pub fn spill_if_large(&mut self, content: &str, threshold_bytes: usize) -> Option<SpillHandle> {
        if content.len() <= threshold_bytes {
            return None;
        }
        self.next_id += 1;
        let id = format!("spill-{}", self.next_id);
        let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
        let handle = SpillHandle {
            id: id.clone(),
            bytes: content.len(),
            lines: lines.len(),
            receipt: SpillHandle::render_receipt(&id, content.len(), lines.len()),
        };
        self.entries.insert(id, SpilledEntry { lines });
        Some(handle)
    }

    /// 检索含 `query` 的行(大小写不敏感子串匹配)。
    ///
    /// 返回 `(行号, 内容)` 列表(行号 1 起)。`limit` 控制返回条数。
    pub fn search(&self, id: &str, query: &str, limit: usize) -> Vec<(usize, String)> {
        let Some(entry) = self.entries.get(id) else {
            return Vec::new();
        };
        let needle = query.to_lowercase();
        entry
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.to_lowercase().contains(&needle))
            .map(|(i, line)| (i + 1, line.clone()))
            .take(limit)
            .collect()
    }

    /// 取回指定行区间(`from_line` 1 起,`count` 条)。
    pub fn resume(&self, id: &str, from_line: usize, count: usize) -> Vec<(usize, String)> {
        let Some(entry) = self.entries.get(id) else {
            return Vec::new();
        };
        let start = from_line.saturating_sub(1);
        entry
            .lines
            .iter()
            .enumerate()
            .skip(start)
            .take(count)
            .map(|(i, line)| (i + 1, line.clone()))
            .collect()
    }

    /// 句柄是否存在。
    pub fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    /// 当前存活的句柄数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 清理指定句柄(会话结束 / 显式释放)。
    pub fn release(&mut self, id: &str) -> bool {
        self.entries.remove(id).is_some()
    }

    /// 清空全部(会话退役)。
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// 总占用字节(诊断:spill 也不能无界增长)。
    pub fn total_bytes(&self) -> usize {
        self.entries
            .values()
            .flat_map(|e| e.lines.iter())
            .map(|l| l.len() + 1)
            .sum()
    }
}

/// 计算自适应的输出上限。
///
/// 依据:上下文剩余空间 + 当前任务阶段。
///
/// - 剩余充足(> 50%):用 `base_cap`(正常上限);
/// - 剩余紧张(20%–50%):收到 `base_cap / 2`;
/// - 剩余告急(< 20%):收到 `base_cap / 4`(并把 spill 阈值也压低)。
///
/// 返回 `(输出上限字节, 建议 spill 阈值)`。spill 阈值恒小于输出上限 ——
/// 否则"超上限就截断"会先于"超阈值就溢出",丢掉 spill 的价值。
pub fn adaptive_output_cap(
    base_cap: usize,
    remaining_tokens: usize,
    context_window: usize,
) -> (usize, usize) {
    let ratio = if context_window == 0 {
        1.0
    } else {
        (remaining_tokens as f64 / context_window as f64).clamp(0.0, 1.0)
    };
    let cap = if ratio > 0.5 {
        base_cap
    } else if ratio > 0.2 {
        base_cap / 2
    } else {
        base_cap / 4
    };
    // spill 阈值取上限的 1/4(小输出不绕 spill,大输出优先溢出)。
    let spill_threshold = (cap / 4).max(1024);
    (cap.max(1024), spill_threshold)
}

/// 前缀稳定化:判断两次系统提示是否共享足够长的稳定前缀。
///
/// 服务端 prompt cache 靠「前缀完全一致」命中。若每次请求都在开头插入
/// 时间戳 / 会话 id,cache 永远不命中。本函数帮调用方**诊断**前缀是否
/// 稳定(不一致时能定位到第一个差异位置)。
pub fn stable_prefix_len(a: &str, b: &str) -> usize {
    let mut n = 0;
    for (ca, cb) in a.chars().zip(b.chars()) {
        if ca != cb {
            break;
        }
        n += ca.len_utf8();
    }
    n
}

/// 缓存命中友好度诊断。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CacheFriendly {
    /// 共享前缀占较短者的比例(0.0–1.0)。
    pub prefix_ratio: f64,
    /// 是否足够稳定(> 90% 视为稳定)。
    pub stable: bool,
}

/// 评估两版系统提示的缓存友好度。
pub fn cache_friendly(previous: &str, current: &str) -> CacheFriendly {
    let shorter = previous.len().min(current.len());
    if shorter == 0 {
        return CacheFriendly {
            prefix_ratio: 0.0,
            stable: false,
        };
    }
    let shared = stable_prefix_len(previous, current);
    let ratio = shared as f64 / shorter as f64;
    CacheFriendly {
        prefix_ratio: ratio,
        stable: ratio > 0.9,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_content_not_spilled() {
        let mut s = SpillStore::new();
        assert!(s.spill_if_large("short", 1024).is_none());
        assert!(s.is_empty());
    }

    #[test]
    fn large_content_spilled_with_receipt() {
        let mut s = SpillStore::new();
        let big = "line\n".repeat(500);
        let handle = s.spill_if_large(&big, 100).expect("spill");
        assert_eq!(handle.lines, 500);
        assert!(handle.receipt.contains("ctx_search"), "{}", handle.receipt);
        assert!(handle.receipt.contains(&handle.id));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn search_finds_matching_lines() {
        let mut s = SpillStore::new();
        let content = "ok line\nERROR: boom\nok line\nERROR: again\n";
        let handle = s.spill_if_large(content, 1).expect("spill");
        let hits = s.search(&handle.id, "error", 10);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].0, 2, "行号 1 起");
        assert!(hits[0].1.contains("boom"));
    }

    #[test]
    fn search_is_case_insensitive() {
        let mut s = SpillStore::new();
        let handle = s
            .spill_if_large("Warning: x\nwarning: y\n", 1)
            .expect("spill");
        assert_eq!(s.search(&handle.id, "WARNING", 10).len(), 2);
    }

    #[test]
    fn search_respects_limit() {
        let mut s = SpillStore::new();
        let content = "match\n".repeat(50);
        let handle = s.spill_if_large(&content, 1).expect("spill");
        assert_eq!(s.search(&handle.id, "match", 5).len(), 5);
    }

    #[test]
    fn resume_returns_slice() {
        let mut s = SpillStore::new();
        let content = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let handle = s.spill_if_large(&content, 1).expect("spill");
        let slice = s.resume(&handle.id, 3, 3);
        assert_eq!(slice.len(), 3);
        assert_eq!(slice[0].0, 3);
        assert_eq!(slice[0].1, "line3");
        assert_eq!(slice[2].1, "line5");
    }

    #[test]
    fn resume_past_end_is_clamped() {
        let mut s = SpillStore::new();
        let handle = s.spill_if_large("a\nb\n", 1).expect("spill");
        let slice = s.resume(&handle.id, 10, 5);
        assert!(slice.is_empty(), "越界应返回空而非 panic");
    }

    #[test]
    fn unknown_id_returns_empty() {
        let s = SpillStore::new();
        assert!(s.search("ghost", "x", 10).is_empty());
        assert!(s.resume("ghost", 1, 10).is_empty());
        assert!(!s.contains("ghost"));
    }

    #[test]
    fn release_and_clear() {
        let mut s = SpillStore::new();
        let h1 = s.spill_if_large(&"x\n".repeat(100), 1).expect("spill");
        let h2 = s.spill_if_large(&"y\n".repeat(100), 1).expect("spill");
        assert_eq!(s.len(), 2);
        assert!(s.release(&h1.id));
        assert!(!s.release(&h1.id), "重复释放返回 false");
        assert_eq!(s.len(), 1);
        s.clear();
        assert!(s.is_empty());
        let _ = h2;
    }

    #[test]
    fn total_bytes_tracked() {
        let mut s = SpillStore::new();
        s.spill_if_large(&"abc\n".repeat(100), 1);
        assert!(s.total_bytes() >= 400);
    }

    #[test]
    fn adaptive_cap_full_room_uses_base() {
        let (cap, spill) = adaptive_output_cap(100_000, 80_000, 100_000);
        assert_eq!(cap, 100_000);
        assert!(spill < cap, "spill 阈值必须小于输出上限");
    }

    #[test]
    fn adaptive_cap_tight_room_halves() {
        let (cap, _) = adaptive_output_cap(100_000, 30_000, 100_000);
        assert_eq!(cap, 50_000);
    }

    #[test]
    fn adaptive_cap_critical_quarters() {
        let (cap, _) = adaptive_output_cap(100_000, 5_000, 100_000);
        assert_eq!(cap, 25_000);
    }

    #[test]
    fn adaptive_cap_has_floor() {
        // 极小 base_cap 也要有下限,避免输出上限归零。
        let (cap, spill) = adaptive_output_cap(100, 0, 1000);
        assert!(cap >= 1024);
        assert!(spill >= 1024);
    }

    #[test]
    fn adaptive_cap_zero_window_treated_as_full() {
        let (cap, _) = adaptive_output_cap(50_000, 0, 0);
        assert_eq!(cap, 50_000);
    }

    #[test]
    fn stable_prefix_identical() {
        assert_eq!(stable_prefix_len("abcdef", "abcdef"), 6);
    }

    #[test]
    fn stable_prefix_stops_at_divergence() {
        assert_eq!(stable_prefix_len("abcdef", "abcXef"), 3);
    }

    #[test]
    fn stable_prefix_handles_unicode() {
        // 中文字符多字节,返回的是**字节**长度。
        let n = stable_prefix_len("中文abc", "中文Xbc");
        assert_eq!(n, "中文".len());
    }

    #[test]
    fn cache_friendly_detects_stability() {
        let prev = "You are a helpful agent.\n\n## Rules\n- be concise\n";
        let cur = "You are a helpful agent.\n\n## Rules\n- be concise\n- and kind\n";
        let cf = cache_friendly(prev, cur);
        assert!(cf.stable, "{cf:?}");
        assert!(cf.prefix_ratio > 0.9);
    }

    #[test]
    fn cache_friendly_detects_instability() {
        // 开头插入时间戳 → 前缀立即分歧。
        let prev = "You are a helpful agent.";
        let cur = "2026-11-16T12:00:00Z You are a helpful agent.";
        let cf = cache_friendly(prev, cur);
        assert!(!cf.stable, "{cf:?}");
        assert_eq!(cf.prefix_ratio, 0.0);
    }

    #[test]
    fn cache_friendly_empty() {
        let cf = cache_friendly("", "abc");
        assert!(!cf.stable);
        assert_eq!(cf.prefix_ratio, 0.0);
    }
}
