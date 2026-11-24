//! resume 历史 —— `RolloutRecord` → `ChatMessage` 的纯映射。
//!
//! 从 `zhuye-exec::bootstrap_resume` 抽取(exec 是 CLI 入口,GUI 等其它
//! 宿主需要同一映射却不应依赖 exec crate)。无 I/O:调用方自行 replay
//! JSONL 后把记录切片传进来。
//!
//! 语义(与原 bootstrap_resume 逐字一致,单一事实源在此):
//! - User 消息:支持 legacy `Value::String` 与 `Vec<ContentBlock>` 两种
//!   content 形态;仅 Text/Image 参与,user 侧的 ToolUse/ToolResult 忽略。
//! - Assistant 消息:Text → `AssistantContent.text`;ToolUse →
//!   `tool_calls`;ToolResult → 独立 `ChatMessage::Tool` 且**紧跟**
//!   Assistant 之后(provider 要求 assistant/tool 角色交替)。工具对
//!   忠实重建,不丢弃。
//! - `Compaction`:有快照时恢复压缩后的消息；旧记录没有快照时保留
//!   全量历史，并将最近一次非空摘要前置为合成的 `System` 消息。
//! - 其余记录(SessionMeta/Fork/Checkpoint/Rewind/TokenCount/Plan*/
//!   PermissionModeChanged/DiscussionTranscript)是审计轨迹,不注入对话。

use zhuye_llm::ChatMessage;
use zhuye_protocol::RolloutRecord;

