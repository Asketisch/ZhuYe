//! 事务变更端口(v1.11 M5a):结构化 ADD / UPDATE / DELETE 在**同一个
//! SQLite 事务**内提交 —— 条目版本、FTS posting、库 revision 的原子性
//! 由事务保证;`expected_revision` 提供跨进程乐观并发控制(CAS),
//! 冲突时整体回滚并返回 [`MemoryError::RevisionConflict`],调用方须
//! 重读后重新决策,不得套用旧结论。
//!
//! 语义(plan §5 的 M5a 子集,模型自动写入在 M5b):
//! - **Add**:新 entry_id + 新版本;同名活跃条目已存在 → `EntryExists`;
//!   标题命中墓碑 → 默认跳过(`Tombstoned` 结果),仅 `revive = true`
//!   (显式授权,如工具写入经 Prompt 审批)可清除墓碑重建,留痕于
//!   `source`;
//! - **Update**:关闭旧版本(`state = superseded`)+ 追加新版本
//!   (`supersedes` 指向前驱),entry_id 沿用 —— 事实演进有版本链;
//! - **Delete**:软失效(`state = invalidated`)+ 写入墓碑(含删除
//!   代次 = 新 revision);条目本就不存在 → 幂等跳过,不报错。

use chrono::Utc;
use rusqlite::{Connection, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use super::schema::{canonical_ts, meta_set, revision, sql_err};
use crate::model::MemoryError;

/// 条目类别(plan §3:至少区分事实 / 偏好 / 决策 / 事件)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EntryKind {
    /// 客观事实(默认;导入的旧 MEMORY.md 条目都归此类)。
    #[default]
    Fact,
    /// 用户偏好。
    Preference,
    /// 决策记录。
    Decision,
    /// 事件(某时点发生,天然带时间语义)。
    Event,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::Fact => "fact",
            EntryKind::Preference => "preference",
            EntryKind::Decision => "decision",
            EntryKind::Event => "event",
        }
    }

    /// 解析失败回落 Fact(库内 kind 未知值不炸读取路径)。
    pub fn parse(s: &str) -> Self {
        match s {
            "preference" => EntryKind::Preference,
            "decision" => EntryKind::Decision,
            "event" => EntryKind::Event,
            _ => EntryKind::Fact,
        }
    }
}

/// 变更动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeAction {
    Add,
    Update,
    Delete,
}

/// 一条结构化变更提案。模型 / 导入 / 工具都构造此结构,经
/// [`apply_changes`] 统一提交 —— 唯一权威写入链。
#[derive(Debug, Clone)]
pub struct MemoryChange {
    pub action: ChangeAction,
    /// 条目标题(唯一活跃键;即旧 MEMORY.md 的 `## <key>`)。
    pub title: String,
    /// 条目正文(Delete 忽略)。
    pub content: String,
    pub kind: EntryKind,
    /// 现实有效区间(RFC3339;`None` = 立即生效 / 未关闭)。
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    /// 来源标识(tool / import / import:external-edit / revive …)。
    pub source: String,
    pub reason: Option<String>,
    /// 显式越过墓碑(须经用户授权的路径才允许置 true)。
    pub revive: bool,
}

impl MemoryChange {
    /// 便捷构造:新增事实条目。
    pub fn add(title: impl Into<String>, content: impl Into<String>, source: &str) -> Self {
        Self {
            action: ChangeAction::Add,
            title: title.into(),
            content: content.into(),
            kind: EntryKind::Fact,
            valid_from: None,
            valid_to: None,
            source: source.to_string(),
            reason: None,
            revive: false,
        }
    }
}

/// 单条变更的执行结果。
#[derive(Debug, Clone)]
pub struct ChangeOutcome {
    pub title: String,
    pub applied: bool,
    /// 未生效原因(墓碑跳过 / 目标不存在等);成功时为 `None`。
    pub skipped_reason: Option<String>,
    pub entry_id: Option<String>,
    pub version_id: Option<i64>,
}

/// 一批变更的事务报告。
#[derive(Debug, Clone)]
pub struct ChangeReport {
    pub revision_before: u64,
    pub revision_after: u64,
    pub outcomes: Vec<ChangeOutcome>,
}

/// 待插入的新版本(字段聚合,避免插入函数长参数表)。
struct NewVersion {
    entry_id: String,
    title: String,
    content: String,
    kind: EntryKind,
    valid_from: Option<String>,
    valid_to: Option<String>,
    supersedes: Option<i64>,
    source: String,
    reason: Option<String>,
}

/// 活跃条目的最小快照(变更决策用)。
pub(crate) struct ActiveRow {
    pub version_id: i64,
    pub entry_id: String,
    pub content: String,
}

