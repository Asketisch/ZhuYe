//! # 混合检索:BM25 + 向量 RRF 融合(F16)
//!
//! 记忆的「语义搜索」:按**意思**找,而不只按关键词找。「上次那个登录
//! bug 怎么修的」能命中一条只写了「认证 token 过期导致 401」的记忆。
//!
//! ## 三档降级(关键设计)
//!
//! | 条件 | 行为 |
//! |---|---|
//! | 有 embedding 后端 + 向量库 | BM25 + 向量 RRF 融合 |
//! | 只有 embedding,无向量库 | BM25 + 内存余弦(小规模可用) |
//! | 无 embedding | 纯 BM25(与既有行为一致) |
//!
//! 降级是**自动**的:没有 embedding 配置时完全走 BM25,不会因为"向量
//! 库没装"而让记忆检索整个失效。这是本地闭环的硬要求。
//!
//! ## 为什么用 RRF 而不是加权分数
//!
//! BM25 分数与余弦相似度量纲完全不同(前者 0–∞,后者 -1–1),加权求和
//! 需要归一化,而归一化依赖当次结果分布 —— 结果集变了排序就漂。
//! Reciprocal Rank Fusion 只看**名次**:`1/(k + rank)`,与分数尺度无关,
//! 是混合检索的公认稳健做法(k=60 为常用值)。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// 检索来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalSource {
    /// 仅 BM25 关键词命中。
    Keyword,
    /// 仅向量语义命中。
    Semantic,
    /// 两路都命中(融合后名次靠前)。
    Hybrid,
}

impl RetrievalSource {
    pub fn label(self) -> &'static str {
        match self {
            RetrievalSource::Keyword => "keyword",
            RetrievalSource::Semantic => "semantic",
            RetrievalSource::Hybrid => "hybrid",
        }
    }
}

/// 一条检索结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HybridHit {
    /// 条目 id(调用方定义,通常是 note 标题 / 记忆键)。
    pub id: String,
    /// 融合后的 RRF 分数(越大越相关)。
    pub score: f64,
    /// 命中来源(UI 徽标用)。
    pub source: RetrievalSource,
    /// BM25 原始名次(1 起;`None` = 该路未命中)。
    pub keyword_rank: Option<usize>,
    /// 向量原始名次(1 起;`None` = 该路未命中)。
    pub semantic_rank: Option<usize>,
}

/// RRF 常数(业界常用 60)。
pub const RRF_K: f64 = 60.0;

/// 融合两路检索结果。
///
/// `keyword` / `semantic` 各自是**已按相关性降序**的 id 列表。返回按
/// RRF 分数降序的融合结果。
pub fn fuse_rrf(keyword: &[String], semantic: &[String], limit: usize) -> Vec<HybridHit> {
    let mut scores: HashMap<&str, (f64, Option<usize>, Option<usize>)> = HashMap::new();

    for (rank, id) in keyword.iter().enumerate() {
        let entry = scores.entry(id.as_str()).or_insert((0.0, None, None));
        entry.0 += 1.0 / (RRF_K + (rank + 1) as f64);
        entry.1 = Some(rank + 1);
    }
    for (rank, id) in semantic.iter().enumerate() {
        let entry = scores.entry(id.as_str()).or_insert((0.0, None, None));
        entry.0 += 1.0 / (RRF_K + (rank + 1) as f64);
        entry.2 = Some(rank + 1);
    }

    let mut hits: Vec<HybridHit> = scores
        .into_iter()
        .map(|(id, (score, kr, sr))| HybridHit {
            id: id.to_string(),
            score,
            source: match (kr.is_some(), sr.is_some()) {
                (true, true) => RetrievalSource::Hybrid,
                (true, false) => RetrievalSource::Keyword,
                (false, true) => RetrievalSource::Semantic,
                (false, false) => RetrievalSource::Keyword, // 不可达
            },
            keyword_rank: kr,
            semantic_rank: sr,
        })
        .collect();

    // 分数降序;同分按 id 字典序(确定性,便于测试与快照)。
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    hits.truncate(limit);
    hits
}

/// 余弦相似度(两向量长度必须相同;零向量返回 0)。
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// 内存向量索引(小规模降级路径:无向量库时可用)。
///
/// 规模上限由调用方控制 —— O(n) 扫描,记忆条目通常在几百到几千量级,
/// 足够;上万条时应接 sqlite-vec / LanceDB。
#[derive(Debug, Default)]
pub struct InMemoryVectorIndex {
    entries: Vec<(String, Vec<f32>)>,
}

impl InMemoryVectorIndex {
    pub fn new() -> InMemoryVectorIndex {
        InMemoryVectorIndex::default()
    }

    /// 插入或替换一条向量。
    pub fn upsert(&mut self, id: impl Into<String>, vector: Vec<f32>) {
        let id = id.into();
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| k == &id) {
            slot.1 = vector;
        } else {
            self.entries.push((id, vector));
        }
    }

    /// 删除一条。
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|(k, _)| k != id);
        before != self.entries.len()
    }

    /// 条目数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 按余弦相似度取 top-k(降序)。相似度 <= 0 的不返回。
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(String, f32)> {
        let mut scored: Vec<(String, f32)> = self
            .entries
            .iter()
            .map(|(id, v)| (id.clone(), cosine_similarity(query, v)))
            .filter(|(_, s)| *s > 0.0)
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        scored.truncate(k);
        scored
    }
}

/// 混合检索器:两路各自取 top-k,再 RRF 融合。
#[derive(Debug, Default)]
pub struct HybridRetriever {
    vectors: InMemoryVectorIndex,
    /// RRF 融合后的返回条数。
    limit: usize,
}

