//! FTS5 检索路径(v1.11 M5a):有界 OR 候选召回 + 词项覆盖度门控与
//! 重排 + 统一字符预算装箱。
//!
//! 关键决策(与 plan §6/§7 对齐):
//! - **有界 OR 召回**:长查询的全部词项不做强制 AND —— MATCH 表达式
//!   逐词加引号后 OR 连接,候选按 `bm25()`(列加权:正文 > 标题 >
//!   单字列)排序,`CANDIDATE_CAP` 封顶扫描量;
//! - **过滤在 top-K 之前**:scope / 活跃状态 / 有效时间 / 墓碑全部
//!   进 SQL WHERE,再 LIMIT —— 不出现「先 top-K 后丢弃失效项饿死
//!   有效结果」;
//! - **相关性门控**:Rust 侧重排按查询词项覆盖度(`matched / total`)
//!   过滤,低于 [`MIN_COVERAGE_RATIO`] 的候选**不注入** —— 取消旧
//!   路径的「小库全量 / 零分补齐」旁路;空查询只返回空串(第一版
//!   无 pinned 概念,见用户可见影响说明);
//! - **分数方向**:FTS5 `bm25()` 为负值且越小越优,进 Rust 后取负
//!   归一为「越大越优」;跨 scope 的原始 bm25 不直接比较,合并按
//!   (覆盖度, 标题命中, scope 序, 库内名次)分路融合;
//! - **单字中文查询**:全部词项都是单个 CJK 字时切到 `unigrams` 列,
//!   不做全文扫描。

use rusqlite::{Connection, params};

use super::schema::{canonical_ts, sql_err};
use crate::model::{MemoryError, MemoryScope};

/// 候选扫描上限(单次 FTS 查询;有界 OR 的代价封顶)。
pub(crate) const CANDIDATE_CAP: usize = 200;

/// 覆盖度门限:匹配词项数 / 查询词项总数低于 1/3 的候选不注入。
/// 口径:中文一个词 ≈ 一个 bigram 词项,三词查询命中一词(1/3)视为
/// 有词面证据;四词以上要求命中 ≥2。整数比较(`matched * 3 >= len`)
/// 规避浮点边界。
pub(crate) fn coverage_enough(matched: usize, total: usize) -> bool {
    total == 0 || matched * 3 >= total
}

/// 排序后的候选条目(渲染与装箱共用)。
#[derive(Debug, Clone)]
pub(crate) struct ScoredEntry {
    pub scope: MemoryScope,
    pub title: String,
    pub content: String,
    /// 查询词项覆盖度(0..=1)。
    pub coverage: f64,
    /// 标题含查询词项(子串命中;CJK bigram 天然是原文子串)。
    pub title_hit: bool,
    /// 库内名次(0-based;跨库合并时的库内次序信号)。
    pub rank: usize,
}

/// 单 scope 检索:FTS 召回 + 状态/时间/墓碑过滤 + 覆盖度门控与重排。
/// 返回已按(覆盖度, 标题命中, bm25)排好序的候选。
pub(crate) fn search_scope(
    conn: &Connection,
    scope: MemoryScope,
    query: &str,
    now: &chrono::DateTime<chrono::Utc>,
    cap: usize,
) -> Result<Vec<ScoredEntry>, MemoryError> {
    let query_terms = zhuye_bm25::tokenize(query);
    if query_terms.is_empty() {
        return Ok(Vec::new());
    }
    let match_expr = build_match_expr(&query_terms);
    let now_ts = canonical_ts(*now);
    let mut stmt = conn
        .prepare(
            "SELECT v.title, v.content, bm25(entry_fts, 12.0, 1.0, 6.0) AS b
             FROM entry_fts
             JOIN entry_versions v ON v.version_id = entry_fts.rowid
             WHERE entry_fts MATCH ?1
               AND v.state = 'active'
               AND v.recorded_to IS NULL
               AND (v.valid_from IS NULL OR v.valid_from <= ?2)
               AND (v.valid_to IS NULL OR v.valid_to > ?2)
               AND NOT EXISTS (SELECT 1 FROM tombstones t WHERE t.title = v.title)
             ORDER BY b
             LIMIT ?3",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![match_expr, now_ts, cap as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sql_err)?;

    let mut scored: Vec<ScoredEntry> = Vec::new();
    for row in rows {
        let (title, content) = row.map_err(sql_err)?;
        // 覆盖度:查询词项出现在条目词项集合中的比例。单字 CJK 词项走
        // 原文包含计数(bigram 集合不含单字,与 unigrams 列口径对齐)。
        let combined = format!("{title} {content}");
        let doc_terms = zhuye_bm25::tokenize(&combined);
        let doc_set: std::collections::HashSet<&String> = doc_terms.iter().collect();
        let matched = query_terms
            .iter()
            .filter(|t| {
                doc_set.contains(t) || (is_single_cjk_term(t) && combined.contains(t.as_str()))
            })
            .count();
        if !coverage_enough(matched, query_terms.len()) {
            continue;
        }
        let coverage = matched as f64 / query_terms.len() as f64;
        // 标题命中:词项子串出现在标题里(CJK bigram 即原文子串)。
        let title_hit = query_terms.iter().any(|t| title.contains(t.as_str()));
        scored.push(ScoredEntry {
            scope,
            title,
            content,
            coverage,
            title_hit,
            rank: 0,
        });
    }
    // 库内重排:覆盖度 > 标题命中 > 标题字典序(确定性排序,相同输入
    // 相同输出;bm25 只在 SQL ORDER BY 里作候选截断信号,不进最终
    // 排序 —— 版本间统计漂移不会让注入顺序抖动、破坏 prompt cache)。
    scored.sort_by(|a, b| {
        b.coverage
            .partial_cmp(&a.coverage)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.title_hit.cmp(&a.title_hit))
            .then(a.title.cmp(&b.title))
    });
    for (idx, entry) in scored.iter_mut().enumerate() {
        entry.rank = idx;
    }
    Ok(scored)
}

