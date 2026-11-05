//! 导入落盘:records → `<base>/YYYY/MM/DD/<thread_id>.jsonl`。
//!
//! 不用 `JsonlRolloutWriter`:它新建文件时以 `Utc::now()` 定日期目录
//! (`writer.rs` ensure_open),而导入的历史会话必须按**源会话
//! started_at** 落到原日期目录,否则 `session ls` 的时间线失真。这里仿
//! `index::fork_with_history` 的「逐行写 + `serialize_redacted`」范式:
//! 复用密钥脱敏 + 16 KiB 字段截断,与原生文件安全边界一致。

use std::io::Write;
use std::path::{Path, PathBuf};

use zhuye_protocol::RolloutRecord;
use zhuye_rollout::index::{find_files_by_name_prefix, find_session_path, rename_session};
use zhuye_rollout::path::session_path_at;
use zhuye_rollout::redact::serialize_redacted;

use crate::error::ImportError;
use crate::model::ImportedSession;

/// 一次成功写入的报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteReport {
    /// 落盘的 JSONL 绝对/给定路径。
    pub path: PathBuf,
    /// 写入的记录条数(含首行 SessionMeta)。
    pub records: usize,
}

/// 把导入 records 写为一个新烛夜会话文件。
///
/// - 路径按 `session.started_at` 的 UTC 日期分桶;
/// - 目标 id 已存在且 `force == false` → [`ImportError::AlreadyExists`];
/// - `force == true` 时原子替换已有文件,保留首次导入的位置;
/// - `session.title` 非空时顺手写 `_names/<id>.name`(即 `session ls`
///   展示的自定义名,与 `/rename` 同机制);
/// - `records` 首元素必须是 `SessionMeta`(断言防御,防调用方乱序)。
pub fn write_session(
    base: &Path,
    session: &ImportedSession,
    records: &[RolloutRecord],
    force: bool,
) -> Result<WriteReport, ImportError> {
    if records.is_empty() {
        return Err(ImportError::Other(
            "records 为空,拒绝写入(至少需要 SessionMeta)".into(),
        ));
    }
    match &records[0] {
        RolloutRecord::SessionMeta { session_id, .. } if *session_id == session.thread_id => {}
        RolloutRecord::SessionMeta { .. } => {
            return Err(ImportError::Other(
                "SessionMeta 的会话 ID 与目标不一致".into(),
            ));
        }
        _ => {
            return Err(ImportError::Other(
                "records 首行必须是 SessionMeta(索引契约)".into(),
            ));
        }
    }

    // 同一会话再次导入时保留已有文件位置,避免源日期改变产生重复会话。
    let existing = find_session_path(base, session.thread_id);
    if !force {
        let id = session.thread_id.to_string();
        let named_session_exists = find_files_by_name_prefix(base, &id)?.iter().any(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == format!("{id}.jsonl"))
                || rotation_stem(path) == Some(id.as_str())
        });
        if existing.is_some() || named_session_exists {
            return Err(ImportError::AlreadyExists(id));
        }
    }
    let path = match existing {
        Some(path) if rotation_stem(&path).is_some() => path
            .parent()
            .ok_or_else(|| ImportError::Other("会话路径缺少父目录".into()))?
            .join(format!(
                "{}.jsonl",
                rotation_stem(&path).expect("已验证轮转名称")
            )),
        Some(path) => path,
        None => session_path_at(base, session.thread_id, session.started_at),
    };
    if force && path.exists() && !has_session_id(&path, session.thread_id)? {
        return Err(ImportError::Other(format!(
            "无法确认目标会话归属,拒绝覆盖: {}",
            path.display()
        )));
    }
    let old_fragments = if force {
        find_old_fragments(base, &path, session.thread_id)?
    } else {
        Vec::new()
    };
    let parent = path
        .parent()
        .ok_or_else(|| ImportError::Other("会话路径缺少父目录".into()))?;
    std::fs::create_dir_all(parent)?;
    replace_with_rollback(&path, &old_fragments, force, session.thread_id, |file| {
        for record in records {
            let line = serialize_redacted(record).map_err(|e| ImportError::Json {
                path: path.display().to_string(),
                detail: format!("记录序列化失败: {e}"),
            })?;
            writeln!(file, "{line}")?;
        }
        Ok(())
    })?;

    // 源显式标题 → `.name` 文件(失败不致命:标题只是展示层)。
    if let Some(title) = session.title.as_deref() {
        if let Err(e) = rename_session(base, session.thread_id, title) {
            tracing::warn!("写入会话标题失败({e}):{}", session.thread_id);
        }
    }

    tracing::info!(
        thread_id = %session.thread_id,
        records = records.len(),
        "导入会话已写入 {}",
        path.display()
    );
    Ok(WriteReport {
        path,
        records: records.len(),
    })
}

