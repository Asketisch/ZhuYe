//! `memory_tool` — v1.x 记忆读写工具(收编 ReflectDesktop 的 memory 写侧)。
//!
//! 此前框架只有记忆的**读**路径(pre_loop 注入 + `retrieve_relevant`),
//! 写入能力长在桌面端 GUI 里 —— CLI / SDK 完全没有入口,模型也无法在
//! 会话中固化发现。本工具补上:
//!
//! - `list` —— 列某 scope 的全部记忆条目(只读,Auto);
//! - `read` —— 读某 scope 的记忆正文(只读,Auto);
//! - `write` —— 新增 / 更新条目(Prompt;支持 `create_only` 与
//!   `expected_value` 乐观锁);
//! - `remove` —— 删除条目或整份文档(Prompt)。
//!
//! ## 路径与 agent 归属
//!
//! 写侧与读侧共用同一个 [`zhuye_memory::FileMemoryStore`] 与
//! `resolve_path`(agent_type 在构造时注入,不引入进程级全局 —— 桌面端
//! 曾因 `AgentState` 全局与运行时读的 agent 名不一致,出现「GUI 写的
//! 记忆 agent 读不到」)。每次写 / 删后 best-effort 追加一条
//! `RolloutRecord::MemoryAccess { op: Write }`,让 `session_stats` 的
//! `memory_writes` 与导出稿的审计行闭合(镜像 `CheckpointTool` 的
//! best-effort 记录模式)。

use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use zhuye_memory::{EntryWriteOptions, FileMemoryStore, MemoryScope, MemoryStore};
use zhuye_protocol::{MemoryOp, PermissionMode, ToolError, ToolOutput};
use zhuye_tools::{Tool, ToolContext};

/// 记忆读写工具。list / read / write / remove 四个 action。
pub struct MemoryTool {
    store: Arc<FileMemoryStore>,
    agent_type: String,
    /// 审计记录定位:`(rollout base, thread id)`。构造时注入 ——
    /// `ToolContext.session_id` 在 exec / serve 未注入 `with_session_id`
    /// 时是**全零 uuid**(`submission_loop` 的 `cfg.session_id.unwrap_or_default()`),
    /// 拿它定位文件必然落空。
    session: Option<(std::path::PathBuf, zhuye_protocol::ThreadId)>,
}

impl MemoryTool {
    /// `store` 与 bootstrap 注入 pre_loop 的同一个文件 store(共享路径口径);
    /// `agent_type` 为构造时确定的活跃 agent 名。
    pub fn new(store: Arc<FileMemoryStore>, agent_type: impl Into<String>) -> Self {
        Self {
            store,
            agent_type: agent_type.into(),
            session: None,
        }
    }

    /// 注入会话定位(rollout base + thread id),让 MemoryAccess::Write
    /// 审计记录能 append 到正确的 session JSONL。
    pub fn with_session(
        mut self,
        base: std::path::PathBuf,
        thread_id: zhuye_protocol::ThreadId,
    ) -> Self {
        self.session = Some((base, thread_id));
        self
    }

