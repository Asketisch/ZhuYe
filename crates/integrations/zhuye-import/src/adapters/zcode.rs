//! ZCode 会话 adapter(存于 SQLite:`~/.zcode/cli/db/db.sqlite`)。
//!
//! ## Schema 快照(2026-11 对本机 ZCode v2 逆向;解析按需取列,缺列容错)
//!
//! ```sql
//! CREATE TABLE session (
//!   id TEXT PRIMARY KEY,            -- "sess_<uuid>";子代理为 "sess_subagent_agent_<uuid>"
//!   directory TEXT NOT NULL,        -- 工作区绝对路径
//!   title TEXT NOT NULL,
//!   time_created INTEGER NOT NULL,  -- unix 毫秒
//!   time_updated INTEGER NOT NULL, …);
//! CREATE TABLE message (
//!   id TEXT PRIMARY KEY,            -- "msg_<ts>_<uuid>"
//!   session_id TEXT NOT NULL,
//!   time_created INTEGER NOT NULL,
//!   data TEXT NOT NULL,             -- JSON:{role, time:{created}, modelId?, providerId?,
//!                                   --        metadata?:{visibility?: "model-only", source?}}
//!   sequence INTEGER);
//! CREATE TABLE part (
//!   id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT,
//!   time_created INTEGER NOT NULL, data TEXT NOT NULL, sequence INTEGER);
//!   -- data.type ∈ text{text} | reasoning{text} | tool{callID, tool,
//!   --   state:{status:"completed"|"error"|…, input{}, output}} | step-start |
//!   --   step-finish | timeline
//! CREATE TABLE turn_usage (
//!   session_id, turn_id, user_message_id,  -- 挂回开启该轮的 user 消息
//!   input_tokens, output_tokens, cache_read_input_tokens, …);
//! ```
//!
//! 映射规则:
//! - `visibility == "model-only"` 的 user 消息(运行时元消息)默认过滤;
//! - user 文本部件以 `# Environment` / `<system-reminder>` 等开头的注入块
//!   默认过滤(`keep_injected` 保留);
//! - assistant 的 text / reasoning / tool 部件 → Text / Thinking /
//!   ToolUse(+ 完成态时追加 ToolResult,`is_error` 取 `status=="error"`);
//! - `turn_usage` 按 `user_message_id` 挂轮用量;
//! - `directory` → workspace,`title` → 会话标题,首个 assistant
//!   `modelId` → 模型。
//!
//! ## 读取方式
//!
//! 以只读连接打开源库,通过 SQLite 在线备份生成一致快照,再以
//! `SQLITE_OPEN_READ_ONLY` 打开临时副本;备份包含已提交的 WAL 内容。

use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use zhuye_protocol::ThreadId;

use crate::adapter::HarnessAdapter;
use crate::error::ImportError;
use crate::model::{
    Entry, HarnessKind, ImportOptions, ImportSource, ImportWarning, ImportedSession, ImportedTurn,
    ImportedUsage, IrBlock, UserBlock,
};

/// ZCode user 消息里的注入块前缀(环境脚手架)。
const INJECTED_PART_PREFIXES: &[&str] = &["# Environment", "<system-reminder>", "<environment>"];

pub struct ZcodeAdapter;