impl HybridRetriever {
    pub fn new(limit: usize) -> HybridRetriever {
        HybridRetriever {
            vectors: InMemoryVectorIndex::new(),
            limit: if limit == 0 { 10 } else { limit },
        }
    }

    /// 是否具备向量能力(无向量时为纯 BM25)。
    pub fn has_vectors(&self) -> bool {
        !self.vectors.is_empty()
    }

    /// 索引一条记忆的向量。
    pub fn index_vector(&mut self, id: impl Into<String>, vector: Vec<f32>) {
        self.vectors.upsert(id, vector);
    }

    /// 混合检索。
    ///
    /// `keyword_hits` 由调用方用既有 BM25 算出(保持与既有行为一致);
    /// `query_vector` 为 `None` 时退化为纯 BM25。
    pub fn retrieve(
        &self,
        keyword_hits: Vec<String>,
        query_vector: Option<&[f32]>,
    ) -> Vec<HybridHit> {
        match query_vector {
            Some(qv) if self.has_vectors() => {
                let semantic: Vec<String> = self
                    .vectors
                    .search(qv, self.limit)
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect();
                fuse_rrf(&keyword_hits, &semantic, self.limit)
            }
            // 无 query 向量或无索引 → 纯关键词(名次即顺序)。
            _ => fuse_rrf(&keyword_hits, &[], self.limit),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rrf_boosts_items_in_both_lists() {
        // "b" 在两路都靠前 → 融合后应排第一。
        let keyword = ids(&["a", "b", "c"]);
        let semantic = ids(&["b", "d"]);
        let fused = fuse_rrf(&keyword, &semantic, 10);
        assert_eq!(fused[0].id, "b");
        assert_eq!(fused[0].source, RetrievalSource::Hybrid);
        assert_eq!(fused[0].keyword_rank, Some(2));
        assert_eq!(fused[0].semantic_rank, Some(1));
    }

    #[test]
    fn rrf_marks_single_source_hits() {
        let fused = fuse_rrf(&ids(&["a"]), &ids(&["b"]), 10);
        let a = fused.iter().find(|h| h.id == "a").expect("a");
        let b = fused.iter().find(|h| h.id == "b").expect("b");
        assert_eq!(a.source, RetrievalSource::Keyword);
        assert_eq!(b.source, RetrievalSource::Semantic);
    }

    #[test]
    fn rrf_respects_limit() {
        let keyword = ids(&["a", "b", "c", "d", "e"]);
        let fused = fuse_rrf(&keyword, &[], 3);
        assert_eq!(fused.len(), 3);
    }

    #[test]
    fn rrf_empty_inputs() {
        assert!(fuse_rrf(&[], &[], 10).is_empty());
        let only_kw = fuse_rrf(&ids(&["a"]), &[], 10);
        assert_eq!(only_kw.len(), 1);
    }

    #[test]
    fn rrf_score_decreases_with_rank() {
        let fused = fuse_rrf(&ids(&["first", "second", "third"]), &[], 10);
        assert!(fused[0].score > fused[1].score);
        assert!(fused[1].score > fused[2].score);
    }

    #[test]
    fn cosine_basics() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 2.0]), 0.0, "维度不匹配");
    }

    #[test]
    fn vector_index_search_orders_by_similarity() {
        let mut idx = InMemoryVectorIndex::new();
        idx.upsert("near", vec![1.0, 0.1]);
        idx.upsert("far", vec![0.0, 1.0]);
        let hits = idx.search(&[1.0, 0.0], 10);
        assert_eq!(hits[0].0, "near");
    }

    #[test]
    fn vector_index_upsert_replaces() {
        let mut idx = InMemoryVectorIndex::new();
        idx.upsert("a", vec![1.0, 0.0]);
        idx.upsert("a", vec![0.0, 1.0]);
        assert_eq!(idx.len(), 1, "同 id 应替换而非追加");
    }

    #[test]
    fn vector_index_remove() {
        let mut idx = InMemoryVectorIndex::new();
        idx.upsert("a", vec![1.0]);
        assert!(idx.remove("a"));
        assert!(!idx.remove("a"), "重复删除返回 false");
        assert!(idx.is_empty());
    }

    #[test]
    fn hybrid_degrades_to_keyword_without_vectors() {
        let r = HybridRetriever::new(5);
        assert!(!r.has_vectors());
        let hits = r.retrieve(ids(&["a", "b"]), Some(&[1.0, 0.0]));
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.source == RetrievalSource::Keyword));
    }

    #[test]
    fn hybrid_uses_vectors_when_indexed() {
        let mut r = HybridRetriever::new(5);
        r.index_vector("semantic-only", vec![1.0, 0.0]);
        r.index_vector("both", vec![1.0, 0.1]);
        let hits = r.retrieve(ids(&["both"]), Some(&[1.0, 0.0]));
        // "both" 两路命中 → Hybrid;"semantic-only" 仅语义命中。
        let both = hits.iter().find(|h| h.id == "both").expect("both");
        assert_eq!(both.source, RetrievalSource::Hybrid);
        let sem = hits.iter().find(|h| h.id == "semantic-only").expect("sem");
        assert_eq!(sem.source, RetrievalSource::Semantic);
    }

    #[test]
    fn hybrid_without_query_vector_uses_keyword_only() {
        let mut r = HybridRetriever::new(5);
        r.index_vector("v", vec![1.0, 0.0]);
        let hits = r.retrieve(ids(&["a"]), None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source, RetrievalSource::Keyword);
    }

    #[test]
    fn hybrid_zero_limit_defaults_to_ten() {
        let r = HybridRetriever::new(0);
        assert_eq!(r.limit, 10);
    }
}
