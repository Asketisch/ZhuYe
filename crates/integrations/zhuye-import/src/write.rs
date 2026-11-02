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
use zhuye_rollout::index::{find_session_path, rename_session};
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
/// - `force == true` 时 truncate 覆盖原文件;
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
    if !matches!(records[0], RolloutRecord::SessionMeta { .. }) {
        return Err(ImportError::Other(
            "records 首行必须是 SessionMeta(索引契约)".into(),
        ));
    }

    // 幂等保护:同 id 会话已存在则拒绝(保留原 uuid 的导入天然幂等)。
    if !force && find_session_path(base, session.thread_id).is_some() {
        return Err(ImportError::AlreadyExists(session.thread_id.to_string()));
    }

    let path = session_path_at(base, session.thread_id, session.started_at);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)?;

    for r in records {
        let line = serialize_redacted(r).map_err(|e| ImportError::Json {
            path: path.display().to_string(),
            detail: format!("记录序列化失败: {e}"),
        })?;
        writeln!(file, "{line}")?;
    }
    file.flush()?;

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
