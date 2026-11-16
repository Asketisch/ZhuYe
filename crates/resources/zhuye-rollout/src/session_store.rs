//! # 会话存储升级(F48)
//!
//! 三件事:
//!
//! 1. **索引与搜索**([`SessionIndex`])—— 万条历史秒搜。会话多了以后
//!    遍历 JSONL 找一条记录是不可接受的。
//! 2. **血缘**([`Lineage`])—— 谁分叉自谁。fork 出来的会话要知道自己的
//!    源头,便于「从哪来的」追溯。
//! 3. **变更捕获**([`ChangeEvent`])—— 回合级的 workspace / changes 事件,
//!    让 UI 免轮询就能看到「这一轮改了哪些文件」。
//!
//! ## 为什么索引是内存倒排而不是真数据库
//!
//! 本地会话数量级在万条以内。内存倒排(`token -> 会话集合`)构建快、
//! 查询 O(1)、无需外部依赖。真需要 SQL 时(sqlite FTS5)是后续替换
//! [`SessionIndex`] 实现的事 —— 接口不变。
//!
//! ## 血缘的方向
//!
//! `fork` 记录的是「子 → 父」。查询「谁从我分叉」需要反向遍历。
//! [`Lineage::children_of`] 提供这个 —— 没有它,UI 画不出会话树。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// 会话索引条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEntry {
    /// 会话 id。
    pub id: String,
    /// 标题(可空)。
    #[serde(default)]
    pub title: String,
    /// 工作区路径。
    #[serde(default)]
    pub workspace: String,
    /// 消息数。
    pub message_count: usize,
    /// 创建时间(UNIX 秒)。
    pub created_at: u64,
    /// 最后活跃时间。
    pub last_active_at: u64,
    /// 分叉来源(`None` = 根会话)。
    #[serde(default)]
    pub forked_from: Option<String>,
    /// 模型 spec。
    #[serde(default)]
    pub model: String,
}

impl SessionEntry {
    pub fn new(id: impl Into<String>, created_at: u64) -> SessionEntry {
        SessionEntry {
            id: id.into(),
            title: String::new(),
            workspace: String::new(),
            message_count: 0,
            created_at,
            last_active_at: created_at,
            forked_from: None,
            model: String::new(),
        }
    }

    /// 用于索引的文本(标题 + 工作区 + 模型)。
    fn index_text(&self) -> String {
        format!("{} {} {}", self.title, self.workspace, self.model).to_lowercase()
    }
}

/// 会话索引(内存倒排)。
#[derive(Debug, Default)]
pub struct SessionIndex {
    entries: BTreeMap<String, SessionEntry>,
    /// token -> 会话 id 集合。
    inverted: BTreeMap<String, BTreeSet<String>>,
}

impl SessionIndex {
    pub fn new() -> SessionIndex {
        SessionIndex::default()
    }

    /// 索引一个会话(覆盖旧条目)。
    pub fn upsert(&mut self, entry: SessionEntry) {
        // 先摘旧索引。
        if let Some(old) = self.entries.get(&entry.id) {
            let old_tokens = tokenize(&old.index_text());
            for token in old_tokens {
                if let Some(set) = self.inverted.get_mut(&token) {
                    set.remove(&old.id);
                }
            }
        }
        // 加新索引。
        for token in tokenize(&entry.index_text()) {
            self.inverted
                .entry(token)
                .or_default()
                .insert(entry.id.clone());
        }
        self.entries.insert(entry.id.clone(), entry);
    }

    /// 移除。
    pub fn remove(&mut self, id: &str) -> bool {
        let Some(old) = self.entries.remove(id) else {
            return false;
        };
        for token in tokenize(&old.index_text()) {
            if let Some(set) = self.inverted.get_mut(&token) {
                set.remove(id);
                if set.is_empty() {
                    self.inverted.remove(&token);
                }
            }
        }
        true
    }

