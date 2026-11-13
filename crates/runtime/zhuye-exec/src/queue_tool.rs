//! `queue_tool` — v1.10 功能2:把队列模式暴露为 agent 工具。
//!
//! 单工具多 action(镜像 `cron_tool` 的 discriminated-action 模式):
//! `add` / `list` / `remove` / `clear` / `start` / `pause` / `status`。
//! 底层 `zhuye_stream::QueueScheduler` 持有 session 级 FIFO 任务列表,
//! 后台 driver 在前一条任务的 turn 结束后串行注入下一条
//! (`Submission::user_input`)。
//!
//! 权限路由:`list` / `status` → `Auto`(只读);其余(改变队列状态)
//! → `Prompt`。
//!
//! **注册状态:插件门控** —— `goal-queue` 插件启用时由
//! `zhuye-exec::headless` 注册(`zhuye plugin enable goal-queue` 后生效),
//! 与 `CronTool`(无条件注册)的差别在于队列模式按需求以插件形态交付。

use std::sync::Arc;

use async_trait::async_trait;
use zhuye_protocol::{PermissionMode, ToolError, ToolOutput};
use zhuye_stream::QueueScheduler;
use zhuye_tools::{Tool, ToolContext};

/// agent 可调用的队列管理工具。持有一个共享的 `QueueScheduler`。
#[derive(Clone)]
pub struct QueueTool {
    scheduler: Arc<QueueScheduler>,
}

impl std::fmt::Debug for QueueTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueTool")
            .field("scheduler", &self.scheduler)
            .finish()
    }
}

impl QueueTool {
    pub fn new(scheduler: Arc<QueueScheduler>) -> Self {
        Self { scheduler }
    }
}

#[async_trait]
impl Tool for QueueTool {
    fn name(&self) -> &str {
        "queue"
    }

    fn description(&self) -> &str {
        "队列模式:把 prompt 任务排成 FIFO 队列串行执行(前一条 turn 结束后\
         自动注入下一条)。操作:`add` / `list` / `remove` / `clear` / \
         `start` / `pause` / `status`。任务跨 turn 持久、session 范围内生效。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "list", "remove", "clear", "start", "pause", "status"],
                    "description": "Which queue operation to perform."
                },
                "prompt": {
                    "type": "string",
                    "description": "Task prompt to enqueue (add). Dequeued and run as a new turn."
                },
                "name": {
                    "type": "string",
                    "description": "Optional human-readable task name (add)."
                },
                "id": {
                    "type": "string",
                    "description": "Task id (remove)."
                }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // 改队列状态(有副作用),保守串行。
        false
    }

    fn required_permission(&self) -> PermissionMode {
        // Tool-level fallback:`list`/`status` 走 Auto;改状态 action 在
        // `action_permission` 切到 Prompt。
        PermissionMode::Auto
    }

    fn action_permission(&self, args: &serde_json::Value) -> PermissionMode {
        match args.get("action").and_then(|v| v.as_str()) {
            Some("add") | Some("remove") | Some("clear") | Some("start") | Some("pause") => {
                PermissionMode::Prompt
            }
            _ => PermissionMode::Auto,
        }
    }

    async fn execute(
        &self,
        _ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let action =
            args.get("action")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: "queue: missing required field 'action'".into(),
                })?;

        match action {
            "add" => self.exec_add(&args),
            "list" => self.exec_list(),
            "remove" => self.exec_remove(&args),
            "clear" => self.exec_clear(),
            "start" => {
                // set_running(true) 内部已唤醒 driver 立即尝试出队,
                // 不等 2s 周期 tick。
                self.scheduler.set_running(true);
                Ok(queue_output(
                    "started",
                    serde_json::json!({ "running": true }),
                    "队列模式已开启:driver 会在前一条任务 turn 结束后自动执行下一条。",
                ))
            }
            "pause" => {
                self.scheduler.set_running(false);
                Ok(queue_output(
                    "paused",
                    serde_json::json!({ "running": false }),
                    "队列已暂停:任务保留,当前执行中的任务照常完成,但不再出队。",
                ))
            }
            "status" => {
                let s = self.scheduler.status();
                Ok(queue_output(
                    "status",
                    serde_json::json!({
                        "running": s.running,
                        "counts": s.counts,
                        "active": s.active,
                        "next": s.next,
                    }),
                    &format!(
                        "队列状态:running={} queued={} running_task={} done={} failed={}",
                        s.running,
                        s.counts.queued,
                        s.counts.running,
                        s.counts.done,
                        s.counts.failed
                    ),
                ))
            }
            other => Err(ToolError::InvalidArgs {
                message: format!("queue: unknown action '{other}'"),
            }),
        }
    }
}

