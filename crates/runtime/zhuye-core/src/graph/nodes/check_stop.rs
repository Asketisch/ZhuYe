//! `check_stop` 节点 — 分发 `Stop` hook;并在有在飞异步子代理时挂起等待。

use zhuye_hooks::{HookEvent, StopReason};
use zhuye_llm::{ChatMessage, ContentBlock, UserContent};

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
/// 顺序上先等子代理、再判 Stop hook:让模型先看到结果再决定是否收尾,
/// 避免「hook 放行 → 回合结束 → 结果无处可去」。
///
/// 等待被取消(回合中断 / 会话关闭)时返回 [`GraphNode::PreLoop`],由
/// `model_call` 入口的取消检查收敛成 `TurnAborted` —— 不走 `None`
/// 分支,避免把「被打断」误报成 `completed_normally`。
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
    if let Some(registry) = ctx.pending_subagents.as_ref()
        && registry.has_pending()
    {
        let Some(completed) = registry.wait_any(&ctx.cancel).await else {
            // 等待被取消:回到 PreLoop,由 model_call 的取消检查收敛。
            tracing::info!("异步子代理等待被取消,回合中止");
            return Some(GraphNode::PreLoop);
        };
        if !completed.is_empty() {
            inject_subagent_results(state, &completed);
            return Some(GraphNode::PreLoop);
        }
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

/// 把已完成的异步子代理结果注入本轮上下文。
///
/// 注入形态:尾部 user `<system-reminder>` 追加 —— 与 steering 注入、
/// Stop hook 注入提醒同纪律(append-only,不改写已发送前缀,保 prompt
/// cache 前缀稳定)。
///
/// **不落盘**:与 steering / hook 提醒一致,这类回合中途注入是 ephemeral
/// 的。进程崩溃后该结果丢失(子代理自身的 JSONL 仍在,带
/// `parent_session_id` 血缘可追溯);回合中途崩溃本身也会被输入守恒
/// ledger 标记为「未完成输入」,不静默重放。
fn inject_subagent_results(state: &mut AgentState, results: &[zhuye_tools::PendingSubagentResult]) {
    for r in results {
        let body = if r.ok {
            r.text.clone()
        } else {
            format!(
                "子代理执行失败:{}",
                r.error.clone().unwrap_or_else(|| "未知错误".to_string())
            )
        };
        state.messages.messages.push(ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::Text {
                text: format!(
                    "<system-reminder>\n[子代理 {} 已完成(session={})]\n{}\n</system-reminder>",
                    r.role, r.child_id, body
                ),
            }],
        }));
    }
    tracing::info!(count = results.len(), "异步子代理结果已注入,回合续跑");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    use zhuye_hooks::{HookDecision, HookEngine, SystemMessage};

    /// 仅用于 `check_stop` 的最小 `NodeContext` —— 只填充 hook_engine,
    /// 其余字段取空值(check_stop 只读 `hook_engine` / `pending_subagents`,
    /// 写 `state`)。
    fn minimal_ctx(engine: Arc<HookEngine>) -> NodeContext {
        minimal_ctx_with(engine, None)
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

    /// 在飞异步子代理终结后:回合**不结束**,而是注入结果并续跑本轮。
    #[tokio::test]
    async fn pending_subagent_result_is_injected_and_turn_continues() {
        let registry = Arc::new(zhuye_tools::PendingSubagentRegistry::new());
        registry.register("child-1", "explorer", "call-1");
        let ctx = minimal_ctx_with(Arc::new(HookEngine::new()), Some(registry.clone()));
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
        match &state.messages.messages[0] {
            ChatMessage::User(u) => {
                let text = u
                    .blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                assert!(text.contains("explorer"), "注入应带子代理角色:{text}");
                assert!(text.contains("调研结论"), "注入应带子代理结果:{text}");
                assert!(
                    text.starts_with("<system-reminder>"),
                    "注入必须走 system-reminder 形态:{text}"
                );
            }
            other => panic!("expected User reminder message, got {other:?}"),
        }
    }

    /// 失败的子代理:结果同样注入,但标明失败原因。
    #[tokio::test]
    async fn failed_subagent_result_is_injected_with_error() {
        let registry = Arc::new(zhuye_tools::PendingSubagentRegistry::new());
        registry.register("child-1", "explorer", "call-1");
        let ctx = minimal_ctx_with(Arc::new(HookEngine::new()), Some(registry.clone()));
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
        let ChatMessage::User(u) = &state.messages.messages[0] else {
            panic!("结果应以 User 消息注入");
        };
        let text = u
            .blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("子代理执行失败"), "{text}");
        assert!(text.contains("子代理请求被拒绝"), "{text}");
    }

    /// 等待被取消(回合中断)→ 不返回 `None`(那会被记成正常完成),
    /// 而是回到 PreLoop,由 model_call 的取消检查收敛成 TurnAborted。
    #[tokio::test]
    async fn cancelled_wait_returns_preloop_not_none() {
        let registry = Arc::new(zhuye_tools::PendingSubagentRegistry::new());
        registry.register("child-1", "explorer", "call-1");
        let ctx = minimal_ctx_with(Arc::new(HookEngine::new()), Some(registry.clone()));
        ctx.cancel.cancel();
        let mut state = AgentState::default();

        assert_eq!(
            check_stop(&mut state, &ctx).await,
            Some(GraphNode::PreLoop),
            "被打断的回合不能走 None 分支(会被记成 completed_normally)"
        );
        assert!(
            state.messages.messages.is_empty(),
            "被取消的等待不应注入结果"
        );
    }
}
