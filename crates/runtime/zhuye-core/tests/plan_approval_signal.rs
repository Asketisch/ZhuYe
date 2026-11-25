//! v1.4 Plan 审批信号回传集成测试。
//!
//! 回归背景:v1.4 之前 `tool_exec` 在 `ExitPlanMode` 审批完成后**丢弃**用户
//! choice(`let _ = dispatch_plan_ready_blocking(...)`),模型收到的 Tool
//! 消息始终是工具原始输出 —— 真实 LLM 在工具返回后倾向直接结束 turn
//! ("Waiting for your approval to proceed."),已批准的 plan 永远不会被执行
//! (运行时真实 LLM 测试暴露)。修复后,决策被重写进工具输出第一个 text
//! block(模型可见的 Tool 消息 + latest_content):
//! - 批准(AutoMode / ManualApprove)→ "Plan approved. ... Execute the plan now..."
//! - Revise(3/Esc)→ "The user requested changes ... Stay in Plan mode..."
//!
//! 本测试用 stub LLM 驱动完整链路:首响 `ExitPlanMode` tool_call →
//! tool_exec 派发 `PlanReady` 并阻塞等审批 → 测试截获事件后投递
//! `Op::PlanApproval` → 断言后续模型请求历史里的 Tool 消息被重写。

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{Stream, stream};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use zhuye_core::{AgentConfig, AgentThread};
use zhuye_llm::{
    Capabilities, ChatEvent, ChatMessage, ChatRequest, CredentialPool, LlmError, ModelClient,
    ModelRegistry, PoolEntry,
};
use zhuye_protocol::{EventMsg, Op, PermissionMode, PlanApprovalChoice, Submission, UserInputItem};
use zhuye_tools::{Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};

// ── LLM stub(记录每次 request,便于断言请求内容)────────────────

