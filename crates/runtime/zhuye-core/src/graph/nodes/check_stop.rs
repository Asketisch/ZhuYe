//! `check_stop` 节点 — 分发 `Stop` hook;并在有在飞异步子代理时挂起等待。

use zhuye_hooks::{HookEvent, StopReason};
use zhuye_llm::{ChatMessage, ContentBlock, UserContent};
use zhuye_protocol::PermissionMode;

use crate::graph::GraphNode;
use crate::graph::state::AgentState;
use crate::submission_loop::NodeContext;

/// `check_stop` — 分发 `Stop` hook。若 hook 拒绝,turn 继续;
/// 否则 turn 结束。
///
/// ## 异步子代理的挂起 / 激活
///
/// 模型的「不再发工具调用」不等于「本轮真的做完了」—— 若还有在飞的异步
/// 子代理(`mode="async"` 派发),模型只是**还没拿到结果**。此时回合不应
/// 结束,而应:
///
/// 1. 挂起等待子代理终结(`PendingSubagentRegistry::wait_any`);
/// 2. 把结果作为尾部 user `<system-reminder>` 注入上下文;
/// 3. 回到 [`GraphNode::PreLoop`] 续跑**本轮** —— 下一轮 `model_call`
///    就能基于结果继续推理。
///
/// 顺序上先等子代理、再判计划收尾守卫、再判 Stop hook:让模型先拿到结果、
/// 再被要求提交计划,最后才轮到 hook 决定是否收尾 —— 避免「hook 放行 →
/// 回合结束 → 结果无处可去」。
///
/// 等待被取消(回合中断 / 会话关闭)时直接返回 `None`:取消不算自然完成,
/// `StateGraph::run` 只在回合令牌未触发时才把 `CheckStop` 的 `None` 记成
/// `completed_normally`,终态由回合任务按取消路径收敛成 `TurnAborted`。
/// (修复前这里返回 `PreLoop` 绕道 —— 已取消的回合会完整跑一遍前置流水线:
/// PreCompact hook、压缩判定、steering 收割,然后才在 `model_call` 入口被
/// 取消检查拦下。)
///
/// 决策经 [`zhuye_hooks::HookDecision::resolve`] 展开:内置 Stop hook
/// (`verification` / `plan_completion`) 否决时返回
/// `Combined([InjectMessage, Deny])`,`HookEngine::merge` 的产物仍是含
/// `Deny` 叶子的 `Combined` —— 此前顶层 `match` 只认单个 `Deny` 变体,
/// 该形态落入 allow 分支,否决被静默丢弃、注入的失败反馈(如测试输出)
/// 也永远到不了模型。现在按叶子判定否决,并把 hook 注入的提醒以
/// system-reminder User 消息追加进历史,让续作轮能看到否决原因。
pub async fn check_stop(state: &mut AgentState, ctx: &NodeContext) -> Option<GraphNode> {
    // ── 在飞异步子代理 → 挂起等待,拿到结果后注入并续跑本轮 ──
    // 按**本回合**的令牌筛选:登记表是每线程一份,而同一线程上可能并发跑
    // 多个回合 —— 别的回合派发的子代理既不该被本回合等到,也不该被注入到
    // 本回合的上下文里。
    if let Some(registry) = ctx.pending_subagents.as_ref()
        && registry.has_pending_for(&ctx.cancel)
    {
        let Some(completed) = registry.wait_any_for(&ctx.cancel, &ctx.cancel).await else {
            // 等待被取消:回合收尾。本回合仍在飞的子代理由回合任务收尾时的
            // `abandon_turn` 一并取消(它们监听同一回合令牌,此刻已停)。
            tracing::info!("异步子代理等待被取消,回合中止");
            return None;
        };
        if !completed.is_empty() {
            inject_subagent_results(state, ctx, completed).await;
            return Some(GraphNode::PreLoop);
        }
    }

    // v1.x 计划收尾守卫:Plan 模式下 plan 文件已写、PlanReady 未派发时,
    // 否决「纯文本收尾」并逼模型立即调 ExitPlanMode(上限 2 次,防死循环;
    // 命中上限说明模型确实无法收口,放行由用户人工接管)。实测 session
    // 889c3bd0:PlanWrite 成功后模型被 nudge 逼成「FINAL ANSWER: 计划已
    // 写入…」收尾,PlanReady 永不派发,审批链路死锁。
    if ctx.cfg.permission_mode() == PermissionMode::Plan
        && state.plan_draft_written
        && !state.plan_ready_dispatched
        && state.plan_stop_guard_attempts < 2
    {
        state.plan_stop_guard_attempts = state.plan_stop_guard_attempts.saturating_add(1);
        tracing::warn!(
            attempt = state.plan_stop_guard_attempts,
            "plan stop guard: plan written but ExitPlanMode not called; vetoing text completion"
        );
        let set = ctx.cfg.resolve_prompt(&ctx.model.read()).set;
        let nudge = set.get_or(
            "runtime.nudge-plan-submit",
            "The plan file has been written with PlanWrite but ExitPlanMode has NOT been              called yet. Call ExitPlanMode NOW to submit the plan for user approval. Do not \
             output any other content.",
        );
        state.messages.messages.push(ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::Text {
                text: format!("<system-reminder>\n{}\n</system-reminder>", nudge),
            }],
        }));
        return Some(GraphNode::PreLoop);
    }

    let decision = ctx
        .hook_engine
        .dispatch(&HookEvent::Stop {
            reason: StopReason::AgentDecision,
            attempt: state.stop_hook_attempts,
        })
        .await;
    let resolved = decision.resolve();
    match resolved.deny_reason {
        Some(reason) => {
            state.stop_hook_attempts = state.stop_hook_attempts.saturating_add(1);
            tracing::warn!(%reason, attempt = state.stop_hook_attempts, "Stop hook vetoed completion");
            // 投递 hook 注入的提醒(否决时才有下一轮 model_call 可注入)。
            for m in &resolved.injected {
                state.messages.messages.push(ChatMessage::User(UserContent {
                    blocks: vec![ContentBlock::Text {
                        text: format!("<system-reminder>\n{}\n</system-reminder>", m.content),
                    }],
                }));
            }
            Some(GraphNode::PreLoop)
        }
        None => None,
    }
}