impl QueueTool {
    fn exec_add(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
        let prompt = require_str(args, "prompt")?;
        if prompt.trim().is_empty() {
            return Err(ToolError::InvalidArgs {
                message: "queue: prompt 不能为空".into(),
            });
        }
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let task = self.scheduler.enqueue(prompt, name);
        let s = self.scheduler.status();
        Ok(queue_output(
            "added",
            serde_json::json!({
                "id": task.id,
                "position": s.counts.queued,
                "queue_running": s.running,
            }),
            &format!(
                "任务已入队:{}{}\n  排队中:{} 条;队列模式:{}(未开启时用 start 开启)",
                task.id,
                task.name
                    .as_deref()
                    .map_or_else(|| Default::default(), |n| format!(" ({n})")),
                s.counts.queued,
                if s.running { "开启" } else { "暂停" }
            ),
        ))
    }

    fn exec_list(&self) -> Result<ToolOutput, ToolError> {
        let tasks = self.scheduler.list();
        let lines: Vec<String> = tasks
            .iter()
            .map(|t| {
                format!(
                    "- {} [{}] {}: {}",
                    t.id,
                    status_str(t.status),
                    t.name.as_deref().unwrap_or("<unnamed>"),
                    truncate(&t.prompt, 80),
                )
            })
            .collect();
        Ok(queue_output(
            "list",
            serde_json::json!({
                "count": tasks.len(),
                "tasks": tasks.iter().map(task_summary).collect::<Vec<_>>(),
            }),
            &if tasks.is_empty() {
                "队列为空。".to_string()
            } else {
                format!("队列任务:\n{}", lines.join("\n"))
            },
        ))
    }

    fn exec_remove(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
        let id = require_str(args, "id")?;
        match self.scheduler.remove(id) {
            Ok(true) => Ok(queue_output(
                "removed",
                serde_json::json!({ "id": id }),
                &format!("任务已删除:{id}"),
            )),
            Ok(false) => Ok(queue_output(
                "not_found",
                serde_json::json!({ "id": id }),
                &format!("任务不存在:{id}"),
            )),
            Err(e) => Err(ToolError::Execution(format!("queue: {e}"))),
        }
    }

    fn exec_clear(&self) -> Result<ToolOutput, ToolError> {
        let removed = self.scheduler.clear();
        Ok(queue_output(
            "cleared",
            serde_json::json!({ "removed": removed }),
            &format!("已清空 {removed} 条未在执行的任务(执行中的任务保留)。"),
        ))
    }
}

// ── 辅助函数 ─────────────────────────────────────────────────────────

fn require_str<'a>(args: &'a serde_json::Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            message: format!("queue: missing required field '{key}'"),
        })
}

fn status_str(s: zhuye_stream::QueueTaskStatus) -> &'static str {
    match s {
        zhuye_stream::QueueTaskStatus::Queued => "queued",
        zhuye_stream::QueueTaskStatus::Running => "running",
        zhuye_stream::QueueTaskStatus::Done => "done",
        zhuye_stream::QueueTaskStatus::Failed => "failed",
    }
}