/// content 的规范化 hash(SHA-256 十六进制;幂等导入的比对键)。
pub(crate) fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// FTS 预分词列:terms(CJK bigram + ASCII 词,检索主列)。
pub(crate) fn index_terms(text: &str) -> String {
    zhuye_bm25::tokenize(text).join(" ")
}

/// FTS 单字列:CJK 逐字 + ASCII 词,供单字中文查询(计划 §6:单字走
/// 独立 unigram 字段,仅按需启用)。
pub(crate) fn index_unigrams(text: &str) -> String {
    let mut terms: Vec<String> = Vec::new();
    let mut ascii_run = String::new();
    for ch in text.chars() {
        let folded = zhuye_bm25::fold_fullwidth(ch);
        if folded.is_ascii_alphanumeric() || folded == '_' {
            ascii_run.push(folded.to_ascii_lowercase());
        } else {
            if !ascii_run.is_empty() {
                terms.push(std::mem::take(&mut ascii_run));
            }
            if zhuye_bm25::is_cjk(folded) {
                terms.push(folded.to_string());
            }
        }
    }
    if !ascii_run.is_empty() {
        terms.push(ascii_run);
    }
    terms.join(" ")
}

/// 批量提交结构化变更(单事务 + 可选 revision CAS)。
pub(crate) fn apply_changes(
    conn: &mut Connection,
    changes: &[MemoryChange],
    expected_revision: Option<u64>,
) -> Result<ChangeReport, MemoryError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_err)?;
    let rev_before = revision(&tx)?;
    if let Some(expected) = expected_revision
        && expected != rev_before
    {
        // 整体回滚:调用方重读库状态后重新决策。
        return Err(MemoryError::RevisionConflict {
            expected,
            actual: rev_before,
        });
    }
    let now = canonical_ts(Utc::now());
    let next_rev = rev_before + 1;
    let mut outcomes = Vec::with_capacity(changes.len());
    let mut applied = 0usize;
    for change in changes {
        let outcome = apply_one(&tx, change, &now, next_rev)?;
        if outcome.applied {
            applied += 1;
        }
        outcomes.push(outcome);
    }
    let rev_after = if applied > 0 { next_rev } else { rev_before };
    if applied > 0 {
        meta_set(&tx, "revision", &rev_after.to_string())?;
    }
    tx.commit().map_err(sql_err)?;
    Ok(ChangeReport {
        revision_before: rev_before,
        revision_after: rev_after,
        outcomes,
    })
}

/// 单条变更执行(事务内)。返回跳过而非报错的路径:墓碑拦截与幂等
/// 删除 —— 批量导入不因单条被拦截而整批失败。
fn apply_one(
    tx: &rusqlite::Transaction<'_>,
    change: &MemoryChange,
    now: &str,
    next_rev: u64,
) -> Result<ChangeOutcome, MemoryError> {
    match change.action {
        ChangeAction::Add => {
            if let Some(ts) = tombstone_of(tx, &change.title)? {
                if !change.revive {
                    return Ok(ChangeOutcome {
                        title: change.title.clone(),
                        applied: false,
                        skipped_reason: Some(format!(
                            "tombstoned(generation {});需显式授权才能复活",
                            ts.generation
                        )),
                        entry_id: None,
                        version_id: None,
                    });
                }
                // 显式复活:清除墓碑行,历史版本保持 invalidated 不动,
                // 新条目以新 entry_id 重建(source 记录复活来源)。
                tx.execute(
                    "DELETE FROM tombstones WHERE title = ?1",
                    params![change.title],
                )
                .map_err(sql_err)?;
            }
            if active_by_title(tx, &change.title)?.is_some() {
                return Err(MemoryError::EntryExists {
                    key: change.title.clone(),
                });
            }
            let entry_id = uuid::Uuid::now_v7().to_string();
            let version_id = insert_version(
                tx,
                next_rev,
                now,
                &NewVersion {
                    entry_id: entry_id.clone(),
                    title: change.title.clone(),
                    content: change.content.clone(),
                    kind: change.kind,
                    valid_from: change.valid_from.clone(),
                    valid_to: change.valid_to.clone(),
                    supersedes: None,
                    source: change.source.clone(),
                    reason: change.reason.clone(),
                },
            )?;
            Ok(ChangeOutcome {
                title: change.title.clone(),
                applied: true,
                skipped_reason: None,
                entry_id: Some(entry_id),
                version_id: Some(version_id),
            })
        }
        ChangeAction::Update => {
            let Some(current) = active_by_title(tx, &change.title)? else {
                return Err(MemoryError::EntryNotFound {
                    key: change.title.clone(),
                });
            };
            close_version(tx, current.version_id, "superseded", now)?;
            let version_id = insert_version(
                tx,
                next_rev,
                now,
                &NewVersion {
                    entry_id: current.entry_id.clone(),
                    title: change.title.clone(),
                    content: change.content.clone(),
                    kind: change.kind,
                    valid_from: change.valid_from.clone(),
                    valid_to: change.valid_to.clone(),
                    supersedes: Some(current.version_id),
                    source: change.source.clone(),
                    reason: change.reason.clone(),
                },
            )?;
            Ok(ChangeOutcome {
                title: change.title.clone(),
                applied: true,
                skipped_reason: None,
                entry_id: Some(current.entry_id),
                version_id: Some(version_id),
            })
        }
        ChangeAction::Delete => {
            let Some(current) = active_by_title(tx, &change.title)? else {
                // 幂等删除:目标不存在(已删 / 从未存在)不报错。
                return Ok(ChangeOutcome {
                    title: change.title.clone(),
                    applied: false,
                    skipped_reason: Some("no-active-entry".into()),
                    entry_id: None,
                    version_id: None,
                });
            };
            close_version(tx, current.version_id, "invalidated", now)?;
            tx.execute(
                "INSERT INTO tombstones(title, entry_id, generation, created_at, reason)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(title) DO UPDATE SET
                    entry_id = excluded.entry_id,
                    generation = excluded.generation,
                    created_at = excluded.created_at,
                    reason = excluded.reason",
                params![
                    change.title,
                    current.entry_id,
                    next_rev as i64,
                    now,
                    change.reason,
                ],
            )
            .map_err(sql_err)?;
            Ok(ChangeOutcome {
                title: change.title.clone(),
                applied: true,
                skipped_reason: None,
                entry_id: Some(current.entry_id),
                version_id: Some(current.version_id),
            })
        }
    }
}