    /// 搜索(所有 token 都命中才算,AND 语义)。
    ///
    /// 结果按 `last_active_at` 降序(最近活跃优先)。
    pub fn search(&self, query: &str) -> Vec<&SessionEntry> {
        let tokens = tokenize(&query.to_lowercase());
        if tokens.is_empty() {
            return Vec::new();
        }
        let mut candidates: Option<BTreeSet<String>> = None;
        for token in &tokens {
            let Some(ids) = self.inverted.get(token) else {
                return Vec::new(); // 任一 token 无命中 → 空
            };
            candidates = Some(match candidates {
                Some(prev) => prev.intersection(ids).cloned().collect(),
                None => ids.clone(),
            });
        }
        let ids = candidates.unwrap_or_default();
        let mut out: Vec<&SessionEntry> =
            ids.iter().filter_map(|id| self.entries.get(id)).collect();
        out.sort_by(|a, b| {
            b.last_active_at
                .cmp(&a.last_active_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        out
    }

    pub fn get(&self, id: &str) -> Option<&SessionEntry> {
        self.entries.get(id)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 最近活跃的 N 个会话。
    pub fn recent(&self, n: usize) -> Vec<&SessionEntry> {
        let mut all: Vec<&SessionEntry> = self.entries.values().collect();
        all.sort_by(|a, b| {
            b.last_active_at
                .cmp(&a.last_active_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        all.truncate(n);
        all
    }

    /// 某工作区的全部会话。
    pub fn by_workspace(&self, workspace: &str) -> Vec<&SessionEntry> {
        let mut out: Vec<&SessionEntry> = self
            .entries
            .values()
            .filter(|e| e.workspace == workspace)
            .collect();
        out.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));
        out
    }

    /// 更新最后活跃时间。
    pub fn touch(&mut self, id: &str, at: u64) -> bool {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.last_active_at = at;
            return true;
        }
        false
    }
}

/// 极简分词:按非字母数字切分,小写。
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

/// 会话血缘。
#[derive(Debug, Default)]
pub struct Lineage {
    /// 子 → 父。
    parent: BTreeMap<String, String>,
    /// 父 → 子集合。
    children: BTreeMap<String, BTreeSet<String>>,
}

impl Lineage {
    pub fn new() -> Lineage {
        Lineage::default()
    }

    /// 记录 fork 关系。
    pub fn record_fork(&mut self, child: &str, parent: &str) {
        self.parent.insert(child.to_string(), parent.to_string());
        self.children
            .entry(parent.to_string())
            .or_default()
            .insert(child.to_string());
    }

    /// 直接父会话。
    pub fn parent_of(&self, child: &str) -> Option<&str> {
        self.parent.get(child).map(String::as_str)
    }

    /// 直接子会话(排序)。
    pub fn children_of(&self, parent: &str) -> Vec<&str> {
        self.children
            .get(parent)
            .map(|set| set.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// 追溯完整祖先链(从近到远)。
    pub fn ancestors_of(&self, session: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut current = session;
        // 防环(异常数据)
        let mut seen = BTreeSet::new();
        while let Some(parent) = self.parent_of(current) {
            if !seen.insert(parent) {
                break;
            }
            out.push(parent);
            current = parent;
        }
        out
    }

    /// 全部后代(递归,排序)。
    pub fn descendants_of(&self, root: &str) -> Vec<String> {
        let mut out = BTreeSet::new();
        let mut frontier = vec![root.to_string()];
        while let Some(current) = frontier.pop() {
            for child in self.children.get(&current).into_iter().flatten() {
                if out.insert(child.clone()) {
                    frontier.push(child.clone());
                }
            }
        }
        out.into_iter().collect()
    }

    /// 根会话(无父)。
    pub fn root_of<'a>(&'a self, session: &'a str) -> &'a str {
        let mut current = session;
        let mut seen = BTreeSet::new();
        while let Some(parent) = self.parent_of(current) {
            if !seen.insert(current) {
                break;
            }
            current = parent;
        }
        current
    }

    /// 树深度(根 = 0)。
    pub fn depth_of(&self, session: &str) -> usize {
        self.ancestors_of(session).len()
    }
}

/// 回合级变更事件(F48 变更捕获)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChangeEvent {
    /// 工作区切换。
    WorkspaceChanged { from: String, to: String, at: u64 },
    /// 文件变更(回合结束时汇总)。
    FilesChanged {
        turn_id: String,
        paths: Vec<String>,
        at: u64,
    },
    /// 会话被分叉。
    SessionForked {
        child: String,
        parent: String,
        at: u64,
    },
    /// 上下文被清空(clearContext)。
    ContextCleared { turn_id: String, at: u64 },
}

impl ChangeEvent {
    /// 时间戳。
    pub fn at(&self) -> u64 {
        match self {
            ChangeEvent::WorkspaceChanged { at, .. }
            | ChangeEvent::FilesChanged { at, .. }
            | ChangeEvent::SessionForked { at, .. }
            | ChangeEvent::ContextCleared { at, .. } => *at,
        }
    }

    /// 人读一行(UI 通知)。
    pub fn summary(&self) -> String {
        match self {
            ChangeEvent::WorkspaceChanged { from, to, .. } => {
                format!("工作区:{from} → {to}")
            }
            ChangeEvent::FilesChanged { paths, .. } => {
                format!("本轮改动 {} 个文件", paths.len())
            }
            ChangeEvent::SessionForked { child, parent, .. } => {
                format!("会话分叉:{parent} → {child}")
            }
            ChangeEvent::ContextCleared { .. } => "上下文已清空".to_string(),
        }
    }
}

/// 变更事件日志。
#[derive(Debug, Default)]
pub struct ChangeLog {
    events: Vec<ChangeEvent>,
}

impl ChangeLog {
    pub fn new() -> ChangeLog {
        ChangeLog::default()
    }

    pub fn push(&mut self, event: ChangeEvent) {
        self.events.push(event);
    }

    pub fn events(&self) -> &[ChangeEvent] {
        &self.events
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// 某回合改动过的文件(合并去重)。
    pub fn files_in_turn(&self, turn_id: &str) -> Vec<&str> {
        let mut set = BTreeSet::new();
        for e in &self.events {
            if let ChangeEvent::FilesChanged {
                turn_id: t, paths, ..
            } = e
                && t == turn_id
            {
                for p in paths {
                    set.insert(p.as_str());
                }
            }
        }
        set.into_iter().collect()
    }

    /// 最近 N 条。
    pub fn recent(&self, n: usize) -> &[ChangeEvent] {
        let start = self.events.len().saturating_sub(n);
        &self.events[start..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, title: &str, ws: &str, at: u64) -> SessionEntry {
        SessionEntry {
            title: title.to_string(),
            workspace: ws.to_string(),
            last_active_at: at,
            ..SessionEntry::new(id, at)
        }
    }

    #[test]
    fn index_search_finds_by_title() {
        let mut idx = SessionIndex::new();
        idx.upsert(entry("s1", "fix login bug", "/repo/a", 100));
        idx.upsert(entry("s2", "add tests", "/repo/b", 200));
        let hits = idx.search("login");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "s1");
    }

    #[test]
    fn index_search_and_semantics() {
        let mut idx = SessionIndex::new();
        idx.upsert(entry("s1", "fix login bug", "/repo/a", 100));
        idx.upsert(entry("s2", "fix logout bug", "/repo/a", 200));
        // 两个 token 都要命中
        let hits = idx.search("fix login");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "s1");
    }

    #[test]
    fn index_search_no_match() {
        let mut idx = SessionIndex::new();
        idx.upsert(entry("s1", "fix login", "/a", 1));
        assert!(idx.search("nonexistent").is_empty());
        assert!(idx.search("").is_empty());
    }

    #[test]
    fn index_search_sorted_by_last_active() {
        let mut idx = SessionIndex::new();
        idx.upsert(entry("old", "common", "/a", 100));
        idx.upsert(entry("new", "common", "/b", 300));
        let hits = idx.search("common");
        assert_eq!(hits[0].id, "new", "最近活跃优先");
    }

    #[test]
    fn index_upsert_updates_inverted() {
        let mut idx = SessionIndex::new();
        idx.upsert(entry("s1", "old title", "/a", 1));
        idx.upsert(entry("s1", "new title", "/a", 2));
        assert!(idx.search("old").is_empty(), "旧标题不应再命中");
        assert_eq!(idx.search("new").len(), 1);
        assert_eq!(idx.len(), 1, "同 id 不重复");
    }

    #[test]
    fn index_remove_cleans_inverted() {
        let mut idx = SessionIndex::new();
        idx.upsert(entry("s1", "unique", "/a", 1));
        assert!(idx.remove("s1"));
        assert!(idx.search("unique").is_empty());
        assert!(!idx.remove("s1"), "重复移除返回 false");
    }

    #[test]
    fn index_recent() {
        let mut idx = SessionIndex::new();
        for i in 0..5 {
            idx.upsert(entry(&format!("s{i}"), "t", "/a", i));
        }
        let recent = idx.recent(2);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].id, "s4");
    }

    #[test]
    fn index_by_workspace() {
        let mut idx = SessionIndex::new();
        idx.upsert(entry("a1", "t", "/repo/a", 1));
        idx.upsert(entry("a2", "t", "/repo/a", 2));
        idx.upsert(entry("b1", "t", "/repo/b", 3));
        assert_eq!(idx.by_workspace("/repo/a").len(), 2);
        assert_eq!(idx.by_workspace("/repo/b").len(), 1);
    }

    #[test]
    fn index_touch() {
        let mut idx = SessionIndex::new();
        idx.upsert(entry("s1", "t", "/a", 100));
        assert!(idx.touch("s1", 999));
        assert_eq!(idx.get("s1").unwrap().last_active_at, 999);
        assert!(!idx.touch("ghost", 1));
    }

    #[test]
    fn lineage_fork_tracking() {
        let mut l = Lineage::new();
        l.record_fork("child1", "root");
        l.record_fork("child2", "root");
        l.record_fork("grandchild", "child1");
        assert_eq!(l.parent_of("child1"), Some("root"));
        assert_eq!(l.children_of("root"), vec!["child1", "child2"]);
        assert_eq!(l.parent_of("root"), None);
    }

    #[test]
    fn lineage_ancestors() {
        let mut l = Lineage::new();
        l.record_fork("b", "a");
        l.record_fork("c", "b");
        assert_eq!(l.ancestors_of("c"), vec!["b", "a"]);
        assert!(l.ancestors_of("a").is_empty());
    }

    #[test]
    fn lineage_descendants() {
        let mut l = Lineage::new();
        l.record_fork("b", "a");
        l.record_fork("c", "a");
        l.record_fork("d", "b");
        let mut desc = l.descendants_of("a");
        desc.sort();
        assert_eq!(desc, vec!["b", "c", "d"]);
    }

    #[test]
    fn lineage_root_and_depth() {
        let mut l = Lineage::new();
        l.record_fork("b", "a");
        l.record_fork("c", "b");
        assert_eq!(l.root_of("c"), "a");
        assert_eq!(l.depth_of("a"), 0);
        assert_eq!(l.depth_of("b"), 1);
        assert_eq!(l.depth_of("c"), 2);
    }

    #[test]
    fn lineage_handles_cycle_safely() {
        // 异常数据:成环。不应死循环。
        let mut l = Lineage::new();
        l.record_fork("a", "b");
        l.record_fork("b", "a");
        let _ = l.ancestors_of("a"); // 不应挂起
        let _ = l.root_of("a");
    }

    #[test]
    fn change_log_files_in_turn() {
        let mut log = ChangeLog::new();
        log.push(ChangeEvent::FilesChanged {
            turn_id: "t1".into(),
            paths: vec!["a.rs".into(), "b.rs".into()],
            at: 1,
        });
        log.push(ChangeEvent::FilesChanged {
            turn_id: "t1".into(),
            paths: vec!["b.rs".into(), "c.rs".into()],
            at: 2,
        });
        log.push(ChangeEvent::FilesChanged {
            turn_id: "t2".into(),
            paths: vec!["d.rs".into()],
            at: 3,
        });
        let files = log.files_in_turn("t1");
        assert_eq!(files, vec!["a.rs", "b.rs", "c.rs"], "合并去重");
    }

    #[test]
    fn change_event_summaries() {
        let ws = ChangeEvent::WorkspaceChanged {
            from: "/a".into(),
            to: "/b".into(),
            at: 1,
        };
        assert!(ws.summary().contains("/a → /b"), "{}", ws.summary());
        assert_eq!(ws.at(), 1);

        let files = ChangeEvent::FilesChanged {
            turn_id: "t".into(),
            paths: vec!["x".into(), "y".into()],
            at: 2,
        };
        assert!(files.summary().contains("2 个文件"));

        let cleared = ChangeEvent::ContextCleared {
            turn_id: "t".into(),
            at: 3,
        };
        assert!(cleared.summary().contains("清空"));
    }

    #[test]
    fn change_log_recent() {
        let mut log = ChangeLog::new();
        for i in 0..5 {
            log.push(ChangeEvent::ContextCleared {
                turn_id: format!("t{i}"),
                at: i,
            });
        }
        assert_eq!(log.recent(2).len(), 2);
        assert_eq!(log.recent(100).len(), 5);
        assert!(log.recent(0).is_empty());
    }

    #[test]
    fn session_entry_serializes() {
        let e = entry("s1", "title", "/ws", 100);
        let json = serde_json::to_string(&e).expect("serialize");
        let back: SessionEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, e);
    }
}