struct RecordingStub {
    /// 每次 `stream` 从队首弹出一批事件;空队列时返回纯文本收尾批。
    batches: Mutex<Vec<Vec<ChatEvent>>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl RecordingStub {
    fn new(batches: Vec<Vec<ChatEvent>>) -> Self {
        Self {
            batches: Mutex::new(batches),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().clone()
    }
}

#[async_trait]
impl ModelClient for RecordingStub {
    fn name(&self) -> &str {
        "stub"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_use: true,
            prompt_caching: true,
            ..Default::default()
        }
    }
    async fn stream(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        self.requests.lock().push(request);
        let mut b = self.batches.lock();
        let events = if b.is_empty() {
            vec![
                ChatEvent::MessageStart {
                    id: "done".into(),
                    model: "stub-1".into(),
                },
                ChatEvent::MessageStop,
            ]
        } else {
            b.remove(0)
        };
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

/// `ExitPlanMode` 的无副作用替身:返回成功输出。tool_exec 的 plan 审批
/// 派发只认工具名 + `is_error`,不依赖真实工具实现。
struct ExitPlanModeStub;

#[async_trait]
impl Tool for ExitPlanModeStub {
    fn name(&self) -> &str {
        "ExitPlanMode"
    }
    fn description(&self) -> &str {
        "stub ExitPlanMode"
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    async fn execute(&self, _ctx: ToolContext, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: vec![zhuye_protocol::ContentBlock::text("Plan ready (12 chars)")],
            is_error: false,
            metadata: Value::Null,
            elapsed_ms: 0,
        })
    }
}

// ── 辅助 ────────────────────────────────────────────────────────────

fn user_sub(id: &str, text: &str) -> Submission {
    Submission {
        id: id.into(),
        op: Op::UserInput {
            items: vec![UserInputItem::Text { text: text.into() }],
            thread_settings: Default::default(),
        },
        workspace: None,
        client_user_message_id: None,
        trace: None,
        source_command: None,
    }
}

fn plan_approval_sub(submission_id: &str, plan_id: &str, choice: PlanApprovalChoice) -> Submission {
    Submission {
        id: submission_id.into(),
        op: Op::PlanApproval {
            id: plan_id.into(),
            choice,
        },
        workspace: None,
        client_user_message_id: None,
        trace: None,
        source_command: None,
    }
}

/// 提取 ChatRequest 历史里所有 Tool(工具结果)消息的文本。
fn tool_result_text(req: &ChatRequest) -> String {
    let mut out = String::new();
    for m in &req.messages {
        if let ChatMessage::Tool(tr) = m {
            for b in &tr.content {
                if let zhuye_llm::ContentBlock::Text { text } = b {
                    out.push_str(text);
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// 构造「首响 ExitPlanMode tool_call + 尾随文本」的 stub 批次。
fn exit_plan_mode_batch(msg_id: &str, tool_call_id: &str) -> Vec<ChatEvent> {
    vec![
        ChatEvent::MessageStart {
            id: msg_id.into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("## Plan\n- step 1".into()),
        ChatEvent::ToolUseStart {
            id: tool_call_id.into(),
            name: "ExitPlanMode".into(),
            input_json: String::new(),
        },
        ChatEvent::ToolUseDelta("{}".into()),
        ChatEvent::MessageStop,
    ]
}

// ── 测试 ────────────────────────────────────────────────────────────

/// 批准(ManualApprove)→ 模型可见的 Tool 消息必须含
/// "Plan approved ... Execute the plan now" 重写。
#[tokio::test]
async fn plan_approval_approved_rewrites_model_tool_output() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(RecordingStub::new(vec![
        exit_plan_mode_batch("m1", "tc1"),
        vec![
            ChatEvent::MessageStart {
                id: "m2".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ContentDelta("executing".into()),
            ChatEvent::MessageStop,
        ],
    ]));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_approvals(true);
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(ExitPlanModeStub));
    let thread = Arc::new(AgentThread::new(cfg, registry, tools, None, None));

    let mut handle = thread.submit(user_sub("sub-plan", "make a plan")).await;
    while let Some(ev) = timeout(Duration::from_secs(5), handle.next())
        .await
        .expect("event arrives within 5s")
    {
        match ev.msg {
            EventMsg::PlanReady(ready) => {
                let t = thread.clone();
                let pid = ready.plan_id.to_string();
                tokio::spawn(async move {
                    t.submit(plan_approval_sub(
                        "sub-approve",
                        &pid,
                        PlanApprovalChoice::ManualApprove,
                    ))
                    .await;
                });
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    let reqs = stub.requests();
    assert!(
        reqs.len() >= 2,
        "审批后模型应继续下一轮(共 >=2 次调用);got {}",
        reqs.len()
    );
    let text = tool_result_text(&reqs[1]);
    assert!(
        text.contains("Plan approved"),
        "批准后模型可见的 Tool 消息应含 'Plan approved' 重写;got: {text}"
    );
    assert!(
        text.contains("Execute the plan now"),
        "批准信号应指示模型立即执行 plan;got: {text}"
    );
}

/// Revise(3/Esc)→ 模型可见的 Tool 消息必须含 "The user requested changes"
/// 重写,且模型继续 turn(重新规划);最终批准时 Tool 消息再被批准文案重写。
#[tokio::test]
async fn plan_approval_revise_rewrites_then_approval_continues() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(RecordingStub::new(vec![
        // 第 1 轮:ExitPlanMode → 被 Revise。
        exit_plan_mode_batch("m1", "tc1"),
        // 第 2 轮:模型重新规划,再次 ExitPlanMode。
        exit_plan_mode_batch("m2", "tc2"),
        // 第 3 轮:正常文本收尾。
        vec![
            ChatEvent::MessageStart {
                id: "m3".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ContentDelta("done".into()),
            ChatEvent::MessageStop,
        ],
    ]));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_approvals(true);
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(ExitPlanModeStub));
    let thread = Arc::new(AgentThread::new(cfg, registry, tools, None, None));

    let mut handle = thread.submit(user_sub("sub-plan", "make a plan")).await;
    let mut plan_ready_count = 0u32;
    while let Some(ev) = timeout(Duration::from_secs(5), handle.next())
        .await
        .expect("event arrives within 5s")
    {
        match ev.msg {
            EventMsg::PlanReady(ready) => {
                plan_ready_count += 1;
                // 第一次 PlanReady → Revise;第二次 → 批准。
                let choice = if plan_ready_count == 1 {
                    PlanApprovalChoice::Revise
                } else {
                    PlanApprovalChoice::ManualApprove
                };
                let t = thread.clone();
                let pid = ready.plan_id.to_string();
                tokio::spawn(async move {
                    t.submit(plan_approval_sub("sub-approve", &pid, choice))
                        .await;
                });
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    let reqs = stub.requests();
    assert!(
        reqs.len() >= 3,
        "Revise 后模型应重新规划并再次提交 plan(共 >=3 次调用);got {}",
        reqs.len()
    );
    // 第 2 次请求(Revise 之后):Tool 消息应含 Revise 重写文案。
    let revise_text = tool_result_text(&reqs[1]);
    assert!(
        revise_text.contains("The user requested changes"),
        "Revise 后模型可见的 Tool 消息应含修改提示;got: {revise_text}"
    );
    // 第 3 次请求(第二次批准之后):历史里同时保留 Revise 文案(第 1 次
    // ExitPlanMode 的结果)与批准文案(第 2 次)。
    let final_text = tool_result_text(&reqs[2]);
    assert!(
        final_text.contains("Plan approved"),
        "第二次批准后模型可见历史应含 'Plan approved' 重写;got: {final_text}"
    );
    assert!(
        final_text.contains("The user requested changes"),
        "Revise 重写应留在历史中(同 turn 内两条工具结果);got: {final_text}"
    );
}

/// `read` 的无副作用替身:Plan 白名单内的纯读工具,循环烧迭代预算用。
struct ReadStub;

#[async_trait]
impl Tool for ReadStub {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "stub read"
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    async fn execute(&self, _ctx: ToolContext, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: vec![zhuye_protocol::ContentBlock::text("ok")],
            is_error: false,
            metadata: Value::Null,
            elapsed_ms: 0,
        })
    }
}

fn read_tool_batch(msg_id: &str, tool_call_id: &str) -> Vec<ChatEvent> {
    vec![
        ChatEvent::MessageStart {
            id: msg_id.into(),
            model: "stub-1".into(),
        },
        ChatEvent::ToolUseStart {
            id: tool_call_id.into(),
            name: "read".into(),
            input_json: String::new(),
        },
        ChatEvent::ToolUseDelta("{}".into()),
        ChatEvent::MessageStop,
    ]
}

fn tool_names(req: &zhuye_llm::ChatRequest) -> Vec<&str> {
    req.tools
        .iter()
        .map(|t| match t {
            zhuye_llm::ToolSpec::Function { name, .. } => name.as_str(),
        })
        .collect()
}

/// v1.x:force_final_answer 在 Plan 模式保留 ExitPlanMode。max_iterations
/// 触顶的强制收口轮此前清空全部工具 —— 调研耗尽预算的 plan 永远无法经
/// PlanReady 提交,只能纯文本收尾。收口轮的 request.tools 必须仍含
/// ExitPlanMode;非 Plan 模式保持清空(收口语义不变)。
///
/// v1.x 再修复:收口轮改发**全量工具**(plan 文件可能尚未写,只有
/// ExitPlanMode 无法满足「先写后交」前置)且工具调用会被执行 —— 本测试
/// 断言收口轮仍含 ExitPlanMode(其余工具为注册表全集,顺序不敏感)。
#[tokio::test]
async fn force_final_answer_keeps_exit_plan_mode_in_plan_mode() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(RecordingStub::new(vec![
        read_tool_batch("m1", "tc1"),
        read_tool_batch("m2", "tc2"),
        read_tool_batch("m3", "tc3"),
    ]));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let cfg = AgentConfig::new("stub/m1", Path::new("."))
        .with_initial_permission_mode(PermissionMode::Plan)
        .with_max_iterations(3);
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(ReadStub));
    tools.register(Arc::new(ExitPlanModeStub));
    let thread = Arc::new(AgentThread::new(cfg, registry, tools, None, None));

    let mut handle = thread
        .submit(user_sub("sub-ff", "research then plan"))
        .await;
    while let Some(ev) = timeout(Duration::from_secs(5), handle.next())
        .await
        .expect("event arrives within 5s")
    {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }
    let reqs = stub.requests();
    assert!(
        reqs.len() >= 4,
        "3 轮工具 + 1 轮强制收口,至少 4 次调用;got {}",
        reqs.len()
    );
    let final_tools = tool_names(&reqs[3]);
    assert!(
        final_tools.contains(&"ExitPlanMode"),
        "Plan 模式收口轮必须保留 ExitPlanMode(唯一的计划提交出口),got {final_tools:?}"
    );
    assert!(
        final_tools.contains(&"read"),
        "Plan 模式收口轮发全量工具(plan 可能尚未写,需要 PlanWrite),got {final_tools:?}"
    );
}

/// 对照:非 Plan 模式的收口轮仍清空全部工具(既有语义不变)。
#[tokio::test]
async fn force_final_answer_clears_tools_outside_plan_mode() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(RecordingStub::new(vec![
        read_tool_batch("m1", "tc1"),
        read_tool_batch("m2", "tc2"),
        read_tool_batch("m3", "tc3"),
    ]));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_max_iterations(3);
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(ReadStub));
    tools.register(Arc::new(ExitPlanModeStub));
    let thread = Arc::new(AgentThread::new(cfg, registry, tools, None, None));

    let mut handle = thread.submit(user_sub("sub-ff2", "just loop")).await;
    while let Some(ev) = timeout(Duration::from_secs(5), handle.next())
        .await
        .expect("event arrives within 5s")
    {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }
    let reqs = stub.requests();
    assert!(reqs.len() >= 4, "got {}", reqs.len());
    let final_tools = tool_names(&reqs[3]);
    assert!(
        final_tools.is_empty(),
        "非 Plan 模式收口轮应清空工具,got {final_tools:?}"
    );
}

fn op_sub(submission_id: &str, op: Op) -> Submission {
    Submission {
        id: submission_id.into(),
        op,
        workspace: None,
        client_user_message_id: None,
        trace: None,
        source_command: None,
    }
}

/// v1.x:用户手动切模式(`Op::SetPermissionMode`)必须清空在途 plan 审批
/// waiter —— 此前 blocking 等待只听会话级取消,切模式后 turn 悬挂在
/// 审批上不收尾;且迟到的 PlanApproval 会二次翻转模式覆盖用户设置。
#[tokio::test]
async fn set_permission_mode_cancels_pending_plan_approval() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(RecordingStub::new(vec![exit_plan_mode_batch("m1", "tc1")]));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let cfg = AgentConfig::new("stub/m1", Path::new("."))
        .with_initial_permission_mode(PermissionMode::Plan)
        .with_approvals(true);
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(ExitPlanModeStub));
    let thread = Arc::new(AgentThread::new(cfg, registry, tools, None, None));

    // 模式事件走 session 广播(回合开始时 turn 通道已从 turn_subs 摘除)。
    let mut session_rx = thread.subscribe_session();
    let mut handle = thread.submit(user_sub("sub-sw", "plan something")).await;
    let mut completed = false;
    while let Some(ev) = timeout(Duration::from_secs(5), handle.next())
        .await
        .expect("event arrives within 5s (turn 不得悬挂在审批等待上)")
    {
        match ev.msg {
            EventMsg::PlanReady(_) => {
                // 审批弹窗待决时用户直接切走模式(不经审批)。
                let t = thread.clone();
                tokio::spawn(async move {
                    t.submit(op_sub(
                        "sub-setmode",
                        Op::SetPermissionMode {
                            mode: PermissionMode::Auto,
                        },
                    ))
                    .await;
                });
            }
            EventMsg::TurnComplete(_) => {
                completed = true;
                break;
            }
            _ => {}
        }
    }
    assert!(completed, "切模式后 turn 必须正常收尾(不悬挂)");
    // 回合通道收尾后,再排空 session 广播里的模式事件(至多等 1s)。
    let mut mode_changes: Vec<(PermissionMode, PermissionMode)> = Vec::new();
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(1);
    while mode_changes.len() < 2 && std::time::Instant::now() < drain_deadline {
        let Ok(Some(ev)) = timeout(Duration::from_millis(200), session_rx.recv()).await else {
            continue;
        };
        if let EventMsg::PermissionModeChanged(m) = ev.msg {
            mode_changes.push((m.from, m.to));
        }
    }
    // 恰好两次模式事件:会话头初始通告(auto→plan)+ 手动切换(plan→auto);
    // 若迟到审批仍能翻转模式会出现第三次。
    assert_eq!(
        mode_changes,
        vec![
            (PermissionMode::Auto, PermissionMode::Plan),
            (PermissionMode::Plan, PermissionMode::Auto),
        ],
        "模式轨迹应为 auto→plan(初始通告)→auto(手动切换),got {mode_changes:?}"
    );
}

/// v1.x:`Op::Shutdown` 清空 plan gate waiters + 会话令牌级联 —— 审批
/// 待决时关闭会话,turn 必须在限时内解除(handle 关闭、ShutdownComplete
/// 到达),不得悬挂。
#[tokio::test]
async fn shutdown_clears_pending_plan_approval() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(RecordingStub::new(vec![exit_plan_mode_batch("m1", "tc1")]));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let cfg = AgentConfig::new("stub/m1", Path::new("."))
        .with_initial_permission_mode(PermissionMode::Plan)
        .with_approvals(true);
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(ExitPlanModeStub));
    let thread = Arc::new(AgentThread::new(cfg, registry, tools, None, None));

    // blocking 派发的 PlanReady 只进 turn 通道(无 session 扇出),从 turn
    // 通道触发;ShutdownComplete 走 session 广播 + Op 自己的通道,从
    // session 通道断言。
    let mut session_rx = thread.subscribe_session();
    let mut handle = thread.submit(user_sub("sub-sd", "plan something")).await;
    let mut turn_ended = false;
    while let Some(ev) = timeout(Duration::from_secs(5), handle.next())
        .await
        .expect("event arrives within 5s (shutdown 不得悬挂在审批等待上)")
    {
        match ev.msg {
            EventMsg::PlanReady(_) => {
                let t = thread.clone();
                tokio::spawn(async move {
                    t.submit(op_sub("sub-shutdown", Op::Shutdown)).await;
                });
            }
            EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) => {
                turn_ended = true;
                break;
            }
            _ => {}
        }
    }
    assert!(turn_ended, "Shutdown 后 turn 必须收尾(不悬挂在审批等待上)");
    // session 广播里排空找 ShutdownComplete(至多 2s)。
    let mut saw_shutdown = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        let Ok(Some(ev)) = timeout(Duration::from_millis(200), session_rx.recv()).await else {
            if session_rx.is_closed() {
                break;
            }
            continue;
        };
        if matches!(ev.msg, EventMsg::ShutdownComplete) {
            saw_shutdown = true;
            break;
        }
    }

    assert!(
        saw_shutdown,
        "必须收到 ShutdownComplete 且事件流正常关闭(不悬挂)"
    );
}