/// 数字后缀是轮转片段,返回其活跃文件的名称前缀。
fn rotation_stem(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?.strip_suffix(".jsonl")?;
    let (stem, suffix) = name.rsplit_once('.')?;
    suffix.parse::<u32>().ok().map(|_| stem)
}

fn has_session_id(path: &Path, id: zhuye_protocol::ThreadId) -> Result<bool, ImportError> {
    use std::io::BufRead;
    let mut first = String::new();
    std::io::BufReader::new(std::fs::File::open(path)?).read_line(&mut first)?;
    Ok(matches!(serde_json::from_str::<RolloutRecord>(&first),
        Ok(RolloutRecord::SessionMeta { session_id, .. }) if session_id == id))
}

/// 查找会被回放的旧文件;无法确认归属时拒绝删除。
fn find_old_fragments(
    base: &Path,
    target: &Path,
    id: zhuye_protocol::ThreadId,
) -> Result<Vec<PathBuf>, ImportError> {
    let id_string = id.to_string();
    let target_stem = target
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or(&id_string);
    let mut candidates = std::collections::BTreeSet::new();
    for prefix in [id_string.as_str(), target_stem] {
        candidates.extend(find_files_by_name_prefix(base, prefix)?);
    }
    let mut old = Vec::new();
    for path in candidates {
        let stem = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let relevant = [id_string.as_str(), target_stem]
            .iter()
            .any(|prefix| stem == *prefix || rotation_stem(&path) == Some(*prefix));
        if path == target || !relevant {
            continue;
        }
        if has_session_id(&path, id)? {
            old.push(path);
        } else {
            return Err(ImportError::Other(format!(
                "无法确认旧会话分片归属,拒绝覆盖: {}",
                path.display()
            )));
        }
    }
    Ok(old)
}

/// 强制覆盖时先将旧分片移入同目录备份;替换失败则恢复。
fn replace_with_rollback(
    path: &Path,
    old_fragments: &[PathBuf],
    force: bool,
    thread_id: zhuye_protocol::ThreadId,
    write: impl FnOnce(&mut std::fs::File) -> Result<(), ImportError>,
) -> Result<(), ImportError> {
    let mut backups: Vec<(PathBuf, PathBuf)> = Vec::new();
    let result = (|| {
        for old in old_fragments {
            let parent = old
                .parent()
                .ok_or_else(|| ImportError::Other("旧会话路径缺少父目录".into()))?;
            // keep 后即使进程异常退出,备份也不会由 Drop 删除。
            let dir = tempfile::Builder::new()
                .prefix(".zhuye-import-backup-")
                .tempdir_in(parent)?
                .keep();
            if let Err(error) = std::fs::rename(old, dir.join("fragment")) {
                let _ = std::fs::remove_dir(&dir);
                return Err(ImportError::Io(error));
            }
            backups.push((old.clone(), dir));
        }
        write_staged(path, force, thread_id, write)
    })();
    if let Err(error) = result {
        let mut failures = Vec::new();
        for (original, dir) in backups.into_iter().rev() {
            match std::fs::rename(dir.join("fragment"), &original) {
                Ok(()) => {
                    let _ = std::fs::remove_dir(&dir);
                }
                Err(restore_error) => failures.push(format!("{}: {restore_error}", dir.display())),
            }
        }
        return if failures.is_empty() {
            Err(error)
        } else {
            Err(ImportError::Other(format!(
                "{error}; 恢复旧分片失败,备份保留于 {}",
                failures.join(", ")
            )))
        };
    }
    for (_, dir) in backups {
        if let Err(error) = std::fs::remove_dir_all(&dir) {
            tracing::warn!(path = %dir.display(), %error, "删除导入备份失败");
        }
    }
    Ok(())
}