    /// 定位 session JSONL 文件:内部 session id 优先,回落文件名 thread id。
    fn session_file(&self, ctx: &ToolContext) -> Option<std::path::PathBuf> {
        let (base, tid) = self.session.clone().unwrap_or_else(|| {
            let base = std::env::var_os("ZHUYE_ROLLOUT_BASE")
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(|h| std::path::PathBuf::from(h).join(".zhuye").join("sessions"))
                })
                .unwrap_or_default();
            (base, ctx.session_id)
        });
        if base.as_os_str().is_empty() {
            return None;
        }
        zhuye_rollout::index::find_session_path(&base, tid).or_else(|| {
            zhuye_rollout::index::find_files_by_name_prefix(&base, &tid.to_string())
                .ok()
                .and_then(|mut v| {
                    if v.is_empty() {
                        None
                    } else {
                        Some(v.remove(0))
                    }
                })
        })
    }

    fn parse_scope(args: &Value) -> Result<MemoryScope, ToolError> {
        match args.get("scope").and_then(|v| v.as_str()) {
            Some("project") => Ok(MemoryScope::Project),
            Some("user") => Ok(MemoryScope::User),
            Some(other) => Err(ToolError::InvalidArgs {
                message: format!("memory: unknown scope '{other}' (expected project|user)"),
            }),
            None => Ok(MemoryScope::Project),
        }
    }

    fn require_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, ToolError> {
        args.get(field)
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: format!("memory: missing required field '{field}'"),
            })
    }

    fn write_opts(args: &Value, replace_document: bool) -> Result<EntryWriteOptions, ToolError> {
        let expected_value = match args.get("expected_value") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Null) | None => None,
            Some(_) => {
                return Err(ToolError::InvalidArgs {
                    message: "memory: 'expected_value' must be a string".into(),
                });
            }
        };
        Ok(EntryWriteOptions {
            replace_document,
            create_only: args
                .get("create_only")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            expected_value,
        })
    }

    fn map_err(e: zhuye_memory::MemoryError) -> ToolError {
        ToolError::Execution(e.to_string())
    }

    /// best-effort 追加 MemoryAccess::Write 记录。
    ///
    /// 让 `session_stats` 的 `memory_writes` 与导出稿的审计行闭合
    /// (镜像 `CheckpointTool` 的 best-effort 记录模式,但会话定位不依赖
    /// 无人注入的 `ZHUYE_*` env,见 [`Self::with_session`])。
    fn record_write(&self, ctx: &ToolContext, key: &str, chars: usize, entries: usize) {
        let Some(path) = self.session_file(ctx) else {
            return;
        };
        let record = zhuye_protocol::RolloutRecord::MemoryAccess {
            turn_id: ctx.turn_id,
            op: MemoryOp::Write,
            scope: String::new(),
            agent_type: self.agent_type.clone(),
            chars: chars as u32,
            entries: entries as u32,
            at: chrono::Utc::now(),
        };
        let Ok(line) = serde_json::to_string(&record) else {
            return;
        };
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&path) {
            if let Err(e) = writeln!(f, "{line}") {
                tracing::warn!(error = %e, key, "memory: failed to append MemoryAccess record");
            }
        }
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "读写 agent 持久记忆(`MEMORY.md` 的 `## <key>` 条目块)。\
         操作:`list`(列条目)/ `read`(读正文)/ `write`(新增或更新条目,\
         支持 create_only 与 expected_value 乐观锁)/ `remove`(删条目或整份)。\
         写入对后续所有会话生效(project scope 可入 VCS)。"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "read", "write", "remove"],
                    "description": "Which memory operation to perform."
                },
                "scope": {
                    "type": "string",
                    "enum": ["project", "user"],
                    "description": "Memory scope; defaults to project."
                },
                "key": {
                    "type": "string",
                    "description": "Entry key (`## <key>` heading); required for write/remove. `(all)` = whole document."
                },
                "value": {
                    "type": "string",
                    "description": "Entry body; required for write."
                },
                "create_only": {
                    "type": "boolean",
                    "description": "write: fail if the entry already exists."
                },
                "expected_value": {
                    "type": "string",
                    "description": "Optimistic lock: current body must equal this value."
                },
                "replace_document": {
                    "type": "boolean",
                    "description": "write/remove with key `(all)`: replace/delete the whole MEMORY.md."
                }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        false // 写路径改文件
    }

    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Prompt
    }

    fn action_permission(&self, args: &Value) -> PermissionMode {
        // list / read 只读;write / remove 有副作用。
        match args.get("action").and_then(|v| v.as_str()) {
            Some("list") | Some("read") => PermissionMode::Auto,
            _ => PermissionMode::Prompt,
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let action = Self::require_str(&args, "action")?;
        let scope = Self::parse_scope(&args)?;
        let agent = self.agent_type.as_str();
        let text = match action {
            "list" => {
                let entries = self
                    .store
                    .list_entries(scope, agent)
                    .map_err(Self::map_err)?;
                if entries.is_empty() {
                    format!("(no memory entries in {scope} scope)")
                } else {
                    let mut out = format!("{} entrie(s) in {scope} scope:\n", entries.len());
                    for e in &entries {
                        let preview: String = e.value.trim().chars().take(60).collect();
                        out.push_str(&format!(
                            "- {}{}: {}\n",
                            e.key,
                            if e.whole_document {
                                " (whole document)"
                            } else {
                                ""
                            },
                            preview
                        ));
                    }
                    out
                }
            }
            "read" => {
                let body = self.store.load(scope, agent).map_err(Self::map_err)?;
                if body.trim().is_empty() {
                    format!("(no memory in {scope} scope)")
                } else {
                    body
                }
            }
            "write" => {
                let key = Self::require_str(&args, "key")?;
                let value = Self::require_str(&args, "value")?;
                let replace_document = args
                    .get("replace_document")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let opts = Self::write_opts(&args, replace_document)?;
                self.store
                    .add_entry(scope, agent, key, value, &opts)
                    .map_err(Self::map_err)?;
                self.record_write(&ctx, key, value.chars().count(), 1);
                format!(
                    "memory entry `{key}` written ({scope} scope, {} chars)",
                    value.chars().count()
                )
            }
            "remove" => {
                let key = Self::require_str(&args, "key")?;
                let replace_document = args
                    .get("replace_document")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let opts = Self::write_opts(&args, replace_document)?;
                self.store
                    .remove_entry(scope, agent, key, &opts)
                    .map_err(Self::map_err)?;
                self.record_write(&ctx, key, 0, 0);
                format!("memory entry `{key}` removed ({scope} scope)")
            }
            other => {
                return Err(ToolError::InvalidArgs {
                    message: format!("memory: unknown action '{other}'"),
                });
            }
        };
        Ok(ToolOutput {
            content: vec![zhuye_protocol::ContentBlock::Text { text }],
            is_error: false,
            metadata: json!({
                "action": action,
                "scope": scope.to_string(),
                "agent_type": agent,
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zhuye_tools::ToolContext;

    /// record_write 落盘定位:双 ID 解析(文件名 thread id 优先命中)后,
    /// MemoryAccess::Write 记录必须 append 到该 session 的 JSONL。
    #[test]
    fn record_write_appends_to_session_file_by_thread_id() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("sessions");
        let day = base.join("2026/11/02");
        std::fs::create_dir_all(&day).unwrap();
        let sid = zhuye_protocol::ThreadId::new();
        // 文件名 = thread id;首行 SessionMeta 的 session_id 用**另一个**
        // uuid(生产 serve/exec 双 ID 语义)。
        let inner = zhuye_protocol::ThreadId::new();
        let meta = serde_json::to_string(&zhuye_protocol::RolloutRecord::SessionMeta {
            session_id: inner,
            model: "mock".into(),
            started_at: chrono::Utc::now(),
            workspace: None,
        })
        .unwrap();
        let path = day.join(format!("{sid}.jsonl"));
        std::fs::write(&path, format!("{meta}\n")).unwrap();

        let ctx = ToolContext {
            session_id: sid,
            ..Default::default()
        };
        let store = Arc::new(zhuye_memory::FileMemoryStore::new(tmp.path(), tmp.path()));
        let tool = MemoryTool::new(store, "agent").with_session(base, sid);
        tool.record_write(&ctx, "k", 3, 1);

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("memory_access"), "记录未落盘:{body}");
        assert!(body.contains("\"op\":\"write\""), "缺 write op:{body}");
        assert!(
            body.contains("\"agent_type\":\"agent\""),
            "缺 agent_type:{body}"
        );
    }
}
