//! # 记忆长尾(F59)
//!
//! 四项进阶能力:
//!
//! 1. **时态边**([`TemporalEdge`])—— 记忆之间的「先于 / 取代 / 派生」关系
//!    带时间戳。旧知识自动过期(不是删除,是标记 `valid_until`)。
//! 2. **证据接地**([`Evidence`])—— 每条事实带出处(文件 / URL / 会话),
//!    以及有效期。「这个配置项是 X」的出处若是三个月前的文件,可信度
//!    应随时间下降。
//! 3. **命名槽共享状态**([`NamedSlot`])—— 多 agent 交接时的共享字段,
//!    分公私(私字段不注入他人上下文),避免交接丢信息。
//! 4. **MemFS 导出**—— 把记忆导出成可 git 化的目录结构,便于版本化。
//!
//! ## 为什么「时态」而不是「覆盖」
//!
//! 覆盖会丢失「原来是什么」。带时态边能回答「这条在 3 月时是什么」——
//! 对调试「为什么当时这么做」至关重要。

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

/// 时态边类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// A 先于 B 发生。
    Precedes,
    /// A 取代 B(新事实覆盖旧事实)。
    Supersedes,
    /// A 派生自 B。
    DerivedFrom,
    /// A 与 B 相关(无方向语义)。
    RelatedTo,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Precedes => "precedes",
            EdgeKind::Supersedes => "supersedes",
            EdgeKind::DerivedFrom => "derived_from",
            EdgeKind::RelatedTo => "related_to",
        }
    }
}

/// 一条带时间戳的边。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemporalEdge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    /// 关系建立时的逻辑时间戳(调用方维护,单位自定)。
    pub at: u64,
}

/// 证据出处类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    /// 工作区文件(带路径)。
    File,
    /// 外部 URL。
    Url,
    /// 会话(带 session id)。
    Session,
    /// 用户直接告知。
    UserStated,
}

impl EvidenceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceSource::File => "file",
            EvidenceSource::Url => "url",
            EvidenceSource::Session => "session",
            EvidenceSource::UserStated => "user_stated",
        }
    }
}

/// 一条证据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub source: EvidenceSource,
    /// 出处标识(路径 / URL / session id / 用户原话摘录)。
    pub reference: String,
    /// 采集时的逻辑时间戳。
    pub observed_at: u64,
    /// 有效期截止(`None` = 永久有效)。过期后事实应降级。
    #[serde(default)]
    pub valid_until: Option<u64>,
}

impl Evidence {
    pub fn new(source: EvidenceSource, reference: impl Into<String>, observed_at: u64) -> Evidence {
        Evidence {
            source,
            reference: reference.into(),
            observed_at,
            valid_until: None,
        }
    }

    pub fn with_expiry(mut self, valid_until: u64) -> Evidence {
        self.valid_until = Some(valid_until);
        self
    }

    /// 在 `now` 时刻是否仍有效。
    pub fn is_valid_at(&self, now: u64) -> bool {
        match self.valid_until {
            Some(deadline) => now <= deadline,
            None => true,
        }
    }
}

/// 带证据的事实。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroundedFact {
    pub key: String,
    pub content: String,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
}

impl GroundedFact {
    pub fn new(key: impl Into<String>, content: impl Into<String>) -> GroundedFact {
        GroundedFact {
            key: key.into(),
            content: content.into(),
            evidence: Vec::new(),
        }
    }

    pub fn with_evidence(mut self, ev: Evidence) -> GroundedFact {
        self.evidence.push(ev);
        self
    }

    /// 是否有仍有效的证据(无证据 = 不可信,返回 false)。
    pub fn has_valid_evidence(&self, now: u64) -> bool {
        self.evidence.iter().any(|e| e.is_valid_at(now))
    }