impl HarnessAdapter for ZcodeAdapter {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Zcode
    }

    /// ZCode 是 SQLite 存储,不参与 JSONL 嗅探(必须显式 `--from zcode`)。
    fn probe(&self, _path: &Path) -> bool {
        false
    }

    fn parse(
        &self,
        source: &ImportSource,
        opts: &ImportOptions,
    ) -> Result<ImportedSession, ImportError> {
        let (db, key) = match source {
            ImportSource::SqliteSession { db, key } => (db.clone(), key.clone()),
            ImportSource::JsonlFile(_) => {
                return Err(ImportError::Other(
                    "zcode 来源是 SQLite 会话标识,JSONL 文件属于 claude-code / codex".into(),
                ));
            }
        };
        let _guard = TempDbCopy::new(&db)?;
        let conn = _guard.open()?;

        // ── session 行 ──
        let (directory, title, time_created): (String, Option<String>, i64) = conn
            .query_row(
                "SELECT directory, title, time_created FROM session WHERE id = ?1",
                [&key],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    ImportError::Other(format!("ZCode 库中不存在会话 {key}"))
                }
                other => ImportError::Sqlite(other),
            })?;

        let mut warnings: Vec<ImportWarning> = Vec::new();

        // ── thread_id:sess_ / sess_subagent_agent_ 前缀剥除后取 uuid ──
        let bare = key
            .trim_start_matches("sess_subagent_agent_")
            .trim_start_matches("sess_");
        let thread_id = match uuid::Uuid::parse_str(bare) {
            Ok(u) => ThreadId(u),
            Err(_) => {
                warnings.push(ImportWarning::new(
                    "invalid-uuid",
                    format!("ZCode 会话 id `{key}` 不含合法 UUID,已分配新的 thread id"),
                ));
                ThreadId::new()
            }
        };

        // ── 消息按序读取 ──
        let mut stmt = conn.prepare(
            "SELECT id, data, time_created FROM message WHERE session_id = ?1 \
             ORDER BY COALESCE(sequence, time_created), id",
        )?;
        let rows: Vec<(String, String, i64)> = stmt
            .query_map([&key], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<_, _>>()?;

        let mut entries: Vec<Entry> = Vec::new();
        let mut current: Option<ImportedTurn> = None;
        let mut model: Option<String> = None;
        let usage_by_user_msg: Vec<(String, ImportedUsage)> = turn_usage_map(&conn, &key);

        for (msg_id, data, time_created) in rows {
            let Ok(msg) = serde_json::from_str::<Value>(&data) else {
                warnings.push(ImportWarning::new(
                    "malformed-message",
                    format!("message {msg_id} 的 data 不是合法 JSON,已跳过"),
                ));
                continue;
            };
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
            let ts = ms_to_utc(time_created);

            // 运行时元消息(plan_mode_exit 等)默认剔除。
            let model_only = msg
                .get("metadata")
                .and_then(|m| m.get("visibility"))
                .and_then(Value::as_str)
                == Some("model-only");
            if model_only && !opts.keep_injected {
                continue;
            }

            // 该消息的部件按序读取。
            let mut parts = message_parts(&conn, &msg_id)?;

            match role {
                "user" => {
                    let mut user_blocks: Vec<UserBlock> = Vec::new();
                    for p in parts.drain(..) {
                        if p.get("type").and_then(Value::as_str) != Some("text") {
                            continue;
                        }
                        let text = p
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        if text.trim().is_empty() {
                            continue;
                        }
                        if is_injected_part(&text) && !opts.keep_injected {
                            continue;
                        }
                        user_blocks.push(UserBlock::Text { text });
                    }
                    if user_blocks.is_empty() {
                        continue;
                    }
                    // 真实用户输入 → 开新 turn;挂回该轮用量。
                    if let Some(t) = current.take() {
                        if !t.is_empty() {
                            entries.push(Entry::Turn(t));
                        }
                    }
                    let mut turn = ImportedTurn::empty();
                    turn.started_at = ts;
                    turn.user = user_blocks;
                    if let Some((_, u)) = usage_by_user_msg.iter().find(|(id, _)| *id == msg_id) {
                        turn.usage = Some(*u);
                    }
                    current = Some(turn);
                }
                "assistant" => {
                    if model.is_none() {
                        model = msg
                            .get("modelId")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    let turn = current.get_or_insert_with(|| {
                        let mut t = ImportedTurn::empty();
                        t.started_at = ts;
                        t
                    });
                    for p in parts {
                        match p.get("type").and_then(Value::as_str).unwrap_or("") {
                            "text" => {
                                let text = p
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string();
                                if !text.trim().is_empty() {
                                    turn.assistant.push(IrBlock::Text { text });
                                }
                            }
                            "reasoning" => {
                                let text = p
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string();
                                if !text.is_empty() {
                                    turn.assistant.push(IrBlock::Thinking { text });
                                }
                            }
                            "tool" => {
                                let call_id = p
                                    .get("callID")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string();
                                let name = p
                                    .get("tool")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string();
                                let state = p.get("state").cloned().unwrap_or(Value::Null);
                                let args = state.get("input").cloned().unwrap_or(Value::Null);
                                turn.assistant.push(IrBlock::ToolUse {
                                    id: call_id.clone(),
                                    name,
                                    args,
                                });
                                // 完成态才有输出;error 状态的输出同样保留
                                // (is_error = true),与原生中断语义一致。
                                let status = state
                                    .get("status")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string();
                                if let Some(out) = state.get("output") {
                                    let output = match out {
                                        Value::String(s) => s.clone(),
                                        other if !other.is_null() => {
                                            serde_json::to_string(other).unwrap_or_default()
                                        }
                                        _ => String::new(),
                                    };
                                    turn.assistant.push(IrBlock::ToolResult {
                                        call_id,
                                        output,
                                        is_error: status == "error",
                                    });
                                }
                            }
                            // step-start / step-finish / timeline:忽略。
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(t) = current.take() {
            if !t.is_empty() {
                entries.push(Entry::Turn(t));
            }
        }

        if entries.is_empty() {
            return Err(ImportError::EmptySession(format!("{}#{key}", db.display())));
        }

        let started_at = ms_to_utc(time_created).unwrap_or_else(Utc::now);
        Ok(ImportedSession {
            kind: HarnessKind::Zcode,
            source_id: key,
            thread_id,
            model: model.unwrap_or_default(),
            started_at,
            workspace: Some(directory),
            title: title.filter(|t| !t.trim().is_empty()),
            entries,
            warnings,
        })
    }

    fn discover(&self) -> Result<Vec<ImportSource>, ImportError> {
        // 默认库:~/.zcode/cli/db/db.sqlite;库不存在 = 未安装 ZCode。
        let Some(home) = std::env::var_os("HOME") else {
            return Ok(Vec::new());
        };
        let db = PathBuf::from(home)
            .join(".zcode")
            .join("cli")
            .join("db")
            .join("db.sqlite");
        if !db.exists() {
            return Ok(Vec::new());
        }
        let _guard = TempDbCopy::new(&db)?;
        let conn = _guard.open()?;
        let mut stmt = conn.prepare(
            "SELECT id FROM session WHERE time_deleted IS NULL ORDER BY time_created DESC",
        )?;
        let ids: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        Ok(ids
            .into_iter()
            .map(|key| ImportSource::SqliteSession {
                db: db.clone(),
                key,
            })
            .collect())
    }
}

/// 读取某会话全部 turn_usage,按 user_message_id 挂轮。
fn turn_usage_map(conn: &Connection, session_id: &str) -> Vec<(String, ImportedUsage)> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT user_message_id, input_tokens, output_tokens, cache_read_input_tokens \
         FROM turn_usage WHERE session_id = ?1",
    ) else {
        return Vec::new();
    };
    let mapped = stmt.query_map([session_id], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?.unwrap_or_default(),
            ImportedUsage {
                input_tokens: r.get::<_, i64>(1)?.max(0) as u64,
                output_tokens: r.get::<_, i64>(2)?.max(0) as u64,
                cached_tokens: r.get::<_, i64>(3)?.max(0) as u64,
            },
        ))
    });
    match mapped {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

/// 读取一条消息的全部部件(按序)。
fn message_parts(conn: &Connection, message_id: &str) -> Result<Vec<Value>, ImportError> {
    let mut stmt = conn.prepare(
        "SELECT data FROM part WHERE message_id = ?1 \
         ORDER BY COALESCE(sequence, time_created), id",
    )?;
    let rows = stmt.query_map([message_id], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        let s = row?;
        match serde_json::from_str::<Value>(&s) {
            Ok(v) => out.push(v),
            Err(_) => {
                tracing::warn!(message_id, "part.data 不是合法 JSON,已跳过");
            }
        }
    }
    Ok(out)
}

fn is_injected_part(text: &str) -> bool {
    INJECTED_PART_PREFIXES.iter().any(|p| text.starts_with(p))
}

/// unix 毫秒 → UTC;非法值返回 None。
fn ms_to_utc(ms: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms).single()
}

/// 通过 SQLite 在线备份生成只读快照,临时目录随守卫清理。
struct TempDbCopy {
    _dir: tempfile::TempDir,
    db_copy: PathBuf,
}

impl TempDbCopy {
    fn new(src: &Path) -> Result<Self, ImportError> {
        if !src.exists() {
            return Err(ImportError::Other(format!(
                "ZCode 数据库不存在: {}",
                src.display()
            )));
        }
        let dir = tempfile::Builder::new().prefix("zhuye-import-").tempdir()?;
        let db_copy = dir.path().join("db.sqlite");
        let source = Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        source.busy_timeout(std::time::Duration::from_millis(100))?;
        let mut destination = Connection::open(&db_copy)?;
        {
            use rusqlite::backup::{Backup, StepResult};
            let backup = Backup::new(&source, &mut destination)?;
            let mut busy_since = None;
            loop {
                match backup.step(128)? {
                    StepResult::Done => break,
                    StepResult::More => busy_since = None,
                    StepResult::Busy | StepResult::Locked => {
                        let since = busy_since.get_or_insert_with(std::time::Instant::now);
                        if since.elapsed() >= std::time::Duration::from_secs(5) {
                            return Err(ImportError::Other(
                                "ZCode 数据库繁忙,无法生成一致快照".into(),
                            ));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    _ => return Err(ImportError::Other("SQLite 备份返回未知状态".into())),
                }
            }
        }
        drop(destination);
        Ok(Self { _dir: dir, db_copy })
    }

    fn open(&self) -> Result<Connection, ImportError> {
        Ok(Connection::open_with_flags(
            &self.db_copy,
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在临时目录构造与真实 schema 同构的最小 ZCode 库。
    fn make_fixture_db(dir: &Path) -> PathBuf {
        let db = dir.join("db.sqlite");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE session (
              id text primary key, directory text not null, title text,
              time_created integer not null, time_updated integer not null,
              time_deleted integer);
            CREATE TABLE message (
              id text primary key, session_id text not null,
              time_created integer not null, data text not null, sequence integer);
            CREATE TABLE part (
              id text primary key, message_id text not null, session_id text,
              time_created integer not null, data text not null, sequence integer);
            CREATE TABLE turn_usage (
              session_id text not null, turn_id text not null,
              user_message_id text,
              input_tokens integer not null default 0,
              output_tokens integer not null default 0,
              cache_read_input_tokens integer not null default 0,
              primary key(session_id, turn_id));
            "#,
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES ('sess_9a1b0000-0000-4000-8000-000000000001', '/work/zc', '会话标题', 1790000000000, 1790000600000, NULL)",
            [],
        )
        .unwrap();
        // m1:真实用户输入;m2:model-only 元消息(过滤);m3:assistant。
        conn.execute(
            "INSERT INTO message VALUES ('msg_1','sess_9a1b0000-0000-4000-8000-000000000001',1790000000100,'{\"role\":\"user\",\"time\":{\"created\":1790000000100}}',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('msg_2','sess_9a1b0000-0000-4000-8000-000000000001',1790000000200,'{\"role\":\"user\",\"time\":{\"created\":1790000000200},\"metadata\":{\"visibility\":\"model-only\",\"source\":\"plan_mode_exit\"}}',1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('msg_3','sess_9a1b0000-0000-4000-8000-000000000001',1790000000300,'{\"role\":\"assistant\",\"time\":{\"created\":1790000000300},\"modelId\":\"GLM-5.3-Flash\",\"providerId\":\"account:bigmodel\"}',2)",
            [],
        )
        .unwrap();
        // user 部件:真实输入 + 注入 Environment 块。
        conn.execute(
            "INSERT INTO part VALUES ('p1','msg_1','s',1790000000101,'{\"type\":\"text\",\"text\":\"# Environment\\n环境注入\"}',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('p2','msg_1','s',1790000000102,'{\"type\":\"text\",\"text\":\"帮我查这个函数\"}',1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('p3','msg_2','s',1790000000201,'{\"type\":\"text\",\"text\":\"元消息文本\"}',0)",
            [],
        )
        .unwrap();
        // assistant 部件:reasoning + text + tool(含输出)。
        conn.execute(
            "INSERT INTO part VALUES ('p4','msg_3','s',1790000000301,'{\"type\":\"reasoning\",\"text\":\"先搜函数\"}',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('p5','msg_3','s',1790000000302,'{\"type\":\"text\",\"text\":\"我用 grep 查。\"}',1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('p6','msg_3','s',1790000000303,'{\"type\":\"tool\",\"callID\":\"call_z1\",\"tool\":\"Grep\",\"state\":{\"status\":\"completed\",\"input\":{\"pattern\":\"foo\"},\"output\":\"src/a.rs:1\"}}',2)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO turn_usage VALUES ('sess_9a1b0000-0000-4000-8000-000000000001','turn_1','msg_1',500,60,300)",
            [],
        )
        .unwrap();
        db
    }

    /// 主干:前缀剥 uuid、model-only 过滤、注入块过滤、tool 配对、
    /// 用量挂轮、directory/title/model 提取。
    #[test]
    fn parses_zcode_sqlite_session() {
        let dir = tempfile::tempdir().unwrap();
        let db = make_fixture_db(dir.path());
        let source = ImportSource::SqliteSession {
            db,
            key: "sess_9a1b0000-0000-4000-8000-000000000001".into(),
        };
        let s = ZcodeAdapter
            .parse(&source, &ImportOptions::default())
            .unwrap();

        assert_eq!(
            s.thread_id,
            ThreadId::parse_str("9a1b0000-0000-4000-8000-000000000001").unwrap()
        );
        assert_eq!(s.workspace.as_deref(), Some("/work/zc"));
        assert_eq!(s.title.as_deref(), Some("会话标题"));
        assert_eq!(s.model, "GLM-5.3-Flash");
        assert_eq!(s.turn_count(), 1, "model-only 元消息不开轮");

        let Entry::Turn(t) = &s.entries[0] else {
            panic!();
        };
        assert_eq!(t.user.len(), 1, "注入 Environment 块被过滤");
        let UserBlock::Text { text } = &t.user[0] else {
            panic!();
        };
        assert_eq!(text, "帮我查这个函数");
        let kinds: Vec<&str> = t
            .assistant
            .iter()
            .map(|b| match b {
                IrBlock::Text { .. } => "text",
                IrBlock::Thinking { .. } => "thinking",
                IrBlock::ToolUse { .. } => "tool_use",
                IrBlock::ToolResult { .. } => "tool_result",
            })
            .collect();
        assert_eq!(kinds, ["thinking", "text", "tool_use", "tool_result"]);
        match &t.assistant[2] {
            IrBlock::ToolUse { name, args, .. } => {
                assert_eq!(name, "Grep");
                assert_eq!(args["pattern"], "foo");
            }
            other => panic!("{other:?}"),
        }
        match &t.assistant[3] {
            IrBlock::ToolResult {
                output, is_error, ..
            } => {
                assert_eq!(output, "src/a.rs:1");
                assert!(!is_error);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(t.usage.map(|u| u.input_tokens), Some(500));
        assert_eq!(t.usage.map(|u| u.cached_tokens), Some(300));
    }

    /// 子代理前缀同样剥出 uuid;不存在的 key 报错。
    #[test]
    fn subagent_prefix_and_missing_key() {
        let dir = tempfile::tempdir().unwrap();
        let db = make_fixture_db(dir.path());
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO session VALUES ('sess_subagent_agent_9a1b0000-0000-4000-8000-000000000002', '/w', '子', 1790000010000, 1790000010000, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('msg_s1','sess_subagent_agent_9a1b0000-0000-4000-8000-000000000002',1790000010100,'{\"role\":\"user\",\"time\":{\"created\":1790000010100}}',0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('ps1','msg_s1','s',1790000010101,'{\"type\":\"text\",\"text\":\"子任务输入\"}',0)",
            [],
        )
        .unwrap();
        let s = ZcodeAdapter
            .parse(
                &ImportSource::SqliteSession {
                    db,
                    key: "sess_subagent_agent_9a1b0000-0000-4000-8000-000000000002".into(),
                },
                &ImportOptions::default(),
            )
            .unwrap_or_else(|e| panic!("解析不应因 id 前缀失败: {e}"));
        assert_eq!(
            s.thread_id,
            ThreadId::parse_str("9a1b0000-0000-4000-8000-000000000002").unwrap()
        );
        assert_eq!(s.turn_count(), 1);

        let missing = ZcodeAdapter.parse(
            &ImportSource::SqliteSession {
                db: dir.path().join("db.sqlite"),
                key: "sess_nope".into(),
            },
            &ImportOptions::default(),
        );
        assert!(matches!(missing, Err(ImportError::Other(_))));
    }

    #[test]
    fn online_snapshot_reads_committed_wal_without_uncommitted_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.sqlite");
        let source = Connection::open(&path).unwrap();
        source.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE sample(value TEXT); INSERT INTO sample VALUES ('committed'); BEGIN IMMEDIATE; INSERT INTO sample VALUES ('uncommitted');").unwrap();
        assert!(PathBuf::from(format!("{}-wal", path.display())).exists());
        let snapshot = TempDbCopy::new(&path).unwrap();
        let copy = snapshot.open().unwrap();
        let mut statement = copy
            .prepare("SELECT value FROM sample ORDER BY value")
            .unwrap();
        let values: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(values, vec!["committed"]);
        assert!(
            copy.execute("INSERT INTO sample VALUES ('write')", [])
                .is_err()
        );
        source.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            source
                .query_row("SELECT count(*) FROM sample", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    /// discover:列出全部会话(按时间倒序)。
    #[test]
    fn discover_lists_sessions() {
        // discover 依赖 $HOME/.zcode/cli/db/db.sqlite,进程级 env 并发
        // 不安全,这里直接测 TempDbCopy + SQL 路径的等价逻辑:
        let dir = tempfile::tempdir().unwrap();
        let db = make_fixture_db(dir.path());
        let guard = TempDbCopy::new(&db).unwrap();
        let conn = guard.open().unwrap();
        let mut stmt = conn
            .prepare("SELECT id FROM session WHERE time_deleted IS NULL ORDER BY time_created DESC")
            .unwrap();
        let ids: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(ids.len(), 1);
    }

    /// 库文件不存在 → 带路径的错误。
    #[test]
    fn missing_db_reports_path() {
        let err = match TempDbCopy::new(Path::new("/nonexistent/db.sqlite")) {
            Err(e) => e,
            Ok(_) => panic!("不存在的库不应构造成功"),
        };
        assert!(matches!(err, ImportError::Other(ref m) if m.contains("/nonexistent")));
    }
}