/// 系统消息是字符串新类型，需单独编码以避免内部标签枚举的 serde 限制。
pub(crate) fn encode_context_snapshot(
    messages: &[ChatMessage],
) -> Result<serde_json::Value, serde_json::Error> {
    let items = messages
        .iter()
        .map(|message| match message {
            ChatMessage::System(text) => Ok(serde_json::json!({"role": "system", "text": text})),
            other => serde_json::to_value(other),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::Value::Array(items))
}

fn decode_context_snapshot(value: &serde_json::Value) -> Option<Vec<ChatMessage>> {
    value
        .as_array()?
        .iter()
        .map(|item| {
            if item.get("role").and_then(|v| v.as_str()) == Some("system") {
                Some(ChatMessage::System(item.get("text")?.as_str()?.to_owned()))
            } else {
                serde_json::from_value(item.clone()).ok()
            }
        })
        .collect()
}

/// 把 rollout 记录切片映射为适用于 `AgentThread` 的历史消息。
///
/// 空 / 全审计记录的输入 → 空输出。纯函数,可重入;`&[..]` 借用让调用方
/// 保留记录所有权(GUI 侧同一份 records 既喂本函数又序列化给前端水合)。
pub fn records_to_preload(records: &[RolloutRecord]) -> Vec<ChatMessage> {
    let mut messages: Vec<ChatMessage> = Vec::new();
    let mut last_summary: Option<String> = None;
    // 增量落盘让一次回合有多条 assistant 记录。先按 turn 拼回一条,
    // 模型看到的块顺序才与「回合结束整体落盘」的旧格式一致。
    let records = RolloutRecord::coalesce_turn_messages(records);

    for r in &records {
        match r {
            RolloutRecord::Message {
                turn_id: _,
                role,
                content,
            } => {
                match role {
                    // ── User 消息 ───────────────────────────────────────────
                    // v1.2 P2:支持两种 content 形态。
                    //   Value::String —— 旧格式(裸文本,向后兼容)。
                    //   Value::Array  —— 新格式(protocol ContentBlock 数组,
                    //                    含 Text/Image,由 submission_loop 落盘)。
                    zhuye_protocol::MessageRole::User => match content {
                        serde_json::Value::String(s) => {
                            messages.push(ChatMessage::User(zhuye_llm::UserContent {
                                blocks: vec![zhuye_llm::ContentBlock::Text { text: s.clone() }],
                            }));
                        }
                        arr @ serde_json::Value::Array(_) => {
                            if let Ok(blocks) =
                                serde_json::from_value::<Vec<zhuye_protocol::ContentBlock>>(arr.clone())
                            {
                                // protocol → llm 层映射(仅 Text/Image 两变体,
                                // 与 user_input_items_to_messages 的输入域一致)。
                                let llm_blocks: Vec<_> = blocks
                                    .into_iter()
                                    .filter_map(|b| match b {
                                        zhuye_protocol::ContentBlock::Text { text } => {
                                            Some(zhuye_llm::ContentBlock::Text { text })
                                        }
                                        zhuye_protocol::ContentBlock::Image { data, mime_type } => {
                                            Some(zhuye_llm::ContentBlock::Image { data, mime_type })
                                        }
                                        // user 消息不含 ToolUse/ToolResult/Diff,
                                        // 出现则忽略(resume 不应注入结构化工具块)。
                                        _ => None,
                                    })
                                    .collect();
                                if !llm_blocks.is_empty() {
                                    messages.push(ChatMessage::User(zhuye_llm::UserContent {
                                        blocks: llm_blocks,
                                    }));
                                }
                            }
                        }
                        _ => {}
                    },
                    // ── Assistant 消息 ──────────────────────────────────────
                    // v1.2 P2:支持两种 content 形态。
                    //   Value::String —— 旧格式(最后一条纯文本,向后兼容)。
                    //   Value::Array  —— 新格式(protocol ContentBlock 数组,
                    //                    Text + ToolUse + ToolResult,完整无损)。
                    //
                    // 新格式按块顺序拆分:工具调用组成助手消息,回执独立输出,
                    // 回执后的文本和下一轮工具调用留在后续助手消息中。
                    zhuye_protocol::MessageRole::Assistant => match content {
                        serde_json::Value::String(s) => {
                            messages.push(ChatMessage::Assistant(
                                zhuye_llm::AssistantContent {
                                    text: Some(s.clone()),
                                    ..Default::default()
                                },
                            ));
                        }
                        arr @ serde_json::Value::Array(_) => {
                            if let Ok(blocks) =
                                serde_json::from_value::<Vec<zhuye_protocol::ContentBlock>>(arr.clone())
                            {
                                fn flush_assistant(
                                    messages: &mut Vec<ChatMessage>,
                                    text_parts: &mut Vec<String>,
                                    tool_calls: &mut Vec<zhuye_llm::ToolCallRequest>,
                                ) {
                                    if text_parts.is_empty() && tool_calls.is_empty() {
                                        return;
                                    }
                                    messages.push(ChatMessage::Assistant(
                                        zhuye_llm::AssistantContent {
                                            text: if text_parts.is_empty() {
                                                None
                                            } else {
                                                Some(std::mem::take(text_parts).join("\n"))
                                            },
                                            tool_calls: std::mem::take(tool_calls),
                                            thinking: None,
                                        },
                                    ));
                                }

                                let mut text_parts = Vec::new();
                                let mut tool_calls = Vec::new();
                                for block in blocks {
                                    match block {
                                        zhuye_protocol::ContentBlock::Text { text } if !text.is_empty() => {
                                            text_parts.push(text);
                                        }
                                        zhuye_protocol::ContentBlock::ToolUse { id, name, args } => {
                                            tool_calls.push(zhuye_llm::ToolCallRequest { id, name, arguments: args });
                                        }
                                        zhuye_protocol::ContentBlock::ToolResult { call_id, output } => {
                                            // 工具结果前先完成调用消息;后续文本留在结果之后。
                                            flush_assistant(&mut messages, &mut text_parts, &mut tool_calls);
                                            messages.push(ChatMessage::Tool(
                                                zhuye_llm::ToolResult::from_output(call_id, &output),
                                            ));
                                        }
                                        // 不向助手历史注入图像或差异块。
                                        _ => {}
                                    }
                                }
                                flush_assistant(&mut messages, &mut text_parts, &mut tool_calls);
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            RolloutRecord::Compaction { summary, context_snapshot, .. } => {
                if let Some(snapshot) = context_snapshot.as_ref().and_then(decode_context_snapshot) {
                    messages = snapshot;
                    last_summary = None;
                } else if !summary.is_empty() {
                    last_summary = Some(summary.clone());
                }
            }
            RolloutRecord::SessionMeta { .. }
            | RolloutRecord::Fork { .. }
            | RolloutRecord::DiscussionTranscript { .. }
            // v1.2 P0-3:checkpoint / rewind 是工作区 marker,resume 历史
            // 时丢弃(只回放对话消息)。
            | RolloutRecord::Checkpoint { .. }
            | RolloutRecord::Rewind { .. }
            // v1.x:TokenCount 是 per-turn 统计快照(累计展示交给 CLI ls/show),
            // resume 历史时丢弃 —— 对话语义不受 token 计数影响。
            | RolloutRecord::TokenCount { .. }
            // v1.x Plan mode:PlanRequest / PlanReady / PlanRejected /
            // PermissionModeChanged 是 plan 生命周期与权限状态轨迹的审计
            // 记录。resume 回放时只重建 LLM 对话消息,plan 上下文不注入
            // (避免把已废弃的 plan_id / 旧权限态污染新会话)。
            | RolloutRecord::PlanRequest { .. }
            | RolloutRecord::PlanReady { .. }
            | RolloutRecord::PlanRejected { .. }
            | RolloutRecord::PermissionModeChanged { .. }
            | RolloutRecord::PromptSelectionChanged { .. }
            // v1.x:MemoryAccess 是记忆读写审计轨迹,不注入对话(与
            // TokenCount 同类 —— 记忆内容本身已随 system prompt 走
            // telemetry 层,回放不重复)。
            | RolloutRecord::MemoryAccess { .. }
            // v1.x:流锚点恢复丢弃的尾部留痕 —— 纯审计,resume **绝不**
            // 回放(丢弃内容永不回到模型上下文)。
            | RolloutRecord::StreamRecoveryDiscarded { .. }
            // v1.x:Prompt Snapshot 由 bootstrap 侧单独提取(见
            // `last_prompt_snapshot`),消息映射忽略。
            | RolloutRecord::PromptSnapshot { .. }
            // v1.x:输入守恒 ledger —— 由 bootstrap 侧对账提取
            // (`unsettled_input_turns`),消息映射忽略。
            | RolloutRecord::InputDisposition { .. }
            // v1.12:人机交互审计。**有意不重发**未决交互:进程重启后原
            // 回合(及其 waiter)已不存在,重发只会造出「无人接听的幽灵
            // 弹窗」—— 用户应答会打到空 waiter 上(submission_loop 只能
            // debug 日志丢弃)。活进程内的未决交互由
            // `AgentThread::pending_interactions()` 查询;跨进程应答属
            // roadmap(需要回合复活语义)。
            | RolloutRecord::InteractionRequest { .. }
            | RolloutRecord::InteractionResolution { .. } => {}
        }
    }

    // 加一条合成的 System 消息,内容为最近一次 compaction 的摘要,
    // 让恢复后的 thread 拥有最新上下文。
    if let Some(s) = last_summary {
        messages.insert(0, ChatMessage::System(s));
    }
    repair_orphan_tool_calls(messages)
}

/// 修复中断或导入产生的无效工具历史。
///
/// 只在对应助手调用后的连续工具消息中匹配回执;缺失回执在下一条
/// 非工具消息前合成错误结果,重复、孤儿和错位回执被丢弃。
/// 调用 ID 或工具名为空及同一批次的重复 ID 也会被过滤。
pub fn repair_orphan_tool_calls(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    use std::collections::HashSet;

    fn flush_pending(pending: &mut Vec<String>, repaired: &mut Vec<ChatMessage>) {
        for call_id in pending.drain(..) {
            repaired.push(ChatMessage::Tool(zhuye_llm::ToolResult {
                call_id,
                content: vec![zhuye_llm::ContentBlock::Text {
                    text: "[interrupted] 工具执行被中断,结果丢失。".to_string(),
                }],
                is_error: true,
            }));
        }
    }

    let mut repaired = Vec::with_capacity(messages.len());
    let mut pending = Vec::<String>::new();
    let mut changed = false;
    for msg in messages {
        match msg {
            ChatMessage::Assistant(mut content) => {
                changed |= !pending.is_empty();
                flush_pending(&mut pending, &mut repaired);
                let mut seen = HashSet::new();
                content.tool_calls.retain(|call| {
                    let valid = !call.id.trim().is_empty() && !call.name.trim().is_empty();
                    let unique = valid && seen.insert(call.id.clone());
                    changed |= !unique;
                    unique
                });
                pending.extend(content.tool_calls.iter().map(|call| call.id.clone()));
                if content.text.as_ref().is_some_and(|text| !text.is_empty())
                    || !content.tool_calls.is_empty()
                    || content.thinking.is_some()
                {
                    repaired.push(ChatMessage::Assistant(content));
                } else {
                    changed = true;
                }
            }
            ChatMessage::Tool(result) => {
                if let Some(index) = pending.iter().position(|id| *id == result.call_id) {
                    pending.remove(index);
                    repaired.push(ChatMessage::Tool(result));
                } else {
                    // 结果必须紧跟对应的 assistant 调用,且同一调用只能应答一次。
                    changed = true;
                }
            }
            other => {
                changed |= !pending.is_empty();
                flush_pending(&mut pending, &mut repaired);
                repaired.push(other);
            }
        }
    }
    changed |= !pending.is_empty();
    flush_pending(&mut pending, &mut repaired);
    if changed {
        tracing::debug!("resume:修复未完成、重复或错位的工具调用和结果");
    }
    repaired
}

/// v1.x:提取最后一条 Prompt Snapshot(取记录尾部向前第一条)。
///
/// 返回 `(model, system_json)`;无快照返回 `None`。调用方(bootstrap
/// resume 路径)在 model 与当前模型一致时激活快照槽。
pub fn last_prompt_snapshot(records: &[RolloutRecord]) -> Option<(String, serde_json::Value)> {
    records.iter().rev().find_map(|r| match r {
        RolloutRecord::PromptSnapshot {
            model, system_json, ..
        } => Some((model.clone(), system_json.clone())),
        _ => None,
    })
}

/// v1.x:输入守恒对账 —— 找出**崩溃窗口**里未完成的 turn(有 user 消息
/// 落盘但无 `InputDisposition` 终态记录)。
///
/// 返回这些 turn 的 id 列表(最多 5 条,按文件顺序)。resume 消费方据此
/// 显式告知「上次会话有 N 条输入未完成,是否重发」—— **永不自动重放**。
pub fn unsettled_input_turns(records: &[RolloutRecord]) -> Vec<String> {
    let mut settled: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut submitted: Vec<String> = Vec::new();
    let mut seen_turns: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in records {
        match r {
            RolloutRecord::Message { turn_id, role, .. } => {
                let tid = turn_id.to_string();
                if matches!(role, zhuye_protocol::MessageRole::User)
                    && seen_turns.insert(tid.clone())
                {
                    submitted.push(tid);
                }
            }
            RolloutRecord::InputDisposition { turn_id, .. } => {
                settled.insert(turn_id.to_string());
            }
            _ => {}
        }
    }
    submitted
        .into_iter()
        .filter(|t| !settled.contains(t))
        .take(5)
        .collect()
}

#[cfg(test)]
mod compaction_snapshot_tests {
    use super::*;
    use zhuye_protocol::{MessageRole, TurnId};

    #[test]
    fn snapshot_replaces_earlier_messages_and_keeps_later_fragments() {
        let turn = TurnId::new();
        let snapshot = vec![
            ChatMessage::System("<summary>摘要</summary>".into()),
            ChatMessage::User(zhuye_llm::UserContent {
                blocks: vec![zhuye_llm::ContentBlock::Text {
                    text: "压缩后的问题".into(),
                }],
            }),
        ];
        let records = vec![
            RolloutRecord::message(turn, MessageRole::User, serde_json::json!("旧问题")),
            RolloutRecord::message(
                turn,
                MessageRole::Assistant,
                serde_json::json!([{"type":"text","text":"旧回答"}]),
            ),
            RolloutRecord::Compaction {
                turn_id: turn,
                strategy: "smart_prune".into(),
                removed_count: 0,
                summary: String::new(),
                before_tokens: 100,
                after_tokens: 20,
                context_snapshot: Some(encode_context_snapshot(&snapshot).unwrap()),
            },
            RolloutRecord::message(
                turn,
                MessageRole::Assistant,
                serde_json::json!([{"type":"text","text":"新回答"}]),
            ),
        ];
        let replay = records_to_preload(&records);
        assert_eq!(replay.len(), 3);
        assert!(matches!(&replay[0], ChatMessage::System(s) if s == "<summary>摘要</summary>"));
        assert!(
            matches!(&replay[1], ChatMessage::User(u) if matches!(&u.blocks[0], zhuye_llm::ContentBlock::Text { text } if text == "压缩后的问题"))
        );
        assert!(
            matches!(&replay[2], ChatMessage::Assistant(a) if a.text.as_deref() == Some("新回答"))
        );
    }
}