    /// 证据强度(0.0–1.0):用户明说 > 文件 > 会话 > URL;
    /// 全部过期则 0。
    pub fn confidence_at(&self, now: u64) -> f64 {
        let valid: Vec<&Evidence> = self
            .evidence
            .iter()
            .filter(|e| e.is_valid_at(now))
            .collect();
        if valid.is_empty() {
            return 0.0;
        }
        valid
            .iter()
            .map(|e| match e.source {
                EvidenceSource::UserStated => 1.0,
                EvidenceSource::File => 0.9,
                EvidenceSource::Session => 0.7,
                EvidenceSource::Url => 0.6,
            })
            .fold(0.0f64, f64::max)
    }
}

/// 命名槽中的字段(公 / 私)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotField {
    pub value: String,
    /// 是否可被其他 agent 看到。`false` = 私有(仅本 agent)。
    pub shared: bool,
}

/// 命名槽(多 agent 交接的共享状态)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedSlot {
    pub name: String,
    fields: BTreeMap<String, SlotField>,
}

impl NamedSlot {
    pub fn new(name: impl Into<String>) -> NamedSlot {
        NamedSlot {
            name: name.into(),
            fields: BTreeMap::new(),
        }
    }

    /// 写入字段。
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>, shared: bool) {
        self.fields.insert(
            key.into(),
            SlotField {
                value: value.into(),
                shared,
            },
        );
    }

    /// 读字段(本 agent 视角,公私都可见)。
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(|f| f.value.as_str())
    }

    /// 交接视图:**只含 shared 字段**。
    ///
    /// 这是「共享状态」的核心约束 —— 私有字段不随交接泄漏。
    pub fn handoff_view(&self) -> NamedSlot {
        NamedSlot {
            name: self.name.clone(),
            fields: self
                .fields
                .iter()
                .filter(|(_, f)| f.shared)
                .map(|(k, f)| (k.clone(), f.clone()))
                .collect(),
        }
    }

    /// 合并另一个槽的 shared 字段(交接接收方用)。
    pub fn merge_shared(&mut self, other: &NamedSlot) {
        for (k, f) in &other.fields {
            if f.shared {
                self.fields.insert(k.clone(), f.clone());
            }
        }
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// 全部字段名(排序)。
    pub fn keys(&self) -> Vec<&str> {
        self.fields.keys().map(String::as_str).collect()
    }
}

/// 记忆库(时态 + 证据 + 边的容器)。
#[derive(Debug, Default)]
pub struct LongTailStore {
    facts: HashMap<String, GroundedFact>,
    edges: Vec<TemporalEdge>,
}

impl LongTailStore {
    pub fn new() -> LongTailStore {
        LongTailStore::default()
    }

    /// 写入/覆盖事实。
    pub fn upsert(&mut self, fact: GroundedFact) {
        self.facts.insert(fact.key.clone(), fact);
    }

    /// 读事实。
    pub fn get(&self, key: &str) -> Option<&GroundedFact> {
        self.facts.get(key)
    }

    /// 加一条时态边。
    pub fn add_edge(&mut self, edge: TemporalEdge) {
        self.edges.push(edge);
    }

    /// 查询某条边类型指向 `key` 的所有来源(如「谁取代了我」)。
    pub fn edges_to(&self, key: &str, kind: EdgeKind) -> Vec<&TemporalEdge> {
        self.edges
            .iter()
            .filter(|e| e.to == key && e.kind == kind)
            .collect()
    }

    /// 查询从 `key` 出发的边。
    pub fn edges_from(&self, key: &str, kind: EdgeKind) -> Vec<&TemporalEdge> {
        self.edges
            .iter()
            .filter(|e| e.from == key && e.kind == kind)
            .collect()
    }