/// 墓碑记录快照。
struct TombstoneRow {
    generation: i64,
}

fn tombstone_of(
    tx: &rusqlite::Transaction<'_>,
    title: &str,
) -> Result<Option<TombstoneRow>, MemoryError> {
    tx.query_row(
        "SELECT generation FROM tombstones WHERE title = ?1",
        params![title],
        |row| {
            Ok(TombstoneRow {
                generation: row.get(0)?,
            })
        },
    )
    .map(Some)
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(sql_err(other)),
    })
}

/// 按标题取当前活跃版本(单活跃不变量由部分唯一索引兜底)。
pub(crate) fn active_by_title(
    conn: &Connection,
    title: &str,
) -> Result<Option<ActiveRow>, MemoryError> {
    conn.query_row(
        "SELECT version_id, entry_id, content FROM entry_versions
         WHERE title = ?1 AND state = 'active' AND recorded_to IS NULL",
        params![title],
        |row| {
            Ok(ActiveRow {
                version_id: row.get(0)?,
                entry_id: row.get(1)?,
                content: row.get(2)?,
            })
        },
    )
    .map(Some)
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(sql_err(other)),
    })
}

/// 关闭一个版本(记录事务时间区间右端;state 见调用点)。
fn close_version(
    tx: &rusqlite::Transaction<'_>,
    version_id: i64,
    state: &str,
    now: &str,
) -> Result<(), MemoryError> {
    tx.execute(
        "UPDATE entry_versions SET state = ?2, recorded_to = ?3 WHERE version_id = ?1",
        params![version_id, state, now],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// 插入新版本 + FTS posting(同事务)。
fn insert_version(
    tx: &rusqlite::Transaction<'_>,
    rev: u64,
    now: &str,
    v: &NewVersion,
) -> Result<i64, MemoryError> {
    tx.execute(
        "INSERT INTO entry_versions(
            entry_id, title, content, kind, state,
            valid_from, valid_to, recorded_from, recorded_to, supersedes,
            content_hash, revision, source, reason)
         VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6, ?7, NULL, ?8, ?9, ?10, ?11, ?12)",
        params![
            v.entry_id,
            v.title,
            v.content,
            v.kind.as_str(),
            v.valid_from,
            v.valid_to,
            now,
            v.supersedes,
            content_hash(&v.content),
            rev as i64,
            v.source,
            v.reason,
        ],
    )
    .map_err(sql_err)?;
    let version_id = tx.last_insert_rowid();
    insert_fts(tx, version_id, &v.title, &v.content)?;
    Ok(version_id)
}

