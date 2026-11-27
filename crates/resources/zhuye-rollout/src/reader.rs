//! JSONL reader / replay 辅助函数。
//!
//! 同时被 [`crate::writer::JsonlRolloutWriter::replay`](转发到
//! [`replay_path`])以及恢复 session 时的 `zhuye-exec::bootstrap_resume` 使用。

use std::path::{Path, PathBuf};

use tokio::io::{AsyncBufReadExt, BufReader};

use zhuye_protocol::{RolloutError, RolloutRecord, ThreadId};

/// 逐行读取 `path` 里的所有 record。
///
/// 行分类(v0.2.1 增强):
/// - 正常 record → push 到输出
/// - 未知 variant(forward-compat,旧 consumer 读 v0.3+ 写入)→ `debug!` + skip
/// - 真正 JSON 损坏(截断 / 非 JSON)→ `warn!` + skip
///
/// 未知 variant 走 debug 是为了避免 v0.2 → v0.3 升级后日志刷屏;且**不打印
/// line 内容**(可能含 prompt/工具结果),防敏感信息泄露到 stderr。
pub async fn replay_path(path: &Path) -> Result<Vec<RolloutRecord>, RolloutError> {
    let mut out = Vec::new();
    if !path.exists() {
        return Ok(out);
    }
    let file = tokio::fs::File::open(path).await?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        // 修复原因:强杀可能把多字节字符截断。逐行读取字节,单条
        // 非法 UTF-8 只影响自身,不能让此前完整的记录一起丢失。
        if reader.read_until(b'\n', &mut line).await? == 0 {
            break;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<RolloutRecord>(&line) {
            Ok(r) => out.push(r),
            Err(e) => {
                // serde_json 对未知 enum variant 的错误信息固定包含此字面量
                // (跨 platform 稳定);用于区分 forward-compat 与真损坏。
                if e.to_string().contains("unknown variant") {
                    tracing::debug!(
                        error = %e,
                        "rollout: skip unknown variant (forward-compat)"
                    );
                } else {
                    tracing::warn!(
                        error = %e,
                        "rollout: skip malformed line"
                    );
                }
            }
        }
    }
    Ok(out)
}

/// 按时间顺序读取会话的全部保留记录,包括任意日期目录和轮转副本。
pub async fn replay(base: &Path, session_id: ThreadId) -> Result<Vec<RolloutRecord>, RolloutError> {
    let mut combined = Vec::new();
    let paths = session_files(base, session_id)?;
    for path in &paths {
        combined.extend(replay_path(&path).await?);
    }
    // 工具执行账本不随对话文件轮转,否则长时间执行可能丢失开始记录。
    // 最后追加使账本成为相同调用阶段的权威顺序;旧会话无此文件时为空。
    if let Some(path) = paths.last()
        && let Some(stem) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.split('.').next())
    {
        combined.extend(replay_path(&path.with_file_name(format!("{stem}.tool-journal"))).await?);
    }
    Ok(combined)
}

/// 同一日期内先读较旧的高编号副本,最后读活跃文件;跨日期按目录升序。
/// 同时支持文件名 thread id 和首行内部 session id 两种入口。
pub(crate) fn session_files(base: &Path, session_id: ThreadId) -> std::io::Result<Vec<PathBuf>> {
    let mut stem = session_id.to_string();
    let mut paths = crate::index::find_files_by_name_prefix(base, &stem)?;
    if paths.is_empty()
        && let Some(path) = crate::index::find_file_by_session_id(base, &session_id)?
        && let Some(name) = path.file_stem().and_then(|s| s.to_str())
    {
        stem = name.split('.').next().unwrap_or(name).to_owned();
        paths = crate::index::find_files_by_name_prefix(base, &stem)?;
    }
    let mut ranked = Vec::new();
    for path in paths {
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let rank = if name == stem {
            0
        } else if let Some(n) = name
            .strip_prefix(&format!("{stem}."))
            .and_then(|n| n.parse::<usize>().ok())
        {
            n
        } else {
            continue;
        };
        ranked.push((
            path.parent().map(Path::to_path_buf),
            std::cmp::Reverse(rank),
            path,
        ));
    }
    ranked.sort();
    Ok(ranked.into_iter().map(|(_, _, path)| path).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use zhuye_protocol::{MessageRole, TurnId};

    #[tokio::test]
    async fn roundtrips_records() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut lines = Vec::new();
        let sid = ThreadId::new();
        let tid = TurnId::new();
        let records = vec![
            RolloutRecord::session_meta(sid, "anthropic/claude"),
            RolloutRecord::message(tid, MessageRole::User, serde_json::json!("hi")),
            RolloutRecord::message(tid, MessageRole::Assistant, serde_json::json!("hello")),
            RolloutRecord::Compaction {
                turn_id: tid,
                strategy: "microcompact".into(),
                removed_count: 5,
                summary: "<summary>...</summary>".into(),
                before_tokens: 0,
                after_tokens: 0,
                context_snapshot: None,
            },
            RolloutRecord::Fork {
                parent_session_id: sid,
                branch_name: "explorer".into(),
                child_session_id: None,
            },
        ];
        for r in &records {
            lines.push(serde_json::to_string(r).unwrap());
        }
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();

        let back = replay_path(&path).await.unwrap();
        assert_eq!(back.len(), records.len());
        for (a, b) in records.iter().zip(back.iter()) {
            assert_eq!(a, b);
        }
    }

    #[tokio::test]
    async fn skips_malformed_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let sid = ThreadId::new();
        let good = serde_json::to_string(&RolloutRecord::session_meta(sid, "m")).unwrap();
        let body = format!("{good}\nnot json\n\n{good}\n");
        std::fs::write(&path, body).unwrap();

        let back = replay_path(&path).await.unwrap();
        assert_eq!(back.len(), 2, "should skip 1 malformed + 1 empty");
    }

    #[tokio::test]
    async fn 跳过强杀留下的半个多字节字符并保留完整记录() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.tool-journal");
        let started = RolloutRecord::ToolExecution {
            session_id: ThreadId::new(),
            turn_id: TurnId::new(),
            call_id: "call-1".into(),
            tool_name: "写入".into(),
            phase: zhuye_protocol::ToolExecutionPhase::Started,
            is_error: None,
            at: chrono::Utc::now(),
        };
        let mut bytes = serde_json::to_vec(&started).unwrap();
        bytes.extend_from_slice(b"\n{\"tool_name\":\"\xe5");
        std::fs::write(&path, bytes).unwrap();

        assert_eq!(replay_path(&path).await.unwrap(), vec![started]);
    }

    #[tokio::test]
    async fn missing_file_returns_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.jsonl");
        let back = replay_path(&path).await.unwrap();
        assert!(back.is_empty());
    }

    /// 未知 variant 应被静默 skip(v0.2.1 行为:走 debug log,不带 line 内容)。
    /// 模拟 v0.3+ 加新 variant,旧 consumer 读 → 不 panic,不报 warn。
    #[tokio::test]
    async fn skips_unknown_variant_silently() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let sid = ThreadId::new();
        let good = serde_json::to_string(&RolloutRecord::session_meta(sid, "m")).unwrap();
        // 模拟 v0.3 加的新 variant;serde_json 解析时报 "unknown variant"。
        let unknown = r#"{"type":"discussion_message","payload":{}}"#;
        let body = format!("{good}\n{unknown}\n{good}\n");
        std::fs::write(&path, body).unwrap();

        let back = replay_path(&path).await.unwrap();
        assert_eq!(back.len(), 2, "should skip 1 unknown variant + 2 known");
    }
}