/// 把一条任务渲染成精简 summary(给 list / metadata 用)。
fn task_summary(t: &zhuye_stream::QueueTask) -> serde_json::Value {
    serde_json::json!({
        "id": t.id,
        "name": t.name,
        "prompt": t.prompt,
        "status": status_str(t.status),
        "created_at": t.created_at,
        "started_at": t.started_at,
        "finished_at": t.finished_at,
        "note": t.note,
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// 构造 `ToolOutput`:一个人类可读文本块 + 结构化 metadata。
fn queue_output(action: &str, extra: serde_json::Value, text: &str) -> ToolOutput {
    let mut metadata = serde_json::json!({ "action": action });
    if let serde_json::Value::Object(map) = extra {
        if let serde_json::Value::Object(meta) = &mut metadata {
            for (k, v) in map {
                meta.insert(k, v);
            }
        }
    }
    ToolOutput {
        content: vec![zhuye_protocol::ContentBlock::Text {
            text: text.to_string(),
        }],
        is_error: false,
        metadata,
        elapsed_ms: 0,
    }
}

// v1.10 功能2初版曾用 Stop hook(`QueueTurnEndHook`)作唯一结清信号,
// 但引擎存在绕过 check_stop 的结束路径(凭证耗尽 / 重试上限 / 预算
// 耗尽 / `Op::Interrupt` / 流取消),Stop hook 在这些路径不触发,active
// 任务会永久卡 `Running`、整条队列死锁到重启;且 Stop hook 无法归因
// turn,手动输入先结束会误结清 active 任务。现改为 headless 装配时经
// `QueueScheduler::with_turn_submitter` 接线 per-turn 事件监视(见
// `headless::wire_goal_queue_plugin`),按任务 id 结清。

#[cfg(test)]
mod tests {
    use super::*;
    use zhuye_protocol::ThreadId;
    use zhuye_stream::QueueScheduler;

    fn tool() -> QueueTool {
        QueueTool::new(Arc::new(QueueScheduler::new(None, ThreadId::new())))
    }

    fn ctx() -> ToolContext {
        ToolContext {
            workspace: Arc::new(parking_lot::RwLock::new(std::env::temp_dir())),
            ..ToolContext::default()
        }
    }

    #[tokio::test]
    async fn add_then_list_roundtrip() {
        let t = tool();
        let out = t
            .execute(
                ctx(),
                serde_json::json!({"action": "add", "prompt": "跑一遍测试", "name": "t1"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.metadata["action"], "added");
        let id = out.metadata["id"].as_str().unwrap().to_string();

        let list = t
            .execute(ctx(), serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        assert_eq!(list.metadata["count"], 1);
        assert_eq!(list.metadata["tasks"][0]["id"], id.as_str());
        assert_eq!(list.metadata["tasks"][0]["status"], "queued");
        assert_eq!(list.metadata["tasks"][0]["prompt"], "跑一遍测试");
        assert_eq!(list.metadata["tasks"][0]["name"], "t1");
    }

    #[tokio::test]
    async fn status_reflects_running_toggle() {
        let t = tool();
        let s0 = t
            .execute(ctx(), serde_json::json!({"action": "status"}))
            .await
            .unwrap();
        assert_eq!(s0.metadata["running"], false);
        let start = t
            .execute(ctx(), serde_json::json!({"action": "start"}))
            .await
            .unwrap();
        assert_eq!(start.metadata["action"], "started");
        let s1 = t
            .execute(ctx(), serde_json::json!({"action": "status"}))
            .await
            .unwrap();
        assert_eq!(s1.metadata["running"], true);
        t.execute(ctx(), serde_json::json!({"action": "pause"}))
            .await
            .unwrap();
        let s2 = t
            .execute(ctx(), serde_json::json!({"action": "status"}))
            .await
            .unwrap();
        assert_eq!(s2.metadata["running"], false);
    }

    #[tokio::test]
    async fn remove_and_clear_paths() {
        let t = tool();
        t.execute(ctx(), serde_json::json!({"action": "add", "prompt": "a"}))
            .await
            .unwrap();
        let out = t
            .execute(ctx(), serde_json::json!({"action": "add", "prompt": "b"}))
            .await
            .unwrap();
        let id_b = out.metadata["id"].as_str().unwrap().to_string();
        let rm = t
            .execute(ctx(), serde_json::json!({"action": "remove", "id": id_b}))
            .await
            .unwrap();
        assert_eq!(rm.metadata["action"], "removed");
        let nf = t
            .execute(ctx(), serde_json::json!({"action": "remove", "id": "zzz"}))
            .await
            .unwrap();
        assert_eq!(nf.metadata["action"], "not_found");
        let cl = t
            .execute(ctx(), serde_json::json!({"action": "clear"}))
            .await
            .unwrap();
        assert_eq!(cl.metadata["removed"], 1);
    }

    #[tokio::test]
    async fn empty_prompt_rejected_and_unknown_action() {
        let t = tool();
        let e = t
            .execute(ctx(), serde_json::json!({"action": "add", "prompt": "  "}))
            .await;
        assert!(e.is_err());
        let e2 = t
            .execute(ctx(), serde_json::json!({"action": "explode"}))
            .await;
        assert!(e2.is_err());
    }

    #[test]
    fn mutating_actions_require_prompt_permission() {
        let t = tool();
        assert_eq!(
            t.action_permission(&serde_json::json!({"action": "add"})),
            PermissionMode::Prompt
        );
        assert_eq!(
            t.action_permission(&serde_json::json!({"action": "start"})),
            PermissionMode::Prompt
        );
        assert_eq!(
            t.action_permission(&serde_json::json!({"action": "list"})),
            PermissionMode::Auto
        );
        assert_eq!(
            t.action_permission(&serde_json::json!({"action": "status"})),
            PermissionMode::Auto
        );
    }
}
