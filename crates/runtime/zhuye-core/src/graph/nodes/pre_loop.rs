//! `pre_loop` 节点 — M4:完整的 turn 前置流水线。

use zhuye_llm::ChatMessage;
use zhuye_protocol::{
    AbortReason, ContextCompactedStrategy, Event, EventMsg, PermissionMode, RolloutRecord,
};
use zhuye_recovery::{MetaKind, RecoveryEntry};

use crate::graph::GraphNode;
use crate::graph::state::AgentState;
use crate::submission_loop::NodeContext;

pub(super) fn compose_prompt_core(
    context: &crate::graph::state::PromptCoreContext,
    set: &zhuye_prompt::PromptSet,
) -> String {
    let agent_prompt = context
        .custom_main
        .as_deref()
        .unwrap_or_else(|| set.get_or("system.main", "你是一名编码助手。"));
    let guidance = set
        .get("system.plan-mode")
        .filter(|value| !zhuye_prompt::is_section_off(value));
    let mut core = zhuye_prompt::LayeredPrompt::compose_core_with_guidance(
        agent_prompt,
        &context.memory,
        guidance,
    );
    if guidance.is_some() {
        zhuye_prompt::append_active_mode_section_with_set(&mut core, context.mode, set);
    }
    if let Some(checklist) = context
        .checklist
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        core.push_str("\n\n## Current Todo List\n");
        core.push_str(checklist);
        core.push_str("\nThis is your own working checklist — keep it up to date with the `TodoWrite` tool, and work through the remaining items.\n");
    }
    core
}