/// 临时文件写入完成后再替换目标;失败时保留原文件。
fn write_staged(
    path: &Path,
    force: bool,
    thread_id: zhuye_protocol::ThreadId,
    write: impl FnOnce(&mut std::fs::File) -> Result<(), ImportError>,
) -> Result<(), ImportError> {
    let parent = path
        .parent()
        .ok_or_else(|| ImportError::Other("会话路径缺少父目录".into()))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    write(file.as_file_mut())?;
    file.flush()?;
    file.as_file().sync_all()?;
    if force {
        file.persist(path).map_err(|e| ImportError::Io(e.error))?;
    } else {
        file.persist_noclobber(path).map_err(|e| {
            if e.error.kind() == std::io::ErrorKind::AlreadyExists {
                ImportError::AlreadyExists(thread_id.to_string())
            } else {
                ImportError::Io(e.error)
            }
        })?;
    }
    Ok(())
}

/// 同步读 JSONL 并解析为 records(`reader::replay` 是 async,测试里
/// 直接按行解析等价验证落盘往返)。
#[cfg(test)]
pub(crate) fn read_records_sync(path: &Path) -> Vec<RolloutRecord> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<RolloutRecord>(l).unwrap())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::{ConvertOptions, imported_to_records};
    use crate::model::{Entry, HarnessKind, ImportedSession, ImportedTurn, IrBlock, UserBlock};
    use chrono::TimeZone;
    use zhuye_protocol::{MessageRole, ThreadId};

    fn sample_session(
        thread_id: ThreadId,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> ImportedSession {
        let turn = ImportedTurn {
            turn_id: zhuye_protocol::TurnId::new(),
            started_at: None,
            user: vec![UserBlock::Text {
                text: "第一句".into(),
            }],
            assistant: vec![IrBlock::Text {
                text: "第一答".into(),
            }],
            usage: None,
        };
        ImportedSession {
            kind: HarnessKind::ClaudeCode,
            source_id: "src-id".into(),
            thread_id,
            model: "test/model".into(),
            started_at,
            workspace: Some("/w".into()),
            title: Some("测试标题".into()),
            entries: vec![Entry::Turn(turn)],
            warnings: Vec::new(),
        }
    }

    /// 落盘路径按源 started_at 定日期目录;文件可被原生 reader 回放。
    #[test]
    fn writes_to_source_date_dir_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("sessions");
        let started = chrono::Utc.with_ymd_and_hms(2025, 3, 14, 8, 0, 0).unwrap();
        let tid = ThreadId::new();
        let s = sample_session(tid, started);
        let records = imported_to_records(&s, &ConvertOptions::default());

        let report = write_session(&base, &s, &records, false).unwrap();
        assert_eq!(report.records, records.len());
        assert_eq!(
            report.path,
            base.join("2025")
                .join("03")
                .join("14")
                .join(format!("{tid}.jsonl"))
        );

        // 原生 reader 能按 id 找到并回放(索引契约成立)。
        let found = find_session_path(&base, tid).expect("按 id 找不到刚导入的会话");
        assert_eq!(found, report.path);
        let records2 = read_records_sync(&found);
        assert_eq!(records2.len(), records.len());
        assert!(
            matches!(records2[0], RolloutRecord::SessionMeta { session_id, .. } if session_id == tid)
        );
        // 首条 user 消息内容可读(serde 往返无损)。
        assert!(matches!(
            &records2[1],
            RolloutRecord::Message {
                role: MessageRole::User,
                ..
            }
        ));
    }

    /// 重复导入拒绝;force 覆盖。
    #[test]
    fn idempotent_reject_and_force_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("sessions");
        let started = chrono::Utc.with_ymd_and_hms(2025, 3, 14, 8, 0, 0).unwrap();
        let tid = ThreadId::new();
        let s = sample_session(tid, started);
        let records = imported_to_records(&s, &ConvertOptions::default());

        write_session(&base, &s, &records, false).unwrap();
        let err = write_session(&base, &s, &records, false).unwrap_err();
        assert!(matches!(err, ImportError::AlreadyExists(_)));

        write_session(&base, &s, &records, true).unwrap();
        assert_eq!(
            read_records_sync(&find_session_path(&base, tid).unwrap()).len(),
            records.len()
        );
    }

    #[test]
    fn force_reimport_with_new_date_replaces_existing_session() {
        let dir = tempfile::tempdir().unwrap();
        let tid = ThreadId::new();
        let mut session = sample_session(
            tid,
            chrono::Utc.with_ymd_and_hms(2025, 3, 14, 8, 0, 0).unwrap(),
        );
        let records = imported_to_records(&session, &ConvertOptions::default());
        let first = write_session(dir.path(), &session, &records, false).unwrap();
        session.started_at = chrono::Utc.with_ymd_and_hms(2026, 4, 15, 8, 0, 0).unwrap();
        let records = imported_to_records(&session, &ConvertOptions::default());
        let second = write_session(dir.path(), &session, &records, true).unwrap();
        assert_eq!(second.path, first.path);
        assert!(!session_path_at(dir.path(), tid, session.started_at).exists());
        let written = read_records_sync(&first.path);
        assert!(
            matches!(written[0], RolloutRecord::SessionMeta { started_at, .. } if started_at == session.started_at)
        );
    }

    #[test]
    fn force_reimport_replaces_rotations_and_duplicate_active_files() {
        let dir = tempfile::tempdir().unwrap();
        let tid = ThreadId::new();
        let mut session = sample_session(
            tid,
            chrono::Utc.with_ymd_and_hms(2025, 3, 14, 8, 0, 0).unwrap(),
        );
        let records = imported_to_records(&session, &ConvertOptions::default());
        let original = write_session(dir.path(), &session, &records, false)
            .unwrap()
            .path;
        let rotated = original.with_file_name(format!("{tid}.1.jsonl"));
        std::fs::copy(&original, &rotated).unwrap();
        let duplicate = session_path_at(
            dir.path(),
            tid,
            chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        );
        std::fs::create_dir_all(duplicate.parent().unwrap()).unwrap();
        std::fs::copy(&original, &duplicate).unwrap();
        session.model = "updated/model".into();
        let records = imported_to_records(&session, &ConvertOptions::default());
        let report = write_session(dir.path(), &session, &records, true).unwrap();
        assert!(!rotated.exists());
        assert_eq!(
            find_files_by_name_prefix(dir.path(), &tid.to_string()).unwrap(),
            vec![report.path.clone()]
        );
        assert!(
            matches!(&read_records_sync(&report.path)[0], RolloutRecord::SessionMeta { model, .. } if model == "updated/model")
        );
    }

    #[test]
    fn failed_replacement_restores_old_fragments() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("active.jsonl");
        let fragment = dir.path().join("old.jsonl");
        std::fs::write(&target, "active").unwrap();
        std::fs::write(&fragment, "history").unwrap();
        let result = replace_with_rollback(
            &target,
            std::slice::from_ref(&fragment),
            true,
            ThreadId::new(),
            |file| {
                file.write_all(b"partial")?;
                Err(ImportError::Other("模拟失败".into()))
            },
        );
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "active");
        assert_eq!(std::fs::read_to_string(&fragment).unwrap(), "history");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn force_reimport_promotes_when_only_rotation_remains() {
        let dir = tempfile::tempdir().unwrap();
        let tid = ThreadId::new();
        let session = sample_session(tid, chrono::Utc::now());
        let records = imported_to_records(&session, &ConvertOptions::default());
        let active = write_session(dir.path(), &session, &records, false)
            .unwrap()
            .path;
        let rotated = active.with_file_name(format!("{tid}.1.jsonl"));
        std::fs::rename(&active, &rotated).unwrap();
        let report = write_session(dir.path(), &session, &records, true).unwrap();
        assert_eq!(report.path, active);
        assert!(active.exists());
        assert!(!rotated.exists());
    }

    #[test]
    fn force_reimport_handles_rotation_for_nonstandard_filename() {
        let dir = tempfile::tempdir().unwrap();
        let tid = ThreadId::new();
        let session = sample_session(tid, chrono::Utc::now());
        let records = imported_to_records(&session, &ConvertOptions::default());
        let original = write_session(dir.path(), &session, &records, false)
            .unwrap()
            .path;
        let active = original.with_file_name("external-name.jsonl");
        let rotation = original.with_file_name("external-name.1.jsonl");
        std::fs::rename(&original, &active).unwrap();
        std::fs::copy(&active, &rotation).unwrap();
        let report = write_session(dir.path(), &session, &records, true).unwrap();
        assert_eq!(report.path, active);
        assert!(!rotation.exists());
    }

    #[test]
    fn mismatched_metadata_does_not_replace_target() {
        let dir = tempfile::tempdir().unwrap();
        let session = sample_session(ThreadId::new(), chrono::Utc::now());
        let other = sample_session(ThreadId::new(), session.started_at);
        let wrong_records = imported_to_records(&other, &ConvertOptions::default());
        assert!(write_session(dir.path(), &session, &wrong_records, true).is_err());
        assert!(find_session_path(dir.path(), session.thread_id).is_none());
    }

    #[test]
    fn force_does_not_overwrite_unrelated_existing_target() {
        let dir = tempfile::tempdir().unwrap();
        let session = sample_session(ThreadId::new(), chrono::Utc::now());
        let target = session_path_at(dir.path(), session.thread_id, session.started_at);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "unrelated").unwrap();
        let records = imported_to_records(&session, &ConvertOptions::default());
        assert!(write_session(dir.path(), &session, &records, true).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "unrelated");
    }

    #[test]
    fn failed_staged_write_preserves_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, "original").unwrap();
        let result = write_staged(&path, true, ThreadId::new(), |file| {
            file.write_all(b"partial")?;
            Err(ImportError::Other("模拟写入失败".into()))
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn malformed_existing_file_on_other_date_is_not_duplicated() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = sample_session(
            ThreadId::new(),
            chrono::Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
        );
        let original = session_path_at(dir.path(), session.thread_id, session.started_at);
        std::fs::create_dir_all(original.parent().unwrap()).unwrap();
        std::fs::write(&original, "corrupt history").unwrap();
        session.started_at = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let records = imported_to_records(&session, &ConvertOptions::default());
        assert!(matches!(
            write_session(dir.path(), &session, &records, false),
            Err(ImportError::AlreadyExists(_))
        ));
        assert!(!session_path_at(dir.path(), session.thread_id, session.started_at).exists());
        assert_eq!(
            std::fs::read_to_string(original).unwrap(),
            "corrupt history"
        );
    }

    #[test]
    fn malformed_existing_destination_is_not_overwritten_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let session = sample_session(ThreadId::new(), chrono::Utc::now());
        let path = session_path_at(dir.path(), session.thread_id, session.started_at);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "original invalid content").unwrap();
        let records = imported_to_records(&session, &ConvertOptions::default());
        assert!(matches!(
            write_session(dir.path(), &session, &records, false),
            Err(ImportError::AlreadyExists(_))
        ));
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "original invalid content"
        );
    }

    /// 标题写入 `.name` 文件。
    #[test]
    fn writes_title_name_file() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("sessions");
        let tid = ThreadId::new();
        let s = sample_session(
            tid,
            chrono::Utc.with_ymd_and_hms(2025, 3, 14, 8, 0, 0).unwrap(),
        );
        let records = imported_to_records(&s, &ConvertOptions::default());
        write_session(&base, &s, &records, false).unwrap();
        let name = zhuye_rollout::index::read_session_name(&base, tid).unwrap();
        assert_eq!(name.as_deref(), Some("测试标题"));
    }

    /// 首行不是 SessionMeta 直接拒绝(防御调用方乱序)。
    #[test]
    fn rejects_records_without_meta_first() {
        let dir = tempfile::tempdir().unwrap();
        let tid = ThreadId::new();
        let s = sample_session(tid, chrono::Utc::now());
        let err = write_session(
            dir.path(),
            &s,
            &[RolloutRecord::Message {
                turn_id: zhuye_protocol::TurnId::new(),
                role: MessageRole::User,
                content: serde_json::json!("x"),
            }],
            false,
        )
        .unwrap_err();
        assert!(matches!(err, ImportError::Other(_)));
    }
}
