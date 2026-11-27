//! `memory_tool` — v1.x 记忆读写工具(收编 ZhuYe-Desktop 的 memory 写侧)。
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
use zhuye_memory::{EntryWriteOptions, FileMemoryStore, MemoryEditor, MemoryScope, MemoryStore};
use zhuye_protocol::{MemoryOp, PermissionMode, ToolError, ToolOutput};
use zhuye_tools::{Tool, ToolContext};

/// 记忆读写工具。list / read / write / remove 四个 action。
pub struct MemoryTool {
    /// topic 文件投影与旧路径条目编辑的文件 store(两种后端都保留:
    /// topic wiki 第一版继续走文件)。
    store: Arc<FileMemoryStore>,
    /// v1.11 M5a:条目编辑端口 —— 新模式路由 SQLite 变更端口,旧模式
    /// 即 `store` 本身。所有 write / remove / consolidate 经此唯一链。
    editor: Arc<dyn MemoryEditor>,
    agent_type: String,
    /// 审计记录定位:`(rollout base, thread id)`。构造时注入 ——
    /// `ToolContext.session_id` 在 exec / serve 未注入 `with_session_id`
    /// 时是**全零 uuid**(`submission_loop` 的 `cfg.session_id.unwrap_or_default()`),
    /// 拿它定位文件必然落空。
    session: Option<(std::path::PathBuf, zhuye_protocol::ThreadId)>,
}

impl MemoryTool {
    /// `store` 与 bootstrap 注入 pre_loop 的同一个文件 store(共享路径口径);
    /// `agent_type` 为构造时确定的活跃 agent 名。旧模式便捷构造:编辑器
    /// 即文件 store。
    pub fn new(store: Arc<FileMemoryStore>, agent_type: impl Into<String>) -> Self {
        Self {
            editor: store.clone(),
            store,
            agent_type: agent_type.into(),
            session: None,
        }
    }

