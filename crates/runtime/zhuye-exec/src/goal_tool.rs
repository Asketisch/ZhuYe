//! `goal_tool` — v1.10 功能2:把 goal(目标)模式暴露为 agent 工具。
//!
//! goal 模式的引擎路径是 `Op::EnterGoalMode` / `Op::ExitGoalMode`
//! (submission_loop 构造 `GoalController`,turn 结束后自校验 + 续作)。
//! TUI 走 Op 直达;headless / serve / SDK 侧的模型则经本工具以
//! `Submission` 提交同一组 Op —— 复用引擎全部语义(校验 LLM、3-strike、
//! token 预算),不另起炉灶。
//!
//! 权限路由:`status` → `Auto`(只读);`enter` / `exit` → `Prompt`
//! (改变回合编排)。
//!
//! **注册状态:插件门控** —— `goal-queue` 插件启用时由
//! `zhuye-exec::headless` 注册。与 queue 工具同插件交付。

use std::sync::Arc;

use async_trait::async_trait;
use zhuye_protocol::{PermissionMode, Submission, ToolError, ToolOutput};
use zhuye_tools::{Tool, ToolContext};

/// agent 可调用的 goal 模式工具。持有 submission sender(提交 Op)+
/// 与 `AgentConfig.goal` 共享的 controller 槽(读状态)。
///
/// 不持 `Arc<AgentThread>`:thread 拥有 ToolRegistry,registry 再持有
/// thread 会成引用环(CronTool 同款纪律 —— 只拿 sender + 共享 Arc)。
#[derive(Clone)]
pub struct GoalTool {
    sub_tx: tokio::sync::mpsc::Sender<Submission>,
    goal_slot: Arc<parking_lot::RwLock<Option<Arc<zhuye_goal::GoalController>>>>,
}

impl std::fmt::Debug for GoalTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoalTool")
            .field("has_sub_tx", &self.sub_tx.is_closed().not())
            .field("goal_active", &self.goal_slot.read().is_some())
            .finish()
    }
}

impl GoalTool {
    pub fn new(
        sub_tx: tokio::sync::mpsc::Sender<Submission>,
        goal_slot: Arc<parking_lot::RwLock<Option<Arc<zhuye_goal::GoalController>>>>,
    ) -> Self {
        Self { sub_tx, goal_slot }
    }
}

/// bool 取反辅助(Debug 派生用,保持字段可读)。
trait NotExt {
    fn not(self) -> bool;
}
impl NotExt for bool {
    fn not(self) -> bool {
        !self
    }
}

#[async_trait]
impl Tool for GoalTool {
    fn name(&self) -> &str {
        "goal"
    }

    fn description(&self) -> &str {
        "目标(goal)模式:设定一个目标,引擎在每轮结束后自校验(可选命令\
         校验),未达成时自动续作,直到达成 / 阻塞 / 预算耗尽。操作:\
         `enter` / `exit` / `status`。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["enter", "exit", "status"],
                    "description": "Which goal operation to perform."
                },
                "goal": {
                    "type": "string",
                    "description": "Goal statement (enter). Describe the verifiable end state."
                },
                "verify_command": {
                    "type": "string",
                    "description": "Optional shell command; exit 0 counts as an AND-condition for goal met (enter)."
                },
                "token_budget": {
                    "type": "integer",
                    "description": "Optional token budget for the goal session (enter)."
                }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Auto
    }

    fn action_permission(&self, args: &serde_json::Value) -> PermissionMode {
        match args.get("action").and_then(|v| v.as_str()) {
            Some("enter") | Some("exit") => PermissionMode::Prompt,
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
                    message: "goal: missing required field 'action'".into(),
                })?;
        match action {
            "enter" => self.exec_enter(&args),
            "exit" => self.exec_exit(),
            "status" => Ok(self.exec_status()),
            other => Err(ToolError::InvalidArgs {
                message: format!("goal: unknown action '{other}'"),
            }),
        }
    }
}