/// 词项是否为单个 CJK 字(单字查询走 unigrams 列的判定与覆盖度计数
/// 共用;topic 候选路径也复用)。
pub(crate) fn is_single_cjk_term(term: &str) -> bool {
    let mut chars = term.chars();
    matches!(chars.next(), Some(c) if zhuye_bm25::is_cjk(c)) && chars.next().is_none()
}

/// 构造 FTS5 MATCH 表达式:逐词加引号 + 列过滤。
/// 全部词项为单个 CJK 字时切 `unigrams` 列,否则 `terms` 列。
/// 词项内容只可能是字母数字 / 下划线 / CJK 字符(分词器保证),
/// 引号转义仅为防御未知输入。
fn build_match_expr(query_terms: &[String]) -> String {
    let column = if query_terms.iter().all(|t| is_single_cjk_term(t)) {
        "unigrams"
    } else {
        "terms"
    };
    query_terms
        .iter()
        .map(|t| format!("{column}:\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// 跨 scope 合并 + 预算装箱 + 渲染。
///
/// 合并规则:覆盖度降序 → 标题命中 → scope 输入序(project 优先于
/// user 由调用方 scopes 顺序决定)→ 库内名次。**不比较跨库 bm25**
/// (不同库的统计基线不同,原始分数不可比 —— 分路融合)。
/// 装箱:按完整事实装箱(标题 + 正文 + 8 字符头开销),装不下的
/// 跳过并继续尝试较短候选;总预算为 Unicode 字符数。
pub(crate) fn pack_and_render(
    per_scope: Vec<Vec<ScoredEntry>>,
    scopes: &[MemoryScope],
    budget: usize,
) -> String {
    let mut merged: Vec<&ScoredEntry> = per_scope.iter().flatten().collect();
    let scope_order = |s: MemoryScope| scopes.iter().position(|x| *x == s).unwrap_or(usize::MAX);
    merged.sort_by(|a, b| {
        b.coverage
            .partial_cmp(&a.coverage)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.title_hit.cmp(&a.title_hit))
            .then(scope_order(a.scope).cmp(&scope_order(b.scope)))
            .then(a.rank.cmp(&b.rank))
            .then(a.title.cmp(&b.title))
    });

    let mut picked: Vec<&ScoredEntry> = Vec::new();
    let mut used = 0usize;
    for entry in merged {
        let len = entry.title.chars().count() + entry.content.chars().count() + 8;
        if used + len > budget {
            continue;
        }
        picked.push(entry);
        used += len;
    }
    if picked.is_empty() {
        return String::new();
    }
    let multi_scope = picked
        .iter()
        .map(|e| e.scope)
        .collect::<std::collections::HashSet<_>>()
        .len()
        > 1;
    let mut parts: Vec<String> = Vec::new();
    for entry in picked {
        let rendered = format!("## {}\n{}", entry.title, entry.content);
        if multi_scope {
            parts.push(format!("### {} memory\n\n{}", entry.scope, rendered));
        } else {
            parts.push(rendered);
        }
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::change::{ChangeAction, MemoryChange, apply_changes};
    use crate::sqlite::schema::{apply_pragmas, init};
    use chrono::Duration;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        apply_pragmas(&conn).unwrap();
        init(&conn).unwrap();
        conn
    }

    fn seed(conn: &mut Connection, changes: Vec<MemoryChange>) {
        apply_changes(conn, &changes, None).unwrap();
    }

    #[test]
    fn chinese_query_hits_and_english_still_works() {
        let mut conn = setup();
        seed(
            &mut conn,
            vec![
                MemoryChange::add("数据库", "项目使用 postgres 16,连接池 pgbouncer", "test"),
                MemoryChange::add("代码风格", "提交前必须跑 cargo fmt", "test"),
            ],
        );
        let now = chrono::Utc::now();
        let hits = search_scope(
            &conn,
            MemoryScope::Project,
            "数据库连接池",
            &now,
            CANDIDATE_CAP,
        )
        .unwrap();
        assert!(!hits.is_empty(), "中文查询必须召回中文条目");
        assert_eq!(hits[0].title, "数据库");
        assert!(hits[0].coverage >= 0.5);

        let hits = search_scope(
            &conn,
            MemoryScope::Project,
            "cargo fmt before commit",
            &now,
            CANDIDATE_CAP,
        )
        .unwrap();
        assert!(
            hits.iter().any(|h| h.title == "代码风格"),
            "英文词项召回不回归"
        );
    }

    #[test]
    fn single_cjk_char_query_uses_unigrams() {
        let mut conn = setup();
        seed(
            &mut conn,
            vec![MemoryChange::add(
                "索引策略",
                "倒排索引按 bigram 建 立",
                "test",
            )],
        );
        let now = chrono::Utc::now();
        let hits = search_scope(&conn, MemoryScope::Project, "引", &now, CANDIDATE_CAP).unwrap();
        assert!(!hits.is_empty(), "单字查询走 unigrams 列必须可检索");
    }

    #[test]
    fn unrelated_query_gated_to_empty() {
        let mut conn = setup();
        seed(
            &mut conn,
            vec![MemoryChange::add("数据库", "postgres 16", "test")],
        );
        let now = chrono::Utc::now();
        // 多词项查询覆盖度不足 → 门控为空(取消零分补齐)。
        let hits = search_scope(
            &conn,
            MemoryScope::Project,
            "部署 流水线 审批 门禁",
            &now,
            CANDIDATE_CAP,
        )
        .unwrap();
        assert!(hits.is_empty(), "低覆盖候选不得注入:{hits:?}");
    }

    #[test]
    fn time_window_filters_future_and_expired() {
        let mut conn = setup();
        let now = chrono::Utc::now();
        let mut future = MemoryChange::add("迁移计划", "尚未生效的迁移方案", "test");
        future.valid_from = Some(super::super::schema::canonical_ts(now + Duration::days(7)));
        let mut expired = MemoryChange::add("旧机房", "已下线的机房信息", "test");
        expired.valid_to = Some(super::super::schema::canonical_ts(now - Duration::days(1)));
        seed(
            &mut conn,
            vec![
                future,
                expired,
                MemoryChange::add("在线配置", "当前生效", "test"),
            ],
        );

        let hits = search_scope(
            &conn,
            MemoryScope::Project,
            "迁移 机房 配置",
            &now,
            CANDIDATE_CAP,
        )
        .unwrap();
        let titles: Vec<&str> = hits.iter().map(|h| h.title.as_str()).collect();
        assert!(
            titles.contains(&"在线配置"),
            "当前有效条目可召回:{titles:?}"
        );
        assert!(!titles.contains(&"迁移计划"), "未来事实不得注入");
        assert!(!titles.contains(&"旧机房"), "过期事实不得注入");
    }

    #[test]
    fn deleted_entry_not_recalled_even_with_stale_fts_posting() {
        let mut conn = setup();
        seed(
            &mut conn,
            vec![MemoryChange::add(
                "部署方式",
                "deploy via github actions",
                "test",
            )],
        );
        let mut delete = MemoryChange::add("部署方式", String::new(), "test");
        delete.action = ChangeAction::Delete;
        seed(&mut conn, vec![delete]);
        // posting 仍在(首版不删 posting),靠 WHERE 过滤。
        let posting: i64 = conn
            .query_row("SELECT count(*) FROM entry_fts", [], |r| r.get(0))
            .unwrap();
        assert!(posting > 0, "前提:posting 保留");
        let now = chrono::Utc::now();
        let hits = search_scope(
            &conn,
            MemoryScope::Project,
            "deploy github actions",
            &now,
            CANDIDATE_CAP,
        )
        .unwrap();
        assert!(hits.is_empty(), "墓碑条目不得经 FTS 复活");
    }

    #[test]
    fn budget_packing_skips_oversize_keeps_shorter() {
        let mut conn = setup();
        seed(
            &mut conn,
            vec![
                MemoryChange::add("长条目", "x".repeat(600), "test"),
                MemoryChange::add("短条目", "postgres", "test"),
                MemoryChange::add("相关短条目", "postgres 连接池", "test"),
            ],
        );
        let now = chrono::Utc::now();
        let hits = search_scope(
            &conn,
            MemoryScope::Project,
            "postgres 连接池",
            &now,
            CANDIDATE_CAP,
        )
        .unwrap();
        let out = pack_and_render(vec![hits], &[MemoryScope::Project], 200);
        assert!(out.contains("相关短条目"), "命中条目必须在预算内保留");
        assert!(!out.contains("长条目"), "无关超长条目不占预算");
    }

    #[test]
    fn match_expr_escapes_and_selects_column() {
        assert_eq!(
            build_match_expr(&["数据".to_string(), "据库".to_string()]),
            "terms:\"数据\" OR terms:\"据库\""
        );
        assert_eq!(build_match_expr(&["库".to_string()]), "unigrams:\"库\"");
    }
}
