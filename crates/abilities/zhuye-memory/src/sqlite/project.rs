//! MEMORY.md 投影:幂等导入 + 可靠导出 + 外部编辑检测
//! (v1.11 M5a,plan §9 的最小实现)。
//!
//! 真相源关系:**SQLite 是条目权威库,MEMORY.md 是可编辑投影**。
//! - 打开时:库为空 → 首次导入(幂等);投影 hash 与磁盘不一致 →
//!   外部编辑导入(三方比较的最小版:同名更新 / 新增导入 / **缺失
//!   条目不删除**—— 只有干净基线上的明确删除才允许转墓碑,本版
//!   一律保留并报告);
//! - 提交后:活跃条目渲染为 `## <title>\n<content>` 投影,临时文件
//!   原子替换;导出失败置 `projection_dirty = 1`(可观测,检索仍以
//!   DB 为准),下次成功导出清除。

use std::path::Path;

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use super::change::{ChangeAction, MemoryChange, active_by_title, apply_changes, content_hash};
use super::schema::{meta_get, meta_set, sql_err};
use crate::model::{MemoryError, MemoryScope};
use crate::retrieval::sections::split_sections;

/// 导入统计(工具输出 / 日志用)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportStats {
    /// 新导入条目数。
    pub added: usize,
    /// 同名不同文 → 更新为新版本的条目数。
    pub updated: usize,
    /// 内容未变、幂等跳过的条目数。
    pub unchanged: usize,
    /// 被墓碑拦截跳过的条目数(不复活)。
    pub skipped_tombstoned: usize,
    /// 外部编辑中「投影里有、库里活跃但没有」的条目数 —— 本版保留
    /// 不删除,仅报告。
    pub kept_missing: usize,
}