    /// v1.11 M5a:显式注入编辑端口(sqlite 模式装配用)。`store` 仍用于
    /// topic 文件与 `read` 投影(sqlite 模式下 MEMORY.md 是活跃条目投影)。
    pub fn with_editor(
        store: Arc<FileMemoryStore>,
        editor: Arc<dyn MemoryEditor>,
        agent_type: impl Into<String>,
    ) -> Self {
        Self {
            store,
            editor,
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
                .filter(|value| !value.is_empty())
                .map(std::path::PathBuf::from)
                .or_else(|| zhuye_config::data_path(&["sessions"]))
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
        "读写 agent 持久记忆(`MEMORY.md` 的 `## <key>` 条目块 + 多文件 \
         topic 知识库)。操作:`list`(列条目)/ `read`(读正文)/ `write`\
         (新增或更新条目,支持 create_only 与 expected_value 乐观锁)/ \
         `remove`(删条目或整份)/ `topics`(列主题文件)/ `consolidate`\
         (整理去重)。带 `topic` 参数时 read/write/remove 作用于 \
         `topics/<topic>.md` 主题文件(长知识放这里,MEMORY.md 保持索引)。\
         写入对后续所有会话生效(project scope 可入 VCS)。"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "read", "write", "remove", "topics", "consolidate"],
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
                    "description": "Entry body; required for write. With `topic`: whole topic file content (empty = delete topic)."
                },
                "topic": {
                    "type": "string",
                    "description": "Topic file name (`topics/<topic>.md`, [A-Za-z0-9_-]); read/write/remove target the topic file instead of MEMORY.md."
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
        // list / read / topics 只读;write / remove / consolidate 有副作用。
        match args.get("action").and_then(|v| v.as_str()) {
            Some("list") | Some("read") | Some("topics") => PermissionMode::Auto,
            _ => PermissionMode::Prompt,
        }
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let action = Self::require_str(&args, "action")?;
        let scope = Self::parse_scope(&args)?;
        let agent = self.agent_type.as_str();
        // v1.11 基建#8:`topic` 参数把 read/write/remove 重定向到主题文件。
        let topic = args.get("topic").and_then(|v| v.as_str());
        if let Some(topic) = topic {
            zhuye_memory::validate_topic_name(topic).map_err(Self::map_err)?;
        }
        let text = match action {
            "list" => {
                // v1.11 M5a:条目列表经编辑端口(sqlite 模式 = 活跃条目)。
                let entries = self
                    .editor
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
                if let Some(topic) = topic {
                    // 主题文件正文。
                    let body = self
                        .store
                        .load_topic(scope, agent, topic)
                        .map_err(Self::map_err)?;
                    if body.trim().is_empty() {
                        format!("(topic `{topic}` not found or empty in {scope} scope)")
                    } else {
                        body
                    }
                } else {
                    let body = self.store.load(scope, agent).map_err(Self::map_err)?;
                    if body.trim().is_empty() {
                        format!("(no memory in {scope} scope)")
                    } else {
                        body
                    }
                }
            }
            "write" => {
                let value = Self::require_str(&args, "value")?;
                if let Some(topic) = topic {
                    // 主题文件整文件写;空正文 = 删除该 topic(幂等)。
                    self.store
                        .save_topic(scope, agent, topic, value)
                        .map_err(Self::map_err)?;
                    self.record_write(&ctx, &format!("topic:{topic}"), value.chars().count(), 1);
                    if value.trim().is_empty() {
                        format!("topic `{topic}` removed ({scope} scope)")
                    } else {
                        format!(
                            "topic `{topic}` written ({scope} scope, {} chars)",
                            value.chars().count()
                        )
                    }
                } else {
                    let key = Self::require_str(&args, "key")?;
                    let replace_document = args
                        .get("replace_document")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let opts = Self::write_opts(&args, replace_document)?;
                    self.editor
                        .write_entry(scope, agent, key, value, &opts)
                        .map_err(Self::map_err)?;
                    self.record_write(&ctx, key, value.chars().count(), 1);
                    format!(
                        "memory entry `{key}` written ({scope} scope, {} chars)",
                        value.chars().count()
                    )
                }
            }
            "remove" => {
                if let Some(topic) = topic {
                    self.store
                        .remove_topic(scope, agent, topic)
                        .map_err(Self::map_err)?;
                    self.record_write(&ctx, &format!("topic:{topic}"), 0, 0);
                    format!("topic `{topic}` removed ({scope} scope)")
                } else {
                    let key = Self::require_str(&args, "key")?;
                    let replace_document = args
                        .get("replace_document")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let opts = Self::write_opts(&args, replace_document)?;
                    self.editor
                        .delete_entry(scope, agent, key, &opts)
                        .map_err(Self::map_err)?;
                    self.record_write(&ctx, key, 0, 0);
                    format!("memory entry `{key}` removed ({scope} scope)")
                }
            }
            "topics" => {
                // 列主题文件(两 scope 合并列表)。
                let names = self
                    .store
                    .list_topics(scope, agent)
                    .map_err(Self::map_err)?;
                if names.is_empty() {
                    format!(
                        "(no topics in {scope} scope; write one with action=write + topic=<name>)"
                    )
                } else {
                    let mut out = format!("{} topic(s) in {scope} scope:\n", names.len());
                    for t in &names {
                        let len = self
                            .store
                            .load_topic(scope, agent, t)
                            .map_or(0, |b| b.chars().count());
                        out.push_str(&format!("- {t} ({len} chars)\n"));
                    }
                    out
                }
            }
            "consolidate" => {
                // 手动整理(去重 / 合并 / 清空条目)。sqlite 后端下写入
                // 路径已保证标题唯一,确定性整理无事可做 —— 如实报告
                // 零合并(语义合并属 M5b 决策端口)。
                let stats = self
                    .editor
                    .consolidate_entries(scope, agent)
                    .map_err(Self::map_err)?;
                match stats {
                    None => format!("(no MEMORY.md in {scope} scope; nothing to consolidate)"),
                    Some(s) => format!(
                        "consolidated {scope} scope: merged={} dropped_empty={} sections {} -> {}",
                        s.merged, s.dropped_empty, s.sections_before, s.sections_after
                    ),
                }
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
            metadata: {
                let mut meta = json!({
                    "action": action,
                    "scope": scope.to_string(),
                    "agent_type": agent,
                });
                // topic 操作在 metadata 里带目标文件(客户端可展示来源)。
                if let Some(t) = topic
                    && let Value::Object(map) = &mut meta
                {
                    map.insert("topic".into(), Value::String(t.to_string()));
                }
                meta
            },
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zhuye_tools::ToolContext;

    /// v1.11 基建#8:topic 子路径 write → topics → read → remove 全链路。
    #[tokio::test]
    async fn topic_actions_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = MemoryTool::new(
            Arc::new(zhuye_memory::FileMemoryStore::new(tmp.path(), tmp.path())),
            "agent",
        );
        let ctx = ToolContext::default();
        // write topic。
        let out = tool
            .execute(
                ctx.clone(),
                json!({"action": "write", "topic": "arch", "value": "# 架构\npostgres 16"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.metadata["topic"], "arch");
        // topics 列表可见。
        let list = tool
            .execute(ctx.clone(), json!({"action": "topics"}))
            .await
            .unwrap();
        let text = match &list.content[0] {
            zhuye_protocol::ContentBlock::Text { text } => text.clone(),
            other => panic!("topics 输出应是文本:{other:?}"),
        };
        assert!(text.contains("arch"), "topic 应在列表:{text}");
        // read topic 拿回正文。
        let read = tool
            .execute(ctx.clone(), json!({"action": "read", "topic": "arch"}))
            .await
            .unwrap();
        let body = match &read.content[0] {
            zhuye_protocol::ContentBlock::Text { text } => text.clone(),
            other => panic!("read 输出应是文本:{other:?}"),
        };
        assert!(body.contains("postgres 16"), "topic 正文:{body}");
        // remove topic 后列表为空。
        tool.execute(ctx.clone(), json!({"action": "remove", "topic": "arch"}))
            .await
            .unwrap();
        let list2 = tool
            .execute(ctx.clone(), json!({"action": "topics"}))
            .await
            .unwrap();
        let text2 = match &list2.content[0] {
            zhuye_protocol::ContentBlock::Text { text } => text.clone(),
            other => panic!("{other:?}"),
        };
        assert!(text2.contains("no topics"), "删除后应为空:{text2}");
    }

    /// topic 名非法 → InvalidArgs(路径穿越防护)。
    #[tokio::test]
    async fn topic_name_validated() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = MemoryTool::new(
            Arc::new(zhuye_memory::FileMemoryStore::new(tmp.path(), tmp.path())),
            "agent",
        );
        let err = tool
            .execute(
                ToolContext::default(),
                json!({"action": "read", "topic": "../x"}),
            )
            .await;
        assert!(err.is_err(), "穿越名必须拒绝");
    }

    /// consolidate action:有重复条目时合并并报告统计。
    #[tokio::test]
    async fn consolidate_action_reports_stats() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(zhuye_memory::FileMemoryStore::new(tmp.path(), tmp.path()));
        store
            .save(
                zhuye_memory::MemoryScope::Project,
                "agent",
                "## Deploy\nuses actions\n\n## deploy\nruns on main\n",
            )
            .unwrap();
        let tool = MemoryTool::new(store, "agent");
        let out = tool
            .execute(ToolContext::default(), json!({"action": "consolidate"}))
            .await
            .unwrap();
        let text = match &out.content[0] {
            zhuye_protocol::ContentBlock::Text { text } => text.clone(),
            other => panic!("{other:?}"),
        };
        assert!(text.contains("merged=1"), "应报告一次合并:{text}");
    }

    /// topics / consolidate 是只读(无副作用 action 权限口径)。
    #[test]
    fn topics_action_is_auto_permission() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = MemoryTool::new(
            Arc::new(zhuye_memory::FileMemoryStore::new(tmp.path(), tmp.path())),
            "agent",
        );
        assert_eq!(
            tool.action_permission(&json!({"action": "topics"})),
            PermissionMode::Auto
        );
        assert_eq!(
            tool.action_permission(&json!({"action": "consolidate"})),
            PermissionMode::Prompt
        );
    }

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
            parent_session_id: None,
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