/// 把已完成的异步子代理结果注入本轮上下文,并写入会话记录。
///
/// 注入形态:尾部 user `<system-reminder>` 追加 —— 与 steering 注入、
/// Stop hook 注入提醒同纪律(append-only,不改写已发送前缀,保 prompt
/// cache 前缀稳定)。一批结果合成**一条** User 消息(每个结果一个文本块),
/// 不产生连续多条 user 消息。
///
/// **落盘**:与 steering 插话同格式(`role=user` 记录,挂在派发回合名下)。
/// 修复原因:此前不落盘 —— 引擎每轮从 rollout 重建历史,注入的结果只在
/// 当前回合可见;下一轮模型看到的是「已派发」占位回执加自己引用了该结果
/// 的收口文本,却找不到结果本身。回放侧 `coalesce_turn_messages` 本就为
/// 回合中途的 user 记录保留了位置(它会关闭当前回合的 assistant 归并头),
/// 顺序不会被打乱。
///
/// 结果正文走与同步工具回执相同的上下文预算(`limit_model_tool_output`,
/// 逐条计):子代理可能返回大段转储,原样注入会绕开 `tool_exec` 的输出
/// 治理;超出 rollout 单字段上限的部分落盘时还会被截断,回合内与恢复后
/// 看到的内容就不一致。预算只作用于正文,`<system-reminder>` 包裹保持完整。
async fn inject_subagent_results(
    state: &mut AgentState,
    ctx: &NodeContext,
    results: Vec<zhuye_tools::PendingSubagentResult>,
) {
    let budget = ctx.cfg.result_budget.as_deref();
    let mut protocol_blocks: Vec<zhuye_protocol::ContentBlock> = Vec::with_capacity(results.len());
    for r in results {
        let body = if r.ok {
            r.text
        } else {
            format!(
                "子代理执行失败:{}",
                r.error.unwrap_or_else(|| "未知错误".to_string())
            )
        };
        let mut body_blocks = vec![zhuye_protocol::ContentBlock::Text { text: body }];
        super::tool_exec::limit_model_tool_output(
            &mut body_blocks,
            &ctx.cfg.current_workspace(),
            budget,
        )
        .await;
        let body = body_blocks
            .iter()
            .filter_map(|block| match block {
                zhuye_protocol::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        protocol_blocks.push(zhuye_protocol::ContentBlock::Text {
            text: format!(
                "<system-reminder>\n[子代理 {} 已完成(session={})]\n{body}\n</system-reminder>",
                r.role, r.child_id
            ),
        });
    }
    if protocol_blocks.is_empty() {
        return;
    }
    let llm_blocks: Vec<ContentBlock> = protocol_blocks
        .iter()
        .filter_map(|block| match block {
            zhuye_protocol::ContentBlock::Text { text } => {
                Some(ContentBlock::Text { text: text.clone() })
            }
            _ => None,
        })
        .collect();
    state
        .messages
        .messages
        .push(ChatMessage::User(UserContent { blocks: llm_blocks }));
    if let Some(rec) = ctx.recorder.as_ref() {
        let _ = rec
            .record(zhuye_protocol::RolloutRecord::message(
                ctx.turn_id,
                zhuye_protocol::MessageRole::User,
                serde_json::to_value(&protocol_blocks).unwrap_or(serde_json::Value::Null),
            ))
            .await;
    }
    tracing::info!(
        count = protocol_blocks.len(),
        "异步子代理结果已注入并落盘,回合续跑"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    use zhuye_hooks::{HookDecision, HookEngine, SystemMessage};

    /// 仅用于 `check_stop` 的最小 `NodeContext` —— 只填充 hook_engine,
    /// 其余字段取空值(check_stop 读 `hook_engine` / `pending_subagents` /
    /// `cfg` / `recorder`,写 `state`)。
    fn minimal_ctx(engine: Arc<HookEngine>) -> NodeContext {
        minimal_ctx_with(engine, None)
    }

    /// 测试用 recorder:把每次 record 调用收进 Vec,供断言落盘形态。
    #[derive(Debug, Default)]
    struct CapturingRecorder(parking_lot::Mutex<Vec<zhuye_protocol::RolloutRecord>>);

    #[async_trait::async_trait]
    impl zhuye_protocol::RolloutRecorder for CapturingRecorder {
        async fn record(
            &self,
            r: zhuye_protocol::RolloutRecord,
        ) -> Result<(), zhuye_protocol::RolloutError> {
            self.0.lock().push(r);
            Ok(())
        }
        async fn replay(
            &self,
            _session_id: zhuye_protocol::ThreadId,
        ) -> Result<Vec<zhuye_protocol::RolloutRecord>, zhuye_protocol::RolloutError> {
            Ok(self.0.lock().clone())
        }
        async fn list_sessions(
            &self,
        ) -> Result<Vec<zhuye_protocol::SessionInfo>, zhuye_protocol::RolloutError> {
            Ok(Vec::new())
        }
        async fn truncate_after(
            &self,
            _to_turn_id: Option<&zhuye_protocol::TurnId>,
        ) -> Result<usize, zhuye_protocol::RolloutError> {
            Ok(0)
        }
    }

    /// 取某条 User 消息的全部文本(断言辅助)。
    fn user_text(message: &ChatMessage) -> String {
        let ChatMessage::User(u) = message else {
            panic!("expected User reminder message, got {message:?}");
        };
        u.blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// 带在飞子代理表的最小 `NodeContext`。
    fn minimal_ctx_with(
        engine: Arc<HookEngine>,
        pending_subagents: Option<Arc<zhuye_tools::PendingSubagentRegistry>>,
    ) -> NodeContext {
        let (event_tx, _rx) = tokio::sync::mpsc::channel::<zhuye_protocol::Event>(8);
        NodeContext {
            session_seq: None,
            turn_id: zhuye_protocol::TurnId::new(),
            session_id: zhuye_protocol::ThreadId::new(),
            model: Arc::new(parking_lot::RwLock::new("stub/model".to_string())),
            policy: Arc::new(zhuye_llm::RoutingPolicy::default()),
            registry: Arc::new(zhuye_llm::ModelRegistry::new()),
            hook_engine: engine,
            tools_queue: Arc::new(zhuye_tools::ToolExecutionQueue::with_defaults(
                Arc::new(zhuye_tools::ToolRegistry::new()),
                Arc::new(HookEngine::new()),
                zhuye_tools::ToolContext::default(),
            )),
            sub_id: "sub-test".into(),
            cancel: CancellationToken::new(),
            event_tx,
            messages: vec![],
            max_iterations: 32,
            m4: None,
            recorder: None,
            approval_gate: None,
            effort: Arc::new(parking_lot::RwLock::new(
                zhuye_protocol::ReasoningEffortMirror::Medium,
            )),
            session_usage: Arc::new(parking_lot::RwLock::new(
                zhuye_protocol::TokenUsage::default(),
            )),
            token_budget: Arc::new(parking_lot::RwLock::new(None)),
            force_compact_next: Arc::new(parking_lot::RwLock::new(false)),
            telemetry: None,
            goal: Arc::new(parking_lot::RwLock::new(None)),
            todos_slot: Arc::new(parking_lot::RwLock::new(None)),
            quota_tracker: None,
            plan_approval_gate: None,
            plan_session_subs: None,
            steering_queue: None,
            pending_subagents,
            sandbox_override: None,
            cfg: crate::config::AgentConfig::new("stub/model", "/tmp"),
        }
    }

    /// 注册一个返回 `Combined([InjectMessage, Deny])` 的 Stop hook ——
    /// 与内置 `verification` / `plan_completion` 否决时的形态一致。
    struct CombinedDenyStopHook;
    #[async_trait::async_trait]
    impl zhuye_hooks::Hook for CombinedDenyStopHook {
        fn name(&self) -> &str {
            "combined-deny-stop"
        }
        fn events(&self) -> &[zhuye_hooks::HookEventKind] {
            &[zhuye_hooks::HookEventKind::Stop]
        }
        async fn handle(&self, _event: &HookEvent) -> Result<HookDecision, zhuye_hooks::HookError> {
            Ok(HookDecision::Combined(vec![
                HookDecision::InjectMessage(SystemMessage::new("tests failing: 2 failed")),
                HookDecision::Deny {
                    reason: "tests failing".into(),
                },
            ]))
        }
    }

    /// 回归:Stop hook 以 `Combined([InjectMessage, Deny])` 否决时,
    /// `check_stop` 必须返回 `PreLoop`(续作),而不是 allow 终止;
    /// 且注入的提醒必须进历史,供下一轮 model_call 看见。
    #[tokio::test]
    async fn combined_deny_vetoes_and_injects_message() {
        let engine = Arc::new(HookEngine::new());
        engine.register(CombinedDenyStopHook);
        let ctx = minimal_ctx(engine);
        let mut state = AgentState::default();

        let next = check_stop(&mut state, &ctx).await;
        assert_eq!(
            next,
            Some(GraphNode::PreLoop),
            "Combined([InjectMessage, Deny]) 必须否决 turn 完成"
        );
        assert_eq!(state.stop_hook_attempts, 1);
        // 注入的提醒必须以 system-reminder 形态追加进历史。
        assert_eq!(state.messages.messages.len(), 1);
        match &state.messages.messages[0] {
            ChatMessage::User(u) => {
                assert!(u.blocks.iter().any(
                    |b| matches!(b, ContentBlock::Text { text } if text.contains("tests failing"))
                ));
            }
            other => panic!("expected User reminder message, got {other:?}"),
        }
    }

    /// 无 hook 时 allow → turn 自然结束。
    #[tokio::test]
    async fn no_hooks_terminates() {
        let ctx = minimal_ctx(Arc::new(HookEngine::new()));
        let mut state = AgentState::default();
        assert_eq!(check_stop(&mut state, &ctx).await, None);
        assert_eq!(state.stop_hook_attempts, 0);
    }

    /// 在飞异步子代理终结后:回合**不结束**,而是注入结果、落盘并续跑本轮。
    ///
    /// 回归(落盘):引擎每轮从 rollout 重建历史,结果不落盘就只在当前回合
    /// 可见 —— 下一轮模型只剩「已派发」占位回执。落盘形态与 steering 插话
    /// 一致:`role=user` 记录,挂在派发回合名下。
    #[tokio::test]
    async fn pending_subagent_result_is_injected_persisted_and_turn_continues() {
        let registry = Arc::new(zhuye_tools::PendingSubagentRegistry::new());
        let mut ctx = minimal_ctx_with(Arc::new(HookEngine::new()), Some(registry.clone()));
        // 登记用**本回合**的令牌(与工具侧 `ToolContext.cancel` 同源)。
        registry.register("child-1", "explorer", "call-1", ctx.cancel.clone());
        let recorder = Arc::new(CapturingRecorder::default());
        ctx.recorder = Some(recorder.clone());
        let mut state = AgentState::default();

        // 后台任务稍后回填终态 —— 模拟子代理跑完。
        let reg = registry.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            reg.complete(zhuye_tools::PendingSubagentResult {
                child_id: "child-1".into(),
                role: "explorer".into(),
                call_id: "call-1".into(),
                ok: true,
                text: "调研结论:共 3 个模块".into(),
                error: None,
            });
        });

        let next = check_stop(&mut state, &ctx).await;
        writer.await.unwrap();

        assert_eq!(
            next,
            Some(GraphNode::PreLoop),
            "有在飞子代理时回合必须续跑,不能结束"
        );
        assert_eq!(state.messages.messages.len(), 1, "结果应注入一条消息");
        let text = user_text(&state.messages.messages[0]);
        assert!(text.contains("explorer"), "注入应带子代理角色:{text}");
        assert!(text.contains("调研结论"), "注入应带子代理结果:{text}");
        assert!(
            text.starts_with("<system-reminder>"),
            "注入必须走 system-reminder 形态:{text}"
        );

        // 落盘:注入的同一份文本以 user 记录写入,归属本回合。
        let records = recorder.0.lock().clone();
        let [record] = records.as_slice() else {
            panic!("应落盘且只落盘一条 user 记录:{records:?}");
        };
        let zhuye_protocol::RolloutRecord::Message {
            turn_id,
            role,
            content,
        } = record
        else {
            panic!("落盘记录应是 Message:{record:?}");
        };
        assert_eq!(*role, zhuye_protocol::MessageRole::User);
        assert_eq!(*turn_id, ctx.turn_id);
        assert_eq!(
            *content,
            serde_json::to_value(&[zhuye_protocol::ContentBlock::Text { text }]).unwrap()
        );
    }

    /// 一批多个结果合成**一条** User 消息(每个结果一个文本块),不产生连续
    /// 多条 user 消息。
    #[tokio::test]
    async fn multiple_results_are_injected_as_one_user_message() {
        let registry = Arc::new(zhuye_tools::PendingSubagentRegistry::new());
        let ctx = minimal_ctx_with(Arc::new(HookEngine::new()), Some(registry.clone()));
        registry.register("child-1", "explorer", "call-1", ctx.cancel.clone());
        registry.register("child-2", "reviewer", "call-2", ctx.cancel.clone());
        let mut state = AgentState::default();
        for (child_id, role, call_id) in [
            ("child-1", "explorer", "call-1"),
            ("child-2", "reviewer", "call-2"),
        ] {
            registry.complete(zhuye_tools::PendingSubagentResult {
                child_id: child_id.into(),
                role: role.into(),
                call_id: call_id.into(),
                ok: true,
                text: format!("{role} 的结论"),
                error: None,
            });
        }

        assert_eq!(check_stop(&mut state, &ctx).await, Some(GraphNode::PreLoop));
        assert_eq!(state.messages.messages.len(), 1, "一批结果只应注入一条消息");
        let ChatMessage::User(u) = &state.messages.messages[0] else {
            panic!("结果应以 User 消息注入");
        };
        assert_eq!(u.blocks.len(), 2, "每个结果一个文本块:{u:?}");
        let text = user_text(&state.messages.messages[0]);
        assert!(
            text.contains("explorer 的结论") && text.contains("reviewer 的结论"),
            "{text}"
        );
    }

    /// 超长结果走与同步工具回执相同的上下文预算:正文被截断并溢出到工作区,
    /// `<system-reminder>` 包裹保持完整。
    #[tokio::test]
    #[cfg(unix)]
    async fn oversized_result_is_budgeted_like_a_tool_result() {
        let registry = Arc::new(zhuye_tools::PendingSubagentRegistry::new());
        let workspace = tempfile::tempdir().unwrap();
        let mut ctx = minimal_ctx_with(Arc::new(HookEngine::new()), Some(registry.clone()));
        ctx.cfg = crate::config::AgentConfig::new("stub/model", workspace.path());
        registry.register("child-1", "explorer", "call-1", ctx.cancel.clone());
        let mut state = AgentState::default();
        registry.complete(zhuye_tools::PendingSubagentResult {
            child_id: "child-1".into(),
            role: "explorer".into(),
            call_id: "call-1".into(),
            ok: true,
            text: "调研".repeat(6000),
            error: None,
        });

        assert_eq!(check_stop(&mut state, &ctx).await, Some(GraphNode::PreLoop));
        let text = user_text(&state.messages.messages[0]);
        assert!(
            text.starts_with("<system-reminder>\n[子代理 explorer 已完成(session=child-1)]"),
            "{text}"
        );
        assert!(text.ends_with("</system-reminder>"), "包裹必须完整:{text}");
        assert!(text.contains("已截断"), "超长正文应被预算截断:{text}");
        assert!(
            text.len() < 12 * 1024,
            "注入文本应落在预算内:{}",
            text.len()
        );
    }

    /// 失败的子代理:结果同样注入,但标明失败原因。
    #[tokio::test]
    async fn failed_subagent_result_is_injected_with_error() {
        let registry = Arc::new(zhuye_tools::PendingSubagentRegistry::new());
        let ctx = minimal_ctx_with(Arc::new(HookEngine::new()), Some(registry.clone()));
        registry.register("child-1", "explorer", "call-1", ctx.cancel.clone());
        let mut state = AgentState::default();

        let reg = registry.clone();
        let writer = tokio::spawn(async move {
            reg.complete(zhuye_tools::PendingSubagentResult {
                child_id: "child-1".into(),
                role: "explorer".into(),
                call_id: "call-1".into(),
                ok: false,
                text: String::new(),
                error: Some("子代理请求被拒绝".into()),
            });
        });

        let next = check_stop(&mut state, &ctx).await;
        writer.await.unwrap();
        assert_eq!(next, Some(GraphNode::PreLoop));
        let text = user_text(&state.messages.messages[0]);
        assert!(text.contains("子代理执行失败"), "{text}");
        assert!(text.contains("子代理请求被拒绝"), "{text}");
    }

    /// 等待被取消(回合中断)→ 直接收尾返回 `None`,不注入结果。`None` 不会
    /// 被记成正常完成:`StateGraph::run` 只在回合令牌未触发时才把 `CheckStop`
    /// 的 `None` 记为 `completed_normally`(见 `graph::GraphNode::is_natural_stop`)。
    #[tokio::test]
    async fn cancelled_wait_ends_turn_without_injecting() {
        let registry = Arc::new(zhuye_tools::PendingSubagentRegistry::new());
        let ctx = minimal_ctx_with(Arc::new(HookEngine::new()), Some(registry.clone()));
        registry.register("child-1", "explorer", "call-1", ctx.cancel.clone());
        ctx.cancel.cancel();
        let mut state = AgentState::default();

        assert_eq!(
            check_stop(&mut state, &ctx).await,
            None,
            "被取消的等待应直接收尾,不再绕道 PreLoop 跑前置流水线"
        );
        assert!(
            state.messages.messages.is_empty(),
            "被取消的等待不应注入结果"
        );
        assert!(!GraphNode::is_natural_stop(GraphNode::CheckStop, true));
    }

    /// 别的回合派发的子代理:**不被本回合等到,也不注入本回合上下文**。
    ///
    /// 登记表是每线程一份,而同一线程上可能并发跑着多个回合(`submission_loop`
    /// 每条 UserInput 都 spawn 一个回合任务)。若按整表等待,A 回合的子代理
    /// 会被 B 回合等到并把结果注入 B 的上下文 —— B 的模型会看到一份自己没
    /// 派发过的子代理结果。
    #[tokio::test]
    async fn other_turns_pending_subagent_is_not_injected() {
        let registry = Arc::new(zhuye_tools::PendingSubagentRegistry::new());
        let ctx = minimal_ctx_with(Arc::new(HookEngine::new()), Some(registry.clone()));
        // 别的回合派发的条目:已完成,但不属本回合。
        registry.register(
            "child-other",
            "explorer",
            "call-other",
            CancellationToken::new(),
        );
        registry.complete(zhuye_tools::PendingSubagentResult {
            child_id: "child-other".into(),
            role: "explorer".into(),
            call_id: "call-other".into(),
            ok: true,
            text: "别的回合的结论".into(),
            error: None,
        });
        let mut state = AgentState::default();

        assert_eq!(
            check_stop(&mut state, &ctx).await,
            None,
            "本回合没有在飞子代理 → 正常收尾"
        );
        assert!(
            state.messages.messages.is_empty(),
            "别的回合的子代理结果不应注入本回合"
        );
        assert!(
            registry.has_pending(),
            "别的回合的条目应原样留在表里,等它自己的回合来取"
        );
    }
}