impl GoalTool {
    fn exec_enter(&self, args: &serde_json::Value) -> Result<ToolOutput, ToolError> {
        let goal = args
            .get("goal")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: "goal: missing required field 'goal'".into(),
            })?;
        let verify_command = args
            .get("verify_command")
            .and_then(|v| v.as_str())
            .filter(|c| !c.trim().is_empty())
            .map(str::to_string);
        let token_budget = args
            .get("token_budget")
            .and_then(|v| v.as_u64())
            .filter(|b| *b > 0);
        let op = zhuye_protocol::Op::EnterGoalMode {
            goal: goal.to_string(),
            verify_command,
            token_budget,
        };
        // try_send:goal Op 是引擎侧轻量分支,不值得为通道容量阻塞工具
        // 执行;通道满 / 关闭时如实报错。
        self.sub_tx
            .try_send(Submission::with_id(uuid::Uuid::new_v4().to_string(), op))
            .map_err(|e| {
                ToolError::Execution(format!("goal: 提交 Op 失败(agent loop 通道不可用):{e}"))
            })?;
        Ok(goal_output(
            "enter_submitted",
            serde_json::json!({ "goal": goal }),
            &format!(
                "目标模式已提交:「{goal}」。引擎在每轮结束后自校验;若没有可用的 \
                 LLM client 会以 GOAL_NO_CLIENT 错误事件反馈。用 goal status 查询进度。"
            ),
        ))
    }

    fn exec_exit(&self) -> Result<ToolOutput, ToolError> {
        self.sub_tx
            .try_send(Submission::with_id(
                uuid::Uuid::new_v4().to_string(),
                zhuye_protocol::Op::ExitGoalMode,
            ))
            .map_err(|e| {
                ToolError::Execution(format!("goal: 提交 Op 失败(agent loop 通道不可用):{e}"))
            })?;
        Ok(goal_output(
            "exit_submitted",
            serde_json::json!({}),
            "已提交退出目标模式。",
        ))
    }

    fn exec_status(&self) -> ToolOutput {
        let guard = self.goal_slot.read();
        let Some(c) = guard.as_ref() else {
            return goal_output(
                "status",
                serde_json::json!({ "active": false }),
                "目标模式未激活。",
            );
        };
        let snap = c.snapshot();
        match snap {
            Some(s) => goal_output(
                "status",
                serde_json::json!({
                    "active": true,
                    "goal": s.goal,
                    "status": format!("{:?}", s.status),
                    "tokens_used": s.tokens_used,
                    "turn_count": s.turn_count,
                    "token_budget": s.token_budget,
                    "verify_command": s.verify_command,
                }),
                &format!(
                    "目标模式:{{{:?}}}\n  goal: {}\n  turn {} / tokens {}\n  budget: {}",
                    s.status,
                    s.goal,
                    s.turn_count,
                    s.tokens_used,
                    s.token_budget
                        .map(|b| b.to_string())
                        .unwrap_or_else(|| "默认 500k".into())
                ),
            ),
            None => goal_output(
                "status",
                serde_json::json!({ "active": true }),
                "目标模式激活中(首轮校验尚未发生)。",
            ),
        }
    }
}

/// 构造 `ToolOutput`:一个人类可读文本块 + 结构化 metadata。
fn goal_output(action: &str, extra: serde_json::Value, text: &str) -> ToolOutput {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolContext {
        ToolContext {
            workspace: Arc::new(parking_lot::RwLock::new(std::env::temp_dir())),
            ..ToolContext::default()
        }
    }

    fn tool_with_channel() -> (GoalTool, tokio::sync::mpsc::Receiver<Submission>) {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let slot = Arc::new(parking_lot::RwLock::new(None));
        (GoalTool::new(tx, slot), rx)
    }

    #[tokio::test]
    async fn enter_submits_op_exit_submits_op() {
        let (t, mut rx) = tool_with_channel();
        let out = t
            .execute(
                ctx(),
                serde_json::json!({"action": "enter", "goal": "修好所有测试", "token_budget": 12345}),
            )
            .await
            .unwrap();
        assert_eq!(out.metadata["action"], "enter_submitted");
        let sub = rx.recv().await.unwrap();
        match sub.op {
            zhuye_protocol::Op::EnterGoalMode {
                goal,
                verify_command,
                token_budget,
            } => {
                assert_eq!(goal, "修好所有测试");
                assert_eq!(verify_command, None);
                assert_eq!(token_budget, Some(12345));
            }
            other => panic!("expected EnterGoalMode, got {other:?}"),
        }
        let out2 = t
            .execute(ctx(), serde_json::json!({"action": "exit"}))
            .await
            .unwrap();
        assert_eq!(out2.metadata["action"], "exit_submitted");
        let sub2 = rx.recv().await.unwrap();
        assert!(matches!(sub2.op, zhuye_protocol::Op::ExitGoalMode));
    }

    #[tokio::test]
    async fn status_inactive_without_controller() {
        let (t, _rx) = tool_with_channel();
        let out = t
            .execute(ctx(), serde_json::json!({"action": "status"}))
            .await
            .unwrap();
        assert_eq!(out.metadata["active"], false);
    }

    #[tokio::test]
    async fn enter_requires_nonempty_goal() {
        let (t, _rx) = tool_with_channel();
        assert!(
            t.execute(ctx(), serde_json::json!({"action": "enter", "goal": " "}))
                .await
                .is_err()
        );
        assert!(
            t.execute(ctx(), serde_json::json!({"action": "enter"}))
                .await
                .is_err()
        );
    }

    #[test]
    fn permissions_split_read_vs_write() {
        let (t, _rx) = tool_with_channel();
        assert_eq!(
            t.action_permission(&serde_json::json!({"action": "enter"})),
            PermissionMode::Prompt
        );
        assert_eq!(
            t.action_permission(&serde_json::json!({"action": "exit"})),
            PermissionMode::Prompt
        );
        assert_eq!(
            t.action_permission(&serde_json::json!({"action": "status"})),
            PermissionMode::Auto
        );
    }
}