/// FTS posting 写入(rowid = version_id;首版不删除 posting,见模块文档)。
fn insert_fts(
    tx: &rusqlite::Transaction<'_>,
    version_id: i64,
    title: &str,
    content: &str,
) -> Result<(), MemoryError> {
    let combined = format!("{title} {content}");
    tx.execute(
        "INSERT INTO entry_fts(rowid, terms, unigrams, title_terms) VALUES (?1, ?2, ?3, ?4)",
        params![
            version_id,
            index_terms(&combined),
            index_unigrams(&combined),
            index_terms(title),
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::schema::{apply_pragmas, init};

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        apply_pragmas(&conn).unwrap();
        init(&conn).unwrap();
        conn
    }

    #[test]
    fn add_update_delete_lifecycle() {
        let mut conn = setup();
        let report = apply_changes(
            &mut conn,
            &[MemoryChange::add("数据库", "项目使用 postgres 16", "test")],
            None,
        )
        .unwrap();
        assert_eq!(report.revision_after, 1);
        assert!(report.outcomes[0].applied);

        // Update:旧版本 superseded,新版本 active,版本链对齐。
        let mut update = MemoryChange::add("数据库", "项目使用 postgres 17", "test");
        update.action = ChangeAction::Update;
        let report = apply_changes(&mut conn, &[update], None).unwrap();
        assert_eq!(report.revision_after, 2);
        let (state, supersedes, content): (String, Option<i64>, String) = conn
            .query_row(
                "SELECT state, supersedes, content FROM entry_versions
                 WHERE state = 'active' AND recorded_to IS NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, "active");
        assert_eq!(supersedes, Some(1), "新版本应指向被替代的前驱");
        assert!(content.contains("postgres 17"));
        let old_state: String = conn
            .query_row(
                "SELECT state FROM entry_versions WHERE version_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_state, "superseded");

        // Delete:软失效 + 墓碑;幂等重删不报错。
        let mut delete = MemoryChange::add("数据库", String::new(), "test");
        delete.action = ChangeAction::Delete;
        let report = apply_changes(&mut conn, &[delete.clone()], None).unwrap();
        assert!(report.outcomes[0].applied);
        let report = apply_changes(&mut conn, &[delete], None).unwrap();
        assert!(!report.outcomes[0].applied, "重复删除应幂等跳过");
        assert!(active_by_title(&conn, "数据库").unwrap().is_none());
    }

    #[test]
    fn tombstone_blocks_add_unless_revive() {
        let mut conn = setup();
        // 先建后删:墓碑记录被删条目谱系(不存在的条目删除是幂等跳过,
        // 不产生墓碑 —— 见 delete 幂等语义)。
        apply_changes(
            &mut conn,
            &[MemoryChange::add("部署方式", "github actions", "test")],
            None,
        )
        .unwrap();
        let mut delete = MemoryChange::add("部署方式", String::new(), "test");
        delete.action = ChangeAction::Delete;
        apply_changes(&mut conn, &[delete], None).unwrap();

        // 墓碑拦截:导入路径(revive = false)跳过而非报错。
        let report = apply_changes(
            &mut conn,
            &[MemoryChange::add("部署方式", "github actions", "import")],
            None,
        )
        .unwrap();
        assert!(!report.outcomes[0].applied);
        assert!(report.outcomes[0].skipped_reason.is_some());
        assert_eq!(
            report.revision_after, report.revision_before,
            "全跳过不推进 revision"
        );

        // 显式复活:墓碑清除,新 entry_id 重建。
        let mut revive = MemoryChange::add("部署方式", "github actions", "revive");
        revive.revive = true;
        let report = apply_changes(&mut conn, &[revive], None).unwrap();
        assert!(report.outcomes[0].applied);
        assert!(active_by_title(&conn, "部署方式").unwrap().is_some());
    }

    #[test]
    fn revision_cas_conflicts_reject_batch() {
        let mut conn = setup();
        apply_changes(&mut conn, &[MemoryChange::add("A", "a", "test")], None).unwrap();
        // 期望 revision 过期 → 整批拒绝。
        let err =
            apply_changes(&mut conn, &[MemoryChange::add("B", "b", "test")], Some(0)).unwrap_err();
        assert!(matches!(
            err,
            MemoryError::RevisionConflict {
                expected: 0,
                actual: 1
            }
        ));
        assert!(
            active_by_title(&conn, "B").unwrap().is_none(),
            "冲突批次不得部分写入"
        );
    }

    #[test]
    fn add_on_existing_title_errors() {
        let mut conn = setup();
        apply_changes(&mut conn, &[MemoryChange::add("A", "a", "test")], None).unwrap();
        let err =
            apply_changes(&mut conn, &[MemoryChange::add("A", "a2", "test")], None).unwrap_err();
        assert!(matches!(err, MemoryError::EntryExists { .. }));
    }

    #[test]
    fn index_strings_cover_cjk_and_ascii() {
        let terms = index_terms("使用PostgreSQL数据库");
        assert!(terms.contains("使用"), "terms 应含 CJK bigram:{terms}");
        assert!(terms.contains("postgresql"), "terms 应含 ASCII 词:{terms}");
        let unigrams = index_unigrams("数据库db");
        assert!(unigrams.contains("数"));
        assert!(unigrams.contains("库"));
        assert!(unigrams.contains("db"));
    }
}