/// 文件字节 hash(SHA-256 十六进制;投影基线比对用)。
pub(crate) fn file_hash(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some(
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

/// 打开库时的对账入口:首次导入或外部编辑导入。
/// 返回 `Some(stats)` 表示发生了导入(调用方记日志);`None` = 库与
/// 投影一致,无事发生。
pub(crate) fn reconcile_on_open(
    conn: &mut Connection,
    memory_md: &Path,
    scope: MemoryScope,
) -> Result<Option<ImportStats>, MemoryError> {
    let Some(disk_hash) = file_hash(memory_md) else {
        return Ok(None); // 无投影文件(全新库 / 纯增量场景)
    };
    let recorded = meta_get(conn, "projection_hash")?;
    let revision_meta = meta_get(conn, "revision")?;
    if revision_meta.is_none() {
        // 全新库:首次导入。
        let stats = import_pass(conn, memory_md, scope, "import")?;
        export_projection(conn, memory_md)?;
        return Ok(Some(stats));
    }
    if recorded.as_deref() != Some(disk_hash.as_str()) {
        // 投影被人改过:增量导入(缺失不删)。
        let stats = import_pass(conn, memory_md, scope, "import:external-edit")?;
        export_projection(conn, memory_md)?;
        return Ok(Some(stats));
    }
    Ok(None)
}

/// 导入一趟:解析 MEMORY.md 条目,与库内活跃条目按(标题, 内容 hash)
/// 对账。幂等:重复执行第二次全部 unchanged。墓碑条目跳过(revive =
/// false —— 导入路径永不复活被删事实)。
fn import_pass(
    conn: &mut Connection,
    memory_md: &Path,
    scope: MemoryScope,
    source: &str,
) -> Result<ImportStats, MemoryError> {
    let Ok(body) = std::fs::read_to_string(memory_md) else {
        return Ok(ImportStats::default());
    };
    if body.trim().is_empty() {
        return Ok(ImportStats::default());
    }
    // 复用读侧的拆节逻辑:无标题引言归入首条(与旧路径同源,不丢内容)。
    let sections = split_sections(scope, &body);
    let mut changes: Vec<MemoryChange> = Vec::new();
    let mut stats = ImportStats::default();
    for section in &sections {
        if section.title.is_empty() {
            // 无标题内容(引言 / 整文件无标题):挂到保留标题下,不静默丢。
            if section.body.trim().is_empty() {
                continue;
            }
            let title = "(imported preamble)".to_string();
            match active_by_title(conn, &title)? {
                Some(existing) if existing.content == section.body => {
                    stats.unchanged += 1;
                }
                Some(_) => {
                    let mut ch = MemoryChange::add(title, section.body.clone(), source);
                    ch.action = ChangeAction::Update;
                    changes.push(ch);
                    stats.updated += 1;
                }
                None => {
                    changes.push(MemoryChange::add(title, section.body.clone(), source));
                    stats.added += 1;
                }
            }
            continue;
        }
        let hash = content_hash(section.body.trim_end());
        match active_by_title(conn, &section.title)? {
            Some(existing) => {
                if content_hash(&existing.content) == hash {
                    stats.unchanged += 1;
                } else {
                    // 同名不同文:外部编辑 → 追加新版本(保留被替代版本)。
                    let mut ch =
                        MemoryChange::add(section.title.clone(), section.body.clone(), source);
                    ch.action = ChangeAction::Update;
                    changes.push(ch);
                    stats.updated += 1;
                }
            }
            None => {
                let mut ch = MemoryChange::add(section.title.clone(), section.body.clone(), source);
                ch.revive = false;
                changes.push(ch);
                stats.added += 1;
            }
        }
    }
    if changes.is_empty() {
        return Ok(stats);
    }
    let report = apply_changes(conn, &changes, None)?;
    // 墓碑拦截的条目从 added 挪到 skipped_tombstoned。
    for outcome in &report.outcomes {
        if !outcome.applied && outcome.skipped_reason.is_some() {
            stats.added = stats.added.saturating_sub(1);
            stats.skipped_tombstoned += 1;
        }
    }
    Ok(stats)
}

/// 渲染当前活跃条目为投影文本(entry_id = uuid v7,时间有序 → 插入
/// 序稳定,prompt-cache 友好)。
pub(crate) fn render_projection(conn: &Connection) -> Result<String, MemoryError> {
    let mut stmt = conn
        .prepare(
            "SELECT title, content FROM entry_versions
             WHERE state = 'active' AND recorded_to IS NULL
               AND NOT EXISTS (SELECT 1 FROM tombstones t WHERE t.title = entry_versions.title)
             ORDER BY entry_id",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sql_err)?;
    let mut out = String::new();
    for row in rows {
        let (title, content) = row.map_err(sql_err)?;
        out.push_str(&format!("## {title}\n{content}\n\n"));
    }
    let trimmed = out.trim_end();
    if trimmed.is_empty() {
        Ok(String::new())
    } else {
        Ok(format!("{trimmed}\n"))
    }
}

/// 导出投影:渲染 → 原子替换 → 记录基线 hash、清除 dirty 标记。
/// 失败置 `projection_dirty = 1` 并返回错误(调用方降级为日志,检索
/// 仍以 DB 为准 —— DB 已提交而 Markdown 尚旧是可观测的中间态)。
pub(crate) fn export_projection(
    conn: &mut Connection,
    memory_md: &Path,
) -> Result<(), MemoryError> {
    let text = render_projection(conn)?;
    let hash = {
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    if let Some(parent) = memory_md.parent() {
        std::fs::create_dir_all(parent)?;
    }
    zhuye_fs::write_atomic(memory_md, text.as_bytes())?;
    let tx = conn.transaction().map_err(sql_err)?;
    meta_set(&tx, "projection_hash", &hash)?;
    meta_set(&tx, "projection_dirty", "0")?;
    tx.commit().map_err(sql_err)?;
    Ok(())
}

/// 投影导出失败后的补偿路径:置 dirty 标记(可观测)。
pub(crate) fn mark_projection_dirty(conn: &Connection) {
    if let Err(e) = meta_set(conn, "projection_dirty", "1") {
        tracing::warn!(error = %e, "记忆投影 dirty 标记写入失败");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::schema::{apply_pragmas, init, meta_get};

    fn setup(dir: &std::path::Path) -> Connection {
        let conn = Connection::open(dir.join("memory.db")).unwrap();
        apply_pragmas(&conn).unwrap();
        init(&conn).unwrap();
        conn
    }

    #[test]
    fn initial_import_then_idempotent_rerun() {
        let dir = tempfile::tempdir().unwrap();
        let md = dir.path().join("MEMORY.md");
        std::fs::write(
            &md,
            "## 数据库\n项目使用 postgres 16\n\n## 部署\n走 github actions\n",
        )
        .unwrap();
        let mut conn = setup(dir.path());
        let stats = reconcile_on_open(&mut conn, &md, MemoryScope::Project)
            .unwrap()
            .expect("首次应导入");
        assert_eq!(stats.added, 2);
        assert_eq!(stats.unchanged, 0);

        // 投影已导出;再次对账 → 无事发生(幂等)。
        let again = reconcile_on_open(&mut conn, &md, MemoryScope::Project).unwrap();
        assert!(again.is_none(), "hash 一致不得重复导入");
        // 手动再跑 import_pass:全部 unchanged。
        let direct = import_pass(&mut conn, &md, MemoryScope::Project, "import").unwrap();
        assert_eq!(direct.added, 0);
        assert_eq!(direct.unchanged, 2);
    }

    #[test]
    fn external_edit_updates_version_and_keeps_missing() {
        let dir = tempfile::tempdir().unwrap();
        let md = dir.path().join("MEMORY.md");
        std::fs::write(&md, "## 数据库\npostgres 16\n").unwrap();
        let mut conn = setup(dir.path());
        reconcile_on_open(&mut conn, &md, MemoryScope::Project).unwrap();

        // 外部编辑:改一条、删一条(从文件里消失)、加一条。
        std::fs::write(&md, "## 数据库\npostgres 17\n\n## 新条目\nhello\n").unwrap();
        let stats = reconcile_on_open(&mut conn, &md, MemoryScope::Project)
            .unwrap()
            .expect("hash 不一致应触发外部编辑导入");
        assert_eq!(stats.updated, 1, "同名不同文 → 新版本");
        assert_eq!(stats.added, 1, "新标题 → 导入");
        // 被外部删除的条目仍在库中活跃(不静默删除)。
        assert!(active_by_title(&conn, "不存在才对").unwrap().is_none());
        let text = render_projection(&conn).unwrap();
        assert!(text.contains("postgres 17"));
    }

    #[test]
    fn tombstoned_title_not_resurrected_by_import() {
        let dir = tempfile::tempdir().unwrap();
        let md = dir.path().join("MEMORY.md");
        std::fs::write(&md, "## 部署\ngithub actions\n").unwrap();
        let mut conn = setup(dir.path());
        reconcile_on_open(&mut conn, &md, MemoryScope::Project).unwrap();

        // 用户删除 → 墓碑;随后外部把同样内容改回投影(旧快照 / 合并)。
        let mut delete = MemoryChange::add("部署", String::new(), "test");
        delete.action = ChangeAction::Delete;
        apply_changes(&mut conn, &[delete], None).unwrap();
        std::fs::write(&md, "## 部署\ngithub actions\n## 其他\n内容\n").unwrap();
        let stats = reconcile_on_open(&mut conn, &md, MemoryScope::Project)
            .unwrap()
            .expect("hash 变化触发导入");
        assert_eq!(stats.skipped_tombstoned, 1, "墓碑条目不得被导入复活");
        assert_eq!(stats.added, 1);
        assert!(active_by_title(&conn, "部署").unwrap().is_none());
    }

    #[test]
    fn export_writes_atomic_and_records_hash() {
        let dir = tempfile::tempdir().unwrap();
        let md = dir.path().join("MEMORY.md");
        let mut conn = setup(dir.path());
        apply_changes(
            &mut conn,
            &[MemoryChange::add("条目", "内容", "test")],
            None,
        )
        .unwrap();
        export_projection(&mut conn, &md).unwrap();
        let body = std::fs::read_to_string(&md).unwrap();
        assert_eq!(body, "## 条目\n内容\n");
        assert!(meta_get(&conn, "projection_hash").unwrap().is_some());
        assert_eq!(
            meta_get(&conn, "projection_dirty").unwrap().as_deref(),
            Some("0")
        );
    }
}