/// `pre_loop` — M4:完整的 turn 前置流水线。
///
/// 当 `ctx.m4` 为 `Some`(生产):
/// 1. 取 `ctx.messages` 中的用户输入,载入 `state.messages`。
/// 2. 对消息列表调 `compactor.compact`;若某策略触发,发出
///    `ContextCompacted` 并更新 `state.compact_triggered`。
/// 3. 按 `active_agent_def.memory` 加载内存(project + user + session 作用域);
///    截断到 8KB。
/// 4. 构建分层 system prompt(core = agent def + memory;
///    ephemeral = tools + skills catalog + iteration reminder)。
/// 5. 计算 `state.effective_tools`(always_on ∪ active_skills)。
/// 6. 把结果存回 `state.messages`,供 `model_call` 使用。
///
/// 当 `ctx.m4` 为 `None`(测试 / 旧路径):直接透传。`state.messages.messages`
/// 保持空;`model_call` 回退到 `ctx.messages`(向后兼容)。
pub async fn pre_loop(state: &mut AgentState, ctx: &NodeContext) -> Option<GraphNode> {
    let Some(m4) = ctx.m4.as_ref() else {
        return Some(GraphNode::ModelCall);
    };

    let model = ctx.model.read().clone();
    let window = ctx
        .cfg
        .context_window_override_for(&model)
        .or_else(|| zhuye_llm::context_window_for(&model));
    let compactor = m4.compactor.for_window(window);
    tracing::debug!(model = %model, window, threshold = compactor.trigger_tokens(), source = compactor.trigger_source(), window_known = window.is_some(), "上下文压缩决策配置");
    if !state.history_seeded && window.is_none() && compactor.trigger_source() == "window_ratio" {
        tracing::warn!(model = %model, fallback_window = 128_000, threshold = compactor.trigger_tokens(), "未知模型窗口，压缩阈值使用回退值");
    }
    if !state.history_seeded && window.is_some_and(|w| compactor.trigger_tokens() > w) {
        tracing::warn!(model = %model, window, threshold = compactor.trigger_tokens(), source = compactor.trigger_source(), "压缩阈值超过模型窗口");
    }

    // Review 2026-06-29 BUG-2 修复: `compact_triggered` 必须先重置再决策,
    // 否则 `AgentState` 跨 turn 持久化时,上一轮触发的 true 会泄漏到本轮,
    // 导致 active file meta-message 反复注入。
    state.compact_triggered = false;

    // 1. 从初始用户输入(`ctx.messages`)播种 `state.messages`。
    // 仅在本 turn 首次进入 pre_loop 时播种;之后 model_call / tool_exec 会把
    // assistant 消息与工具结果 commit 进 state.messages,若每轮都 clear 会
    // 丢弃这些记录 —— 模型每轮只看到原始用户问题,于是重复发同样的工具
    // 调用(M2 多步历史丢失 bug)。
    if !state.history_seeded {
        state.messages.messages.clear();
        for m in &ctx.messages {
            state.messages.messages.push(m.clone());
        }
        // 回放历史将最近一次持久化摘要作为 System 消息注入。每个回合
        // 都新建 AgentState,需从回放内容恢复摘要,后续压缩才能增量更新。
        if state.compaction_summary.is_none() {
            state.compaction_summary = state.messages.messages.iter().rev().find_map(|m| match m {
                ChatMessage::System(s)
                    if s.trim_start().starts_with("<summary>") && s.contains("</summary>") =>
                {
                    Some(s.clone())
                }
                _ => None,
            });
        }
        state.history_seeded = true;
    }

    // 2. Compact。M8:把**最近一次** LLM 调用上报的 `input_tokens` 透传,
    // 使之优先于本地 `estimate_messages` 启发式(见
    // `Compactor::compact_with_prior_and_tokens`)。
    //
    // 注意:必须用 `last_llm_input_tokens`(最近一次调用的输入,即权威
    // 上下文大小),**不能**用 `total_usage.input_tokens`(turn 内逐次
    // model_call 的累计值)—— 累计值随迭代数线性增长,长 turn 下会在
    // 每次迭代都误触发 compaction(默认阈值 10k,3 次调用 × ~5k 输入
    // 即越线),导致 microcompact / smart_prune / LLM 摘要反复空转。
    // turn 首次 pre_loop 时尚无先验调用(`None`),仅以本地估算为信号。
    let llm_reported = state.last_llm_input_tokens.filter(|n| *n > 0);
    // v1.2 P1-12(已有-B):`/compact` 手动触发 —— 读 + 清零
    // `force_compact_next`,若为 true 则把 `llm_reported` 强制成 `u32::MAX`
    // 让 `before_tokens` 超过任何阈值,compactor 必然运行(microcompact /
    // smart_prune)。这样 `Op::Compact` 设置的标志在本轮 pre_loop 生效。
    let force_compact = {
        let mut g = ctx.force_compact_next.write();
        let v = *g;
        *g = false;
        v
    };
    // v1.x:`/compact <指示>` 的附加指示 —— 与强制标志同批取出(仅在
    // 手动压缩本轮注入摘要 prompt;阈值触发的自动压缩不消费残留值)。
    let compact_focus = if force_compact {
        ctx.cfg.take_compact_focus()
    } else {
        None
    };
    let llm_reported = if force_compact {
        Some(u32::MAX)
    } else {
        llm_reported
    };
    // v1.x:rapid-refill 熔断(借鉴同类 agent 的 compact rapid-refill 熔断)——
    // 「压缩 → 立即填满 → 再压缩」是纯烧钱的死循环,专门熔断。预检只在
    // 「本次真的会触发压缩」(LLM 上报 ≥ 阈值,或手动 force)时进行:
    // 窗口内 refill 前两次跳过压缩并注入提醒让模型收敛,连续第 3 次
    // 直接以结构化错误收口当前 turn。手动 `/compact`(force)绕过守卫
    // —— 用户显式指令不被熔断拦截。
    let step_no = ctx
        .cfg
        .model_step_counter
        .load(std::sync::atomic::Ordering::Relaxed);
    let estimated = zhuye_compact::estimate_messages(&state.messages.messages);
    let observed = llm_reported.unwrap_or(0).max(estimated);
    let will_trigger = observed >= compactor.trigger_tokens();
    let refill_skip_reminder: Option<String> = if !will_trigger || force_compact {
        None
    } else {
        let decision = {
            let mut guard = ctx.cfg.compact_refill.lock();
            guard.pre_check(
                step_no,
                zhuye_compact::RAPID_REFILL_WINDOW_STEPS,
                zhuye_compact::RAPID_REFILL_MAX_CONSECUTIVE,
            )
        };
        match decision {
            zhuye_compact::RefillDecision::Allow => None,
            zhuye_compact::RefillDecision::Skip
                if window.is_some_and(|w| u64::from(observed) * 100 >= u64::from(w) * 95) =>
            {
                state.deferred_error = Some(zhuye_protocol::ErrorEvent {
                    code: "compact_rapid_refill_window_full".into(),
                    message:
                        "上下文快速回填且接近模型窗口上限，无法安全继续；请缩小任务或开启新会话。"
                            .into(),
                    details: Some(
                        serde_json::json!({"model": model, "window": window, "observed": observed, "threshold": compactor.trigger_tokens()}),
                    ),
                });
                return None;
            }
            zhuye_compact::RefillDecision::Skip => Some(format!(
                "<system-reminder>\n上下文刚压缩过又在很短的几步内再次逼近阈值\
                 (rapid refill,连续第 {} 次)。为避免无限压缩循环,本次已跳过\
                 自动压缩。请立即收敛:减少产出规模、不要再次读取大文件、\
                 尽快完成任务并给出最终答复。\n</system-reminder>",
                {
                    let g = ctx.cfg.compact_refill.lock();
                    g.consecutive()
                }
            )),
            zhuye_compact::RefillDecision::Abort => {
                // 连续 refill 达上限:放弃压缩并收口当前 turn(与
                // model_call 的 GiveUp 同路径 —— deferred_error 由
                // submission_loop 统一转为 Error + TurnAborted)。
                let consecutive = ctx.cfg.compact_refill.lock().consecutive();
                tracing::warn!(
                    step = step_no,
                    consecutive,
                    "rapid refill 死循环熔断:放弃压缩并终止当前 turn"
                );
                state.deferred_error = Some(zhuye_protocol::ErrorEvent {
                    code: "compact_rapid_refill".into(),
                    message: format!(
                        "连续 {consecutive} 次压缩后立即再次触发压缩(rapid refill \
                         死循环),已停止当前 turn。建议开启新会话或把任务拆小。"
                    ),
                    details: Some(serde_json::json!({
                        "consecutive": consecutive,
                        "window_steps": zhuye_compact::RAPID_REFILL_WINDOW_STEPS,
                        "model_step": step_no,
                    })),
                });
                return None;
            }
        }
    };
    // v1.5 E1:PreCompact hook —— 压缩前最后关口。Deny 跳过本轮压缩
    // (本轮照常执行,阈值下一轮再议);InjectMessage 在压缩后追加
    // System 提醒。ctx 无 hook_engine 的测试路径跳过 dispatch。
    let (compact_denied, compact_injected) = {
        let hook_ctx = zhuye_hooks::HookContext {
            session_id: ctx.session_id,
            turn_id: ctx.turn_id,
            workspace: ctx.cfg.current_workspace(),
            permission_mode: ctx.cfg.permission_mode(),
            approval_policy: None,
        };
        let trigger = if force_compact { "manual" } else { "threshold" };
        let decision = ctx
            .hook_engine
            .dispatch(&zhuye_hooks::HookEvent::PreCompact {
                trigger: trigger.into(),
                ctx: hook_ctx,
            })
            .await;
        let resolved = decision.resolve();
        // injected 是本块 owned 值:直接 move 出 content,无需逐条克隆。
        (
            resolved.deny_reason.is_some(),
            resolved
                .injected
                .into_iter()
                .map(|m| m.content)
                .collect::<Vec<_>>(),
        )
    };

    // v1.x:第三返回值是记忆固化写入摘要(compact 期间 append 进 MEMORY.md
    // 的事实),供下方落盘 MemoryAccess 审计。
    // 消息列表按借用传入;压缩判定每迭代执行且绝大多数走 Noop ——
    // None 表示未压缩,保持原列表不回写,不为「借一下」深拷贝整个会话。
    let (compacted, evt, memory_write, below_trigger) =
        if compact_denied || refill_skip_reminder.is_some() {
            if refill_skip_reminder.is_some() {
                tracing::info!(
                    step = step_no,
                    "rapid refill:跳过本轮自动压缩(窗口内连续触发),已注入收敛提醒"
                );
            } else {
                tracing::info!("PreCompact hook denied; skipping compaction this round");
            }
            (
                None,
                zhuye_protocol::ContextCompactedEvent {
                    strategy: ContextCompactedStrategy::Noop,
                    removed_messages: 0,
                    before_tokens: 0,
                    after_tokens: 0,
                },
                None,
                false,
            )
        } else {
            match compactor
                .compact_outcome(
                    &state.messages.messages,
                    state.compaction_summary.as_deref(),
                    llm_reported,
                    compact_focus.as_deref(),
                )
                .await
            {
                zhuye_compact::CompactionOutcome::Noop { event, reason } => {
                    tracing::debug!(reason, "上下文压缩未发生");
                    (None, event, None, false)
                }
                zhuye_compact::CompactionOutcome::Applied {
                    messages,
                    event,
                    memory_write,
                    micro_changed,
                    pruned,
                    truncated,
                    below_trigger,
                } => {
                    tracing::info!(
                        micro_changed,
                        pruned,
                        truncated,
                        below_trigger,
                        "上下文压缩阶段结果"
                    );
                    (Some(messages), event, memory_write, below_trigger)
                }
            }
        };
    tracing::info!(
        model = %model,
        threshold = compactor.trigger_tokens(),
        local_estimate = estimated,
        reported_tokens = llm_reported,
        strategy = ?evt.strategy,
        applied = compacted.is_some(),
        below_trigger,
        "上下文压缩结果"
    );
    if !force_compact
        && !compact_denied
        && refill_skip_reminder.is_none()
        && compacted.is_none()
        && window.is_some_and(|w| observed >= w)
    {
        state.deferred_error = Some(zhuye_protocol::ErrorEvent {
            code: "compact_unavailable_window_full".into(),
            message: "上下文已达到模型窗口且没有可压缩的消息；请缩小输入或调整模型窗口配置。"
                .into(),
            details: Some(
                serde_json::json!({"model": model, "window": window, "observed": observed, "threshold": compactor.trigger_tokens()}),
            ),
        });
        return None;
    }
    // 从压缩后的消息列表里抽取 LLM 生成的摘要
    // (它是带 `<summary>...</summary>` 的 System 消息)。
    // iter().flatten() 对 None(未回写)短路,对无摘要的
    // Microcompact / SmartPrune 同样安全。
    let summary_text = if matches!(evt.strategy, ContextCompactedStrategy::LlMSummarize) {
        compacted.iter().flatten().find_map(|m| match m {
            ChatMessage::System(s) if s.contains("<summary>") => Some(s.clone()),
            _ => None,
        })
    } else {
        None
    };
    // 压缩器仅在消息确实改动时返回 Some；只截断但未删除消息也要计入。
    state.compact_triggered = compacted.is_some();
    if state.compact_triggered {
        // 只有校准水位真正低于阈值才给快速回填守卫上膛。
        // 仍超阈的有效裁剪照常回写、发事件和记快照，但不能挡住下一次更强压缩。
        // 手动压缩在进入压缩器前已经绕过预检。
        if below_trigger {
            ctx.cfg.compact_refill.lock().on_compacted(step_no);
        } else {
            tracing::info!(
                step = step_no,
                threshold = compactor.trigger_tokens(),
                "压缩后的校准水位仍不低于触发阈值，不登记快速回填锚点"
            );
        }
        // 在 `evt` 被移入 wire event 之前捕获我们需要的字段。
        let strategy = evt.strategy;
        let removed_count = evt.removed_messages;
        let before_tokens = evt.before_tokens;
        let after_tokens = evt.after_tokens;

        if summary_text.is_some() {
            state.compaction_summary = summary_text.clone();
        }

        let _ = ctx
            .event_tx
            .send(Event::new(
                ctx.sub_id.clone(),
                EventMsg::ContextCompacted(evt),
            ))
            .await;

        // v1.6:PostCompact hook —— 压缩完成后触发(与 PreCompact 配对)。
        // 决策仅作信息性记录(压缩已发生,不可回滚);典型用途:审计
        // 落盘、上下文恢复钩子(重读 active files 等)。与 PreCompact
        // 同构的 HookContext 便于外部命令式 hook 消费。
        {
            let hook_ctx = zhuye_hooks::HookContext {
                session_id: ctx.session_id,
                turn_id: ctx.turn_id,
                workspace: ctx.cfg.current_workspace(),
                permission_mode: ctx.cfg.permission_mode(),
                approval_policy: None,
            };
            let strategy_name = match strategy {
                ContextCompactedStrategy::Noop => "noop",
                ContextCompactedStrategy::Microcompact => "microcompact",
                ContextCompactedStrategy::SmartPrune => "smart_prune",
                ContextCompactedStrategy::LlMSummarize => "llm_summarize",
            };
            let _ = ctx
                .hook_engine
                .dispatch(&zhuye_hooks::HookEvent::PostCompact {
                    strategy: strategy_name.into(),
                    removed_messages: removed_count,
                    before_tokens,
                    after_tokens,
                    ctx: hook_ctx,
                })
                .await;
        }

        // M5:持久化 Compaction 记录,让 `resume` 能回放。
        //
        // 有实际改动即记审计；仅截断时删除数也可能为零。
        if let Some(rec) = ctx.recorder.as_ref() {
            let strategy_str = match strategy {
                ContextCompactedStrategy::Noop => "noop",
                ContextCompactedStrategy::Microcompact => "microcompact",
                ContextCompactedStrategy::SmartPrune => "smart_prune",
                ContextCompactedStrategy::LlMSummarize => "llm_summarize",
            };
            if let Err(error) = rec
                .record(RolloutRecord::Compaction {
                    turn_id: ctx.turn_id,
                    strategy: strategy_str.into(),
                    removed_count,
                    summary: summary_text.clone().unwrap_or_default(),
                    // v1.x:压缩前后 token 估算落盘(此前只在内存事件里,
                    // 会话统计无法回答「压缩省了多少」)。
                    before_tokens,
                    after_tokens,
                    context_snapshot: compacted.as_ref().and_then(|messages| {
                        match crate::resume::encode_context_snapshot(messages) {
                            Ok(snapshot) => Some(snapshot),
                            Err(error) => {
                                tracing::warn!(%error, "压缩快照编码失败，恢复时使用旧历史");
                                None
                            }
                        }
                    }),
                })
                .await
            {
                tracing::warn!(%error, "压缩快照记录失败，恢复时可能使用旧历史");
            }
        }

        // v1.x:记忆固化写入审计 —— compact 的 LLM 摘要成功后把提取的事实
        // append 进了 MEMORY.md(此前只有 tracing 日志,无持久化)。
        // 注意:缓存失效**不能**嵌在 recorder 分支内 —— `M4Config` 默认
        // `recorder: None`(lib 嵌入等无 rollout 的运行模式),嵌套会让
        // 固化写入后检索缓存仍然命中,新事实到不了 system prompt。
        if let Some(info) = &memory_write {
            // 固化改变了 MEMORY.md:失效本 turn 的检索缓存,让后续迭代
            // 重新检索到新写入的事实。
            state.memory_retrieval_cache = None;
            state.memory_read_recorded = false;
            if let Some(rec) = ctx.recorder.as_ref() {
                let _ = rec
                    .record(RolloutRecord::MemoryAccess {
                        turn_id: ctx.turn_id,
                        op: zhuye_protocol::MemoryOp::Write,
                        scope: info.scope.to_string(),
                        agent_type: info.agent.clone(),
                        chars: info.chars as u32,
                        entries: info.facts as u32,
                        at: chrono::Utc::now(),
                    })
                    .await;
            }
        }
    }
    // 仅真正压缩过才回写;Noop 迭代保持原消息列表(零拷贝路径)。
    if let Some(compacted) = compacted {
        state.messages.messages = compacted;
    }
    // v1.5 E1:PreCompact 的 InjectMessage 注入(压缩后可见,天然进入
    // 下一请求上下文)。
    // 修复原因(缓存审计):此前以**内联 System 消息**追加 —— 下一请求
    // fold_system_messages 会把它折叠进顶层 system 块,改写已发送的
    // 前缀字节,缓存全量失效。改为尾部 user system-reminder 追加
    // (append-only,不改写已发送前缀;与 steering / 收敛提醒同先例)。
    for content in compact_injected {
        state
            .messages
            .messages
            .push(ChatMessage::User(zhuye_llm::UserContent {
                blocks: vec![zhuye_llm::ContentBlock::Text {
                    text: format!("<system-reminder>\n{content}\n</system-reminder>"),
                }],
            }));
    }

    // v1.x:rapid-refill 跳过时注入收敛提醒。尾部 user 追加(append-only,
    // 不改写已发送前缀 —— 缓存友好),与 model_call 自动续写提醒同先例。
    if let Some(reminder) = refill_skip_reminder {
        state
            .messages
            .messages
            .push(ChatMessage::User(zhuye_llm::UserContent {
                blocks: vec![zhuye_llm::ContentBlock::Text { text: reminder }],
            }));
    }

    // v1.x(tool-defer 门控,默认关):MCP 工具目录公告。目录**不变不重发**
    // (指纹比对,append-only 尾部追加,缓存友好);变化(插件热挂载 /
    // rescan)时按新目录整份重发 —— 旧目录条目被新公告覆盖语义。
    // 落盘一致性:提醒进 state.messages,随 assistant 步骤的持久化节奏
    // 走(与 steering 注入同先例),resume 后由注入条件重推导。
    if ctx.cfg.tool_defer_enabled {
        let names = ctx.tools_queue.registry().deferred_tool_names();
        if !names.is_empty() {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            for n in &names {
                n.hash(&mut hasher);
            }
            let fingerprint = hasher.finish();
            let announced = ctx.cfg.deferred_dir_announced.lock();
            if announced.as_ref() != Some(&fingerprint) {
                let registry = ctx.tools_queue.registry();
                let mut listing = String::new();
                for name in &names {
                    let desc = registry
                        .get(name)
                        .map_or_else(String::new, |t| t.description().chars().take(100).collect());
                    listing.push_str(&format!("- {name}:{desc}\n"));
                }
                let announcement = format!(
                    "<system-reminder>\n以下 MCP 工具已启用按需加载(deferred),\
                     完整定义未随本次请求下发。目录:\n{listing}\n\
                     调用方式:使用 deferred_call 工具(name = 工具名,args = 参数对象)。\n\
                     需要确认某工具的参数细节时,先用 tool_search 查询该工具。\n\
                     </system-reminder>"
                );
                state
                    .messages
                    .messages
                    .push(ChatMessage::User(zhuye_llm::UserContent {
                        blocks: vec![zhuye_llm::ContentBlock::Text { text: announcement }],
                    }));
                drop(announced);
                *ctx.cfg.deferred_dir_announced.lock() = Some(fingerprint);
            }
        }
    }

    // 1.5 v1.4 A2:回合中途转向收割。ToolExec → PreLoop 回环的每次入口
    // 都执行 —— 用户对正在跑的回合「边跑边改需求」:Now 优先级消息作为
    // 纯 User 消息追加(语义上是用户中途说话);Attachment 作为包裹
    // `<system-reminder>` 的 User 消息追加(补充参考信息,不构成新指令)。
    // 收割放在 compact 之后:注入内容属于「最新指示」,不应被本轮压缩
    // 修剪。上限 `MAX_MID_TURN_STEERING` 防外部无限投喂;注入的消息同步
    // 落盘(recorder),否则 resume 时丢失这段中途指示。
    //
    // v1.9 C6:两级队列 —— turn 起始(`iteration == 0`,首个 model_call
    // 之前)只收 turn 级(Now/Attachment);step 回环(`iteration >= 1`)
    // 额外收 step 级(`SteeringPriority::Step`,只属于"当前这一步"的
    // 补充指令,turn 起始注入会错过语境)。
    if state.iteration == 0 {
        inject_turn_boundary_steering(state, ctx).await;
    } else {
        inject_step_boundary_steering(state, ctx).await;
    }

    // 3. 按 active_agent_def.memory 作用域加载内存。
    // v1.4 D2:检索化注入 —— 用本轮用户输入作查询,BM25 选相关记忆
    // 条目 + 最近条目(8000 字符预算不变);查询为空 / 记忆总量装得下
    // 时等价历史全量注入。旧格式(无 `## 标题`)整文件为单条目,兼容。
    let memory_query: String = ctx
        .messages
        .iter()
        .rev()
        .find_map(|m| match m {
            ChatMessage::User(u) => Some(
                u.blocks
                    .iter()
                    .filter_map(|b| match b {
                        zhuye_llm::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        })
        .unwrap_or_default();
    // 检索走 turn 级缓存:`AgentState` 每 turn 新建,turn 内查询固定,
    // 命中缓存即可省掉每次迭代的 MEMORY.md 读盘 + BM25 全量打分。
    // v1.11 M5a:改经 `MemoryStore::retrieve` trait 路由 —— 默认实现
    // 即旧 BM25 分节路径(自定义 store 行为不变),SQLite 后端覆写为
    // FTS5 中文检索;新旧模式在同一调用点切换。
    let memory_text = match &state.memory_retrieval_cache {
        Some(cached) => cached.clone(),
        None => {
            let text = match m4.memory.retrieve(
                &m4.active_agent_def.memory,
                &m4.active_agent_def.name,
                memory_query.trim(),
                zhuye_memory::MAX_MEMORY_INJECT_CHARS,
            ) {
                Ok(s) => zhuye_memory::truncate_for_injection(&s),
                Err(e) => {
                    tracing::warn!(?e, "failed to load memory; using empty");
                    String::new()
                }
            };
            state.memory_retrieval_cache = Some(text.clone());
            text
        }
    };
    // v1.x:记忆检索注入审计 —— 每轮迭代把相关记忆拼进 system prompt,
    // 此前无任何持久化。空记忆不记(避免无 memory 配置的会话刷噪声);
    // 每 turn 只落盘一条(与检索缓存配套,迭代不重复写)。
    if !memory_text.is_empty()
        && !state.memory_read_recorded
        && let Some(rec) = ctx.recorder.as_ref()
    {
        let scopes = m4
            .active_agent_def
            .memory
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let _ = rec
            .record(RolloutRecord::MemoryAccess {
                turn_id: ctx.turn_id,
                op: zhuye_protocol::MemoryOp::Read,
                scope: scopes,
                agent_type: m4.active_agent_def.name.clone(),
                chars: memory_text.chars().count() as u32,
                entries: 0,
                at: chrono::Utc::now(),
            })
            .await;
        state.memory_read_recorded = true;
    }

    // 4. 构建分层 system prompt。
    // v1.x Plan mode:active-mode 状态告知。在 `compose_core` 之后追加
    // `## Active Mode` 段(仅在 `PermissionMode::Plan` 下),让 LLM 知道
    // 自己正处于 Plan mode,而不是只在 prompt 里记得「Plan mode 是什么」。
    // 这是修复「plan mode 下调研后不出 plan」的关键 —— 没这段时 LLM
    // 只知道 Plan mode 规则,不知道自己当前是不是已经在 Plan mode 里。
    //
    // v1.6:按模型匹配的主 prompt。当前 turn 的 model spec 在节点入口
    // 即可拿到(`ctx.model` 热切换在此刻生效);agent 定义来源为
    // `BuiltinDefault` 时用提示词库解析 `system.main`,用户自定义
    // (`.zhuye/agents/*.md`)则原文不动。
    let model_spec = ctx.model.read().clone();
    let prompt_set = ctx.cfg.resolve_prompt(&model_spec).set;
    let current_mode = ctx.cfg.permission_mode();
    let prompt_context = crate::graph::state::PromptCoreContext {
        custom_main: matches!(
            m4.active_agent_def.source,
            zhuye_agent_def::SystemPromptSource::Custom
        )
        .then(|| m4.active_agent_def.system_prompt.clone()),
        memory: memory_text,
        checklist: ctx.todos_slot.read().clone(),
        mode: current_mode,
    };
    let core = compose_prompt_core(&prompt_context, &prompt_set);
    state.prompt_core = Some(prompt_context);
    let ephemeral_skills = m4.skills.render_for_system_prompt();
    let reminder = format!("Iteration {}/{}", state.iteration, ctx.max_iterations);
    let mut effective = m4.skills.active_tool_names();
    // v1.x:外部工具(MCP / LSP / plugin / serve 远程)注册即对 LLM 可见 ——
    // 用户显式接入的工具不应被 skills catalog 的 always_on 白名单挡住
    // (GUI 的 MCP/LSP 全部走 Runtime/Mcp 源;CLI 的 MCP 走 Mcp 源,同受益)。
    // 只并集不删减:Builtin 集仍由 catalog 策略(curated)管理。
    effective.extend(ctx.tools_queue.registry().external_tool_names());
    // v1.x:入口型 builtin 工具恒可见。修复原因:tool_search(搜索工具的
    // 入口)与 deferred_call(tool-defer 门控插件的调用入口)都不在 skills
    // catalog 的 always_on curated 表 —— 模型永远看不到「找工具 / 调按需
    // 加载工具」的入口,deferred 工具目录公告形同虚设。未注册时
    // `list_specs` 的 contains 过滤自然消失,恒并无副作用。
    for entry_tool in ["tool_search", "deferred_call"] {
        effective.insert(entry_tool.to_string());
    }

    // v1.x Plan mode：从 LLM 可见工具集中移除通用 `write` / `edit`，强制 LLM
    // 使用 `PlanWrite`（`required_permission = Auto`，审批层跳过）。否则 LLM
    // 在「用 write」与「用 PlanWrite」的矛盾 prompt 下倾向选 write，而 write 的
    // `required_permission = Prompt` 会让审批层独立弹窗 —— 即使 `PlanModeGate`
    // hook 放行了 `.zhuye/plan/` 路径也救不了，因为审批层只看
    // `required_permission`、不看 hook 决策。Plan mode 是只读规划阶段，本就
    // 不应允许 write/edit 任意路径；`PlanWrite` 内置 `.zhuye/plan/` 路径
    // 强制，语义更窄、更安全。`PlanModeGate` hook 的 `.zhuye/plan/` 特例
    // 保留作防御性兜底（万一未来有 skill 把 write 显式加回 always_on）。
    //
    // v1.x 询问档语义升级：Prompt 模式同为只读探索档（`AskModeGate` 判定：
    // 写类工具直接拒绝），同样从可见工具集移除 write/edit —— 模型不再
    // 浪费轮次尝试注定被拒的调用。
    if matches!(current_mode, PermissionMode::Plan | PermissionMode::Prompt) {
        effective.remove("write");
        effective.remove("edit");
    }

    // v1.x 功能 6:AgentDefinition 工具硬性过滤(配置了才生效,否则全工具)。
    // 在 skills 过滤之后叠加,语义:白名单 ∩、黑名单 −、readonly 排除变更类。
    // `tools` / `disallowed_tools` / `readonly` 全空 → 不过滤(向后兼容)。
    let def = &m4.active_agent_def;
    if !def.tools.is_empty() {
        effective.retain(|t| def.tools.iter().any(|x| x == t));
    }
    if !def.disallowed_tools.is_empty() {
        effective.retain(|t| !def.disallowed_tools.iter().any(|x| x == t));
    }
    if def.readonly {
        effective.retain(|t| !zhuye_tools::READONLY_DENYLIST.contains(&t.as_str()));
    }
    let tool_specs: Vec<zhuye_llm::ToolSpec> = ctx
        .tools_queue
        .registry()
        .list_specs()
        .into_iter()
        .filter_map(|s| match s {
            zhuye_tools::ToolSpec::Function {
                name,
                description,
                parameters,
                ..
            } => {
                if effective.contains(name.as_str()) {
                    Some(zhuye_llm::ToolSpec::Function {
                        name,
                        description,
                        parameters,
                    })
                } else {
                    None
                }
            }
        })
        .collect();
    // v1.6:模型层工具描述覆盖 —— `tool.<name>` section 存在时替换
    // description(落点在 spec 组装处,Tool trait 与外部工具不受影响)。
    let mut ephemeral = zhuye_prompt::LayeredPrompt::compose_ephemeral_with_mode(
        &tool_specs,
        &ephemeral_skills,
        &reminder,
        current_mode,
    );
    // v1.x Plan mode:previous-turn hint。`take_last_abort_reason()` 一次性
    // 消费最近一次中断原因(用户 Esc / 自然 turn 失败);若消费到
    // `UserInterrupt` 且当前 mode 是 Plan,则在 ephemeral block 里追加
    // `## Previous Turn` 段,提示 LLM 「上轮被中断,在 Plan mode 下应该
    // 收尾并调 ExitPlanMode」。take-once 保证只对紧邻的下一个 turn 注
    // 入一次,不会持续唠叨。
    if let Some(AbortReason::UserInterrupt) = ctx.cfg.take_last_abort_reason()
        && matches!(current_mode, PermissionMode::Plan)
    {
        // 插在 `</system-reminder>` 闭合标签之前 —— `compose_ephemeral`
        // 末尾固定是 `</system-reminder>` 字符串(见 builder.rs:201)。
        if let Some(close_tag_pos) = ephemeral.find("</system-reminder>") {
            let mut new_ephemeral = String::with_capacity(ephemeral.len() + 128);
            new_ephemeral.push_str(&ephemeral[..close_tag_pos]);
            new_ephemeral.push_str(
                "\n## Previous Turn\n\
                 Your previous turn was interrupted by the user (Esc/Stop).\n\
                 In Plan mode, this usually means the user wants you to wrap up:\n\
                 call `ExitPlanMode` with your findings as a Markdown plan.\n",
            );
            new_ephemeral.push_str(&ephemeral[close_tag_pos..]);
            ephemeral = new_ephemeral;
        }
    }

    // v1.x Plan mode: 仅注入 plan 文件目录这一运行时信息。PlanWrite → ExitPlanMode
    // 的收尾指引已由 `compose_ephemeral_with_mode` 在 plan mode 下写入「## Important」
    // 段,这里不再重复,避免两处措辞漂移。
    // `PlanWrite` 在 Plan mode 白名单内且 `required_permission = Auto`,
    // 故写盘免审批(此前用通用 `write` 每次都弹「⚠ Approved: write」)。
    if matches!(current_mode, PermissionMode::Plan) {
        let plan_dir = ctx.cfg.current_workspace().join(".zhuye/plan");
        if let Some(close_tag_pos) = ephemeral.find("</system-reminder>") {
            let mut new_ephemeral = String::with_capacity(ephemeral.len() + 128);
            new_ephemeral.push_str(&ephemeral[..close_tag_pos]);
            new_ephemeral.push_str(&format!(
                "\n## Plan Mode Active\n\
                 Your plan file directory: {}\n\
                 Pass a `.md` filename as `path` to `PlanWrite`; it lands in this directory automatically.\n",
                plan_dir.display(),
            ));
            new_ephemeral.push_str(&ephemeral[close_tag_pos..]);
            ephemeral = new_ephemeral;
        }
    }

    // 把 core 前置为 system block(比 System message 更清爽
    // —— Anthropic 等 provider 偏好 `system` 数组以支持缓存)。
    // v1.x fix: ephemeral(工具目录 + 答案格式 + 迭代计数)也作为 system block
    // 追加,而非 trailing User 消息。此前作为 User 消息注入时,MiniMax-M3 等
    // 模型在多轮工具调用后会把这条 trailing system-reminder 误认为「最新的
    // 用户输入」,从而遗忘原始问题、回答 "No question provided"。放进 system
    // 数组后,它属于指令上下文,不会与用户问题混淆。
    let mut blocks = vec![zhuye_llm::SystemBlock {
        text: core,
        cache_control: None,
        ephemeral: false,
    }];
    if !ephemeral.trim().is_empty() {
        blocks.push(zhuye_llm::SystemBlock {
            text: ephemeral,
            cache_control: None,
            ephemeral: true,
        });
    }
    state.system_blocks = zhuye_llm::SystemBlocks(blocks);

    // Ephemeral reminder 不再作为 User 消息注入(见上注释),留空以跳过
    // model_call 里的 trailing User 追加。
    state.ephemeral_text = String::new();

    // 5. 保存 effective tools 供 model_call 使用。
    state.effective_tools = tool_specs;

    // 6. v1.1.0 Phase 6 P0:收集上下文恢复元消息。
    //
    // 每轮从 `M4Deps` 的共享源重新计算(Notes 走 JSONL 落盘,
    // SubagentRegistry / FileRecovery 走 in-memory Arc),所以本字段
    // 不需跨 turn 持久化,`AgentState::default()` 已经给空 Vec。
    // 渲染在 `model_call` 完成,见下文 ephemeral push 之后的循环。
    //
    // Phase A 启用:SessionMemory。
    // Phase B 启用:ActiveFiles(post-compact)。
    // Phase C 启用:SubagentRegistry(每轮)。
    if let Some(text) = m4.note_store.as_meta_message() {
        state
            .recovery_meta
            .push(RecoveryEntry::new(MetaKind::SessionMemory, text));
    }
    // 6a. Active File Recovery:仅在 compact 触发后注入,文件读取静默跳过失败项。
    // Review 2026-06-29 BUG-5: 使用 `recover_with_deleted` 把 NotFound
    // 收集到 deleted, 在 meta 顶部提示 LLM。
    if state.compact_triggered {
        let (recovered, deleted) = m4
            .file_recovery
            .recover_with_deleted(&state.messages.messages);
        if !recovered.is_empty() || !deleted.is_empty() {
            let content = m4
                .file_recovery
                .to_meta_message_with_deleted(&recovered, &deleted);
            state
                .recovery_meta
                .push(RecoveryEntry::new(MetaKind::ActiveFiles, content));
        }
    }
    // 6b. Subagent Registry:每轮渲染(防止 LLM 重复 spawn)。
    if let Some(text) = m4.subagent_registry.as_meta_message() {
        state
            .recovery_meta
            .push(RecoveryEntry::new(MetaKind::SubagentRegistry, text));
    }

    Some(GraphNode::ModelCall)
}

/// v1.4 A2:单 turn 允许注入的回合中途转向消息总数上限。
const MAX_MID_TURN_STEERING: u32 = 16;

/// v1.9 C6:turn 边界收割(turn 起始,`iteration == 0`)。
///
/// - `Now` 优先级:纯 User 消息(provider 视角 = 用户中途说话,权重最高);
/// - `Attachment` 优先级:`<system-reminder>` 包裹的 User 消息(补充参考,
///   不构成新指令);
/// - **不**收 `Step` 级消息 —— step 指令在 turn 起始注入会错过语境,
///   留在队列等本 turn 的 ToolExec 回环。
/// - 注入消息同步写 recorder(`MessageRole::User`),resume 不丢中途指示。
/// - 超过 `MAX_MID_TURN_STEERING` 的消息丢弃并 warn(防投喂撑爆上下文)。
///
/// step 回环入口见 [`inject_step_boundary_steering`]。
async fn inject_turn_boundary_steering(state: &mut AgentState, ctx: &NodeContext) {
    let Some(sq) = ctx.steering_queue.as_ref() else {
        return;
    };
    let drained = sq.lock().drain_all();
    inject_steering_messages(state, ctx, drained).await;
}

/// v1.9 C6:step 边界收割(ToolExec 回环)。step 级优先(只属于当前
/// 这一步的指令),再普通 turn 级。
async fn inject_step_boundary_steering(state: &mut AgentState, ctx: &NodeContext) {
    let Some(sq) = ctx.steering_queue.as_ref() else {
        return;
    };
    let drained = sq.lock().drain_for_step();
    inject_steering_messages(state, ctx, drained).await;
}

/// 注入共通实现(消息转换 / 上限 / 落盘)。
async fn inject_steering_messages(
    state: &mut AgentState,
    ctx: &NodeContext,
    drained: Vec<crate::steering_queue::SteeringMessage>,
) {
    use crate::steering_queue::SteeringPriority;
    if drained.is_empty() {
        return;
    }
    // protocol 副本(落盘用)与 llm 副本(注入用)同步构建;仅 Text/Image
    // 参与 —— 与 submission_loop 的 user 输入转换域一致。
    let mut protocol_blocks: Vec<zhuye_protocol::ContentBlock> = Vec::new();
    for msg in drained {
        let remaining = MAX_MID_TURN_STEERING.saturating_sub(state.mid_turn_steering_injected);
        if remaining == 0 {
            tracing::warn!(
                injected = state.mid_turn_steering_injected,
                "mid-turn steering 上限已满,丢弃后续消息"
            );
            break;
        }
        state.mid_turn_steering_injected += 1;
        for item in msg.items {
            let block = match item {
                zhuye_protocol::UserInputItem::Text { text } => {
                    if matches!(msg.priority, SteeringPriority::Now) {
                        zhuye_protocol::ContentBlock::Text { text }
                    } else {
                        // Attachment:包 system-reminder,与项目内其他 meta
                        // 注入的惯例一致 —— 模型可见但不冒充直接指令。
                        zhuye_protocol::ContentBlock::Text {
                            text: format!("<system-reminder>{text}</system-reminder>"),
                        }
                    }
                }
                zhuye_protocol::UserInputItem::Image { data, mime_type } => {
                    zhuye_protocol::ContentBlock::Image { data, mime_type }
                }
                // 其余 item 类型(LocalImage / Skill / QuestionAnswer)本就
                // 未在消息转换中接通,这里同样跳过,保持输入域一致。
                _ => continue,
            };
            protocol_blocks.push(block);
        }
    }
    if protocol_blocks.is_empty() {
        return;
    }
    let llm_blocks: Vec<zhuye_llm::ContentBlock> = protocol_blocks
        .iter()
        .map(|b| match b {
            zhuye_protocol::ContentBlock::Text { text } => {
                zhuye_llm::ContentBlock::Text { text: text.clone() }
            }
            zhuye_protocol::ContentBlock::Image { data, mime_type } => {
                zhuye_llm::ContentBlock::Image {
                    data: data.clone(),
                    mime_type: mime_type.clone(),
                }
            }
            _ => unreachable!("protocol_blocks 只含 Text/Image"),
        })
        .collect();
    state
        .messages
        .messages
        .push(ChatMessage::User(zhuye_llm::UserContent {
            blocks: llm_blocks,
        }));
    // 落盘:与 submission_loop 的 user 输入持久化同格式,resume 忠实回放
    // 中途指示(否则恢复后的会话丢掉这段「用户插话」)。
    if let Some(rec) = ctx.recorder.as_ref() {
        let _ = rec
            .record(RolloutRecord::message(
                ctx.turn_id,
                zhuye_protocol::MessageRole::User,
                serde_json::to_value(&protocol_blocks).unwrap_or(serde_json::Value::Null),
            ))
            .await;
    }
}

#[cfg(test)]
mod calibration_tests {
    use super::*;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    use zhuye_compact::{Compactor, CompactorConfig, Summarizer, SummarizerError};
    use zhuye_llm::{ChatMessage, ContentBlock, ToolResult, UserContent};

    struct FailingSummarizer;
    #[async_trait::async_trait]
    impl Summarizer for FailingSummarizer {
        async fn summarize_full(&self, _: &[ChatMessage]) -> Result<String, SummarizerError> {
            Err(SummarizerError::Cancelled)
        }
        async fn summarize_recent(
            &self,
            _: &[ChatMessage],
            _: Option<&str>,
        ) -> Result<String, SummarizerError> {
            Err(SummarizerError::Cancelled)
        }
    }

    fn test_context(trigger: u32, reported: u32) -> (AgentState, NodeContext) {
        let compactor = Arc::new(Compactor::new(
            CompactorConfig {
                trigger_tokens: trigger,
                keep_recent_microcompact: 1,
                keep_recent_smart_prune: 1,
                summarize_after: false,
                ..Default::default()
            },
            Arc::new(FailingSummarizer),
        ));
        let mut state = AgentState {
            history_seeded: true,
            last_llm_input_tokens: Some(reported),
            ..Default::default()
        };
        state.messages.messages = vec![
            ChatMessage::System("s".repeat(500)),
            ChatMessage::Tool(ToolResult {
                call_id: "tool:bash:1".into(),
                content: vec![ContentBlock::text("x".repeat(8_000))],
                is_error: false,
            }),
            ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::text("继续")],
            }),
        ];
        let (event_tx, _rx) = tokio::sync::mpsc::channel(8);
        let engine = Arc::new(zhuye_hooks::HookEngine::new());
        let ctx = NodeContext {
            session_seq: None,
            turn_id: zhuye_protocol::TurnId::new(),
            session_id: zhuye_protocol::ThreadId::new(),
            model: Arc::new(parking_lot::RwLock::new("stub/model".into())),
            policy: Arc::new(zhuye_llm::RoutingPolicy::default()),
            registry: Arc::new(zhuye_llm::ModelRegistry::new()),
            hook_engine: Arc::clone(&engine),
            tools_queue: Arc::new(zhuye_tools::ToolExecutionQueue::with_defaults(
                Arc::new(zhuye_tools::ToolRegistry::new()),
                engine,
                zhuye_tools::ToolContext::for_workspace("."),
            )),
            sub_id: "sub".into(),
            cancel: CancellationToken::new(),
            event_tx,
            messages: vec![],
            max_iterations: 4,
            m4: Some(crate::config::M4Deps {
                compactor,
                memory: Arc::new(zhuye_memory::InMemoryStore::new()),
                skills: Arc::new(zhuye_skills::SkillsCatalog::new()),
                prompt_builder: Arc::new(parking_lot::Mutex::new(
                    zhuye_prompt::PromptBuilder::new(),
                )),
                active_agent_def: Arc::new(zhuye_agent_def::AgentDefinition::default()),
                recorder: None,
                note_store: Arc::new(zhuye_notes::InMemoryNoteStore::new()),
                file_recovery: Arc::new(zhuye_recovery::ActiveFileRecovery::new(Arc::from(
                    std::path::PathBuf::from("."),
                ))),
                subagent_registry: zhuye_recovery::SubagentRegistry::shared(),
                prompt_dirs: Vec::new(),
                skill_dirs: Vec::new(),
                bundled_skills: false,
            }),
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
            pending_subagents: None,
            sandbox_override: None,
            cfg: crate::config::AgentConfig::new("stub/model", "."),
        };
        (state, ctx)
    }

    #[tokio::test]
    async fn still_above_calibrated_level_does_not_arm_refill_guard() {
        let (mut state, ctx) = test_context(1_000, 8_000);
        let next = pre_loop(&mut state, &ctx).await;
        assert_eq!(next, Some(GraphNode::ModelCall));
        assert!(state.compact_triggered, "有效裁剪仍要回写");
        let anchor = ctx.cfg.compact_refill.lock();
        assert_eq!(anchor.consecutive(), 0);
        drop(anchor);
        // 直接检查锚点：未压到阈值下时 last_compact_step 必须保持空。
        let decision = {
            let mut guard = ctx.cfg.compact_refill.lock();
            guard.pre_check(100, 3, 3)
        };
        assert_eq!(decision, zhuye_compact::RefillDecision::Allow);
    }

    #[tokio::test]
    async fn below_calibrated_level_still_arms_refill_guard() {
        let (mut state, ctx) = test_context(4_000, 5_000);
        let next = pre_loop(&mut state, &ctx).await;
        assert_eq!(next, Some(GraphNode::ModelCall));
        assert!(state.compact_triggered);
        let decision = {
            let mut guard = ctx.cfg.compact_refill.lock();
            guard.pre_check(1, 3, 3)
        };
        assert_eq!(decision, zhuye_compact::RefillDecision::Skip);
    }

    #[tokio::test]
    async fn manual_compact_bypasses_refill_precheck() {
        let (_state, ctx) = test_context(4_000, 5_000);
        ctx.cfg.compact_refill.lock().on_compacted(0);
        ctx.cfg
            .model_step_counter
            .store(1, std::sync::atomic::Ordering::Relaxed);
        *ctx.force_compact_next.write() = true;
        let mut state = AgentState {
            history_seeded: true,
            last_llm_input_tokens: Some(12_000),
            ..Default::default()
        };
        state.messages.messages = vec![ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::text("手动压缩")],
        })];
        let blocked = {
            let mut guard = ctx.cfg.compact_refill.lock();
            guard.pre_check(1, 3, 3)
        };
        assert_eq!(blocked, zhuye_compact::RefillDecision::Skip);
        // 上面的预检只证明窗口已武装。真正的手动路径必须绕过它。
        ctx.cfg.compact_refill.lock().on_compacted(0);
        let next = pre_loop(&mut state, &ctx).await;
        assert_eq!(next, Some(GraphNode::ModelCall));
        assert!(state.deferred_error.is_none(), "手动压缩不得被熔断错误拦住");
        assert!(!*ctx.force_compact_next.read());
    }
}
