//! SQLite 条目版本库的 schema 与元信息访问(v1.11 M5a)。
//!
//! 设计要点(与 plan §3/§4 对齐):
//! - `entry_versions` 是**版本表**:一行一个条目版本,`state` 区分
//!   active / superseded / invalidated,当前版本 `recorded_to IS NULL`;
//!   内容更新 = 同事务关闭旧版本 + 追加新版本,不就地覆盖;
//! - `tombstones` 记录用户显式删除(含删除代次),默认查询与自动导入
//!   都不得复活墓碑条目;
//! - `entry_fts` 为 contentless FTS5 索引(rowid = version_id),
//!   预分词列(terms = CJK bigram + ASCII 词,unigrams 供单字查询,
//!   title_terms 标题加权);首版**不做 posting 删除**,软失效靠
//!   查询侧 state/时间过滤(空间与统计成本见设计文档边界说明);
//! - `meta` 存 schema/tokenizer 版本、库 revision、投影 hash —— 升级
//!   与重建的依据。

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, params};

use crate::model::MemoryError;

/// 当前 schema 版本。破坏性变更时递增并在打开时走迁移/重建路径。
pub(crate) const SCHEMA_VERSION: u64 = 1;

/// 分词器版本。索引与查询共用同一实现;改动词法(如 bigram 策略)时
/// 递增本版本,FTS 索引需要按此判断是否重建。
pub(crate) const TOKENIZER_VERSION: &str = "cjk-bigram-ascii-v1";

/// DDL。所有语句幂等(IF NOT EXISTS),重复打开安全。
const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS entry_versions (
    version_id    INTEGER PRIMARY KEY,
    entry_id      TEXT    NOT NULL,
    title         TEXT    NOT NULL,
    content       TEXT    NOT NULL,
    kind          TEXT    NOT NULL DEFAULT 'fact',
    state         TEXT    NOT NULL DEFAULT 'active',
    valid_from    TEXT,
    valid_to      TEXT,
    recorded_from TEXT    NOT NULL,
    recorded_to   TEXT,
    supersedes    INTEGER,
    content_hash  TEXT    NOT NULL,
    revision      INTEGER NOT NULL,
    source        TEXT    NOT NULL,
    reason        TEXT
);
CREATE INDEX IF NOT EXISTS idx_versions_entry ON entry_versions(entry_id);
-- 单活跃不变量:同一标题同时至多一个 active 当前版本(数据库层兜底,
-- 与变更端口的事务校验双保险)。
CREATE UNIQUE INDEX IF NOT EXISTS idx_versions_active_title
    ON entry_versions(title)
    WHERE state = 'active' AND recorded_to IS NULL;

CREATE TABLE IF NOT EXISTS tombstones (
    title      TEXT PRIMARY KEY,
    entry_id   TEXT NOT NULL,
    generation INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    reason     TEXT
);

-- contentless FTS5:rowid 对齐 version_id,只存词项不存原文;
-- 软失效不删 posting,查询侧按 state/时间/墓碑过滤。
CREATE VIRTUAL TABLE IF NOT EXISTS entry_fts USING fts5(
    terms, unigrams, title_terms,
    content = '',
    tokenize = "unicode61 tokenchars '_'"
);
"#;

/// 打开后的连接级 PRAGMA:WAL(读写并发)、busy timeout(跨进程写竞争
/// 有界等待)、外键约束。`synchronous=NORMAL` 是 WAL 下的常规取舍。
/// 内存库(`:memory:`)不支持 WAL,journal_mode 保持 `memory` —— 单元
/// 测试与内存库场景合法放行;仅当文件库切 WAL 失败时视为错误。
pub(crate) fn apply_pragmas(conn: &Connection) -> Result<(), MemoryError> {
    let mode: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .map_err(sql_err)?;
    if !mode.eq_ignore_ascii_case("wal") && !mode.eq_ignore_ascii_case("memory") {
        return Err(MemoryError::Sqlite(format!(
            "journal_mode=WAL 未能生效(当前 {mode};文件可能在不支持 WAL 的介质上)"
        )));
    }
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(sql_err)?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(sql_err)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(sql_err)?;
    Ok(())
}

/// 建表 + 写入初始 meta(schema / tokenizer 版本)。幂等。
pub(crate) fn init(conn: &Connection) -> Result<(), MemoryError> {
    conn.execute_batch(SCHEMA_SQL).map_err(sql_err)?;
    let mut prepared = conn
        .prepare("INSERT OR IGNORE INTO meta(key, value) VALUES (?1, ?2)")
        .map_err(sql_err)?;
    prepared
        .execute(params!["schema_version", SCHEMA_VERSION.to_string()])
        .map_err(sql_err)?;
    prepared
        .execute(params!["tokenizer_version", TOKENIZER_VERSION.to_string()])
        .map_err(sql_err)?;
    Ok(())
}

/// 读取 meta 值;库 revision 用 [`revision`] 便捷封装。
pub(crate) fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>, MemoryError> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .map(Some)
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(sql_err(other)),
    })
}

/// 写 meta 值(UPSERT)。
pub(crate) fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<(), MemoryError> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// 当前库 revision(单调递增,每次生效变更 +1);空库为 0。
pub(crate) fn revision(conn: &Connection) -> Result<u64, MemoryError> {
    Ok(meta_get(conn, "revision")?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0))
}

/// 规范化时间戳:UTC + 毫秒 + `Z` 后缀 —— 定宽 RFC3339,字符串字典序
/// 与时间序一致(valid_from/to 的 SQL 比较依赖这一点)。
pub(crate) fn canonical_ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// 统一的 SQLite 错误包装。
pub(crate) fn sql_err(e: rusqlite::Error) -> MemoryError {
    MemoryError::Sqlite(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent_and_meta_roundtrips() {
        let conn = Connection::open_in_memory().unwrap();
        apply_pragmas(&conn).unwrap();
        init(&conn).unwrap();
        init(&conn).unwrap();
        assert_eq!(
            meta_get(&conn, "schema_version").unwrap().as_deref(),
            Some("1"),
            "schema_version 应已写入"
        );
        assert_eq!(revision(&conn).unwrap(), 0, "空库 revision 为 0");
        meta_set(&conn, "revision", "3").unwrap();
        assert_eq!(revision(&conn).unwrap(), 3);
    }

    #[test]
    fn canonical_ts_is_fixed_width_and_ordered() {
        let a = canonical_ts(Utc::now());
        let b = canonical_ts(Utc::now() + chrono::Duration::milliseconds(1));
        assert!(a.ends_with('Z') && b.ends_with('Z'));
        assert!(a.len() == b.len(), "定宽保证字典序可比:{a} vs {b}");
        assert!(a < b, "时间后者字典序更大");
    }
}