    /// 当前有效的事实(有有效证据),按置信度降序。
    pub fn valid_facts(&self, now: u64) -> Vec<(&GroundedFact, f64)> {
        let mut out: Vec<(&GroundedFact, f64)> = self
            .facts
            .values()
            .filter(|f| f.has_valid_evidence(now))
            .map(|f| (f, f.confidence_at(now)))
            .collect();
        out.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.key.cmp(&b.0.key))
        });
        out
    }

    /// 已过期事实(有证据但全部失效)—— 用于 UI 提示「该更新了」。
    pub fn stale_facts(&self, now: u64) -> Vec<&GroundedFact> {
        let mut out: Vec<&GroundedFact> = self
            .facts
            .values()
            .filter(|f| !f.evidence.is_empty() && !f.has_valid_evidence(now))
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }

    pub fn fact_count(&self) -> usize {
        self.facts.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// 导出成 MemFS 风格的文件清单(`路径 -> 内容`)。
    ///
    /// 结构:`facts/<key>.md`(正文 + 证据)、`edges.md`(全部边)。
    /// 可 git 化 —— 记忆库能版本化、能 diff、能 review。
    pub fn export_memfs(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for (key, fact) in &self.facts {
            let mut md = format!("# {key}\n\n{}\n", fact.content);
            if !fact.evidence.is_empty() {
                md.push_str("\n## 证据\n");
                for e in &fact.evidence {
                    md.push_str(&format!(
                        "- {} `{}` @{}",
                        e.source.as_str(),
                        e.reference,
                        e.observed_at
                    ));
                    if let Some(v) = e.valid_until {
                        md.push_str(&format!(" (valid until {v})"));
                    }
                    md.push('\n');
                }
            }
            out.insert(format!("facts/{key}.md"), md);
        }
        if !self.edges.is_empty() {
            let mut md = String::from("# 时态边\n\n");
            let mut sorted = self.edges.clone();
            sorted.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.from.cmp(&b.from)));
            for e in sorted {
                md.push_str(&format!(
                    "- `{}` --{}--> `{}` @{}\n",
                    e.from,
                    e.kind.as_str(),
                    e.to,
                    e.at
                ));
            }
            out.insert("edges.md".to_string(), md);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev_file(at: u64) -> Evidence {
        Evidence::new(EvidenceSource::File, "src/lib.rs", at)
    }

    #[test]
    fn evidence_validity() {
        let e = ev_file(100);
        assert!(e.is_valid_at(1000), "无截止 = 永久有效");
        let e = ev_file(100).with_expiry(500);
        assert!(e.is_valid_at(500));
        assert!(!e.is_valid_at(501));
    }

    #[test]
    fn confidence_by_source() {
        let now = 1000;
        let user = GroundedFact::new("k", "v").with_evidence(Evidence::new(
            EvidenceSource::UserStated,
            "chat",
            100,
        ));
        let file = GroundedFact::new("k", "v").with_evidence(ev_file(100));
        let url = GroundedFact::new("k", "v").with_evidence(Evidence::new(
            EvidenceSource::Url,
            "https://x",
            100,
        ));
        assert!(user.confidence_at(now) > file.confidence_at(now));
        assert!(file.confidence_at(now) > url.confidence_at(now));
    }

    #[test]
    fn expired_evidence_drops_confidence_to_zero() {
        let fact = GroundedFact::new("k", "v").with_evidence(ev_file(100).with_expiry(200));
        assert_eq!(fact.confidence_at(100), 0.9);
        assert_eq!(fact.confidence_at(300), 0.0, "过期后不可信");
        assert!(!fact.has_valid_evidence(300));
    }

    #[test]
    fn fact_without_evidence_untrusted() {
        let fact = GroundedFact::new("k", "v");
        assert_eq!(fact.confidence_at(0), 0.0);
        assert!(!fact.has_valid_evidence(0));
    }

    #[test]
    fn store_upsert_and_get() {
        let mut s = LongTailStore::new();
        s.upsert(GroundedFact::new("a", "x").with_evidence(ev_file(1)));
        assert_eq!(s.fact_count(), 1);
        assert_eq!(s.get("a").unwrap().content, "x");
    }

    #[test]
    fn valid_vs_stale_partition() {
        let mut s = LongTailStore::new();
        s.upsert(GroundedFact::new("fresh", "x").with_evidence(ev_file(100)));
        s.upsert(GroundedFact::new("stale", "y").with_evidence(ev_file(100).with_expiry(200)));
        let valid = s.valid_facts(300);
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].0.key, "fresh");
        let stale = s.stale_facts(300);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].key, "stale");
    }

    #[test]
    fn edges_queryable() {
        let mut s = LongTailStore::new();
        s.add_edge(TemporalEdge {
            from: "new".into(),
            to: "old".into(),
            kind: EdgeKind::Supersedes,
            at: 10,
        });
        assert_eq!(s.edges_to("old", EdgeKind::Supersedes).len(), 1);
        assert_eq!(s.edges_from("new", EdgeKind::Supersedes).len(), 1);
        assert_eq!(s.edges_to("old", EdgeKind::Precedes).len(), 0);
    }

    #[test]
    fn named_slot_handoff_hides_private() {
        let mut slot = NamedSlot::new("handoff");
        slot.set("task", "fix login", true);
        slot.set("api_key", "secret", false);
        let view = slot.handoff_view();
        assert_eq!(view.get("task"), Some("fix login"));
        assert_eq!(view.get("api_key"), None, "私有字段不得随交接泄漏");
        assert_eq!(slot.get("api_key"), Some("secret"), "本 agent 仍可见");
    }

    #[test]
    fn named_slot_merge_only_shared() {
        let mut a = NamedSlot::new("a");
        a.set("x", "1", true);
        a.set("secret", "s", false);
        let mut b = NamedSlot::new("b");
        b.merge_shared(&a);
        assert_eq!(b.get("x"), Some("1"));
        assert_eq!(b.get("secret"), None, "私有字段不合并");
    }

    #[test]
    fn named_slot_keys_sorted() {
        let mut s = NamedSlot::new("s");
        s.set("zebra", "1", true);
        s.set("apple", "2", true);
        assert_eq!(s.keys(), vec!["apple", "zebra"]);
    }

    #[test]
    fn memfs_export_structure() {
        let mut s = LongTailStore::new();
        s.upsert(GroundedFact::new("cfg", "test cmd is make").with_evidence(ev_file(1)));
        s.add_edge(TemporalEdge {
            from: "b".into(),
            to: "a".into(),
            kind: EdgeKind::Precedes,
            at: 5,
        });
        let files = s.export_memfs();
        assert!(files.contains_key("facts/cfg.md"), "{:?}", files.keys());
        let cfg = &files["facts/cfg.md"];
        assert!(cfg.contains("# cfg"), "{cfg}");
        assert!(cfg.contains("test cmd is make"), "{cfg}");
        assert!(cfg.contains("file `src/lib.rs`"), "{cfg}");
        assert!(files.contains_key("edges.md"));
        assert!(
            files["edges.md"].contains("precedes"),
            "{}",
            files["edges.md"]
        );
    }

    #[test]
    fn memfs_export_without_edges() {
        let mut s = LongTailStore::new();
        s.upsert(GroundedFact::new("k", "v"));
        let files = s.export_memfs();
        assert!(!files.contains_key("edges.md"), "无边不生成 edges.md");
    }

    #[test]
    fn valid_facts_sorted_by_confidence() {
        let mut s = LongTailStore::new();
        s.upsert(GroundedFact::new("low", "x").with_evidence(Evidence::new(
            EvidenceSource::Url,
            "u",
            1,
        )));
        s.upsert(GroundedFact::new("high", "y").with_evidence(Evidence::new(
            EvidenceSource::UserStated,
            "chat",
            1,
        )));
        let valid = s.valid_facts(10);
        assert_eq!(valid[0].0.key, "high");
    }
}
