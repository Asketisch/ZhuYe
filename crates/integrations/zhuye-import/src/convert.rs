//! IR → 烛夜 `RolloutRecord` 序列的统一降层(纯函数,无 I/O)。
//!
//! 产出契约(与原生 `submission_loop` 落盘形状逐字对齐):
//! - 首行 `SessionMeta`(索引契约:`session ls` / `replay` 都只认它);
//! - 每个 turn → 一条 user `message` + 一条 assistant `message`;
//!   assistant `content` 是按源时序折叠的 `ContentBlock` 数组
//!   (Text → ToolUse → ToolResult → Text …),与 v1.5 起的原生形状一致;
//! - `Compaction` 保持在源顺序位置(`records_to_preload` 取最后一条
//!   前置为 System 消息);
//! - 有用量的 turn 追加 `TokenCount`(供 `session ls/stats` 聚合)。

use zhuye_protocol::{ContentBlock, MessageRole, RolloutRecord, TokenUsage, ToolOutput};

use crate::model::{Entry, ImportedSession, ImportedTurn, IrBlock, UserBlock};

/// 转换开关(目前只有 thinking 折叠;与 `ImportOptions` 拆开是因为
/// keep_injected / include_sidechain 在 parse 期消费,与这里无关)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConvertOptions {
    /// 把 `IrBlock::Thinking` 折叠为 `<thinking>…</thinking>` 文本块
    /// 保留;默认(`false`)直接丢弃。
    pub include_thinking: bool,
}

/// 把一个 IR 会话降为可落盘的 `RolloutRecord` 序列。
///
/// 输出首元素必为 `SessionMeta`;空 turn 被跳过;孤儿 tool_result
/// (无同 call_id 的前导 tool_use)被丢弃 —— 它们会让 provider 拒绝
/// 回放后的消息序列。
pub fn imported_to_records(session: &ImportedSession, opts: &ConvertOptions) -> Vec<RolloutRecord> {
    let mut records = Vec::new();

    // 首行:会话元数据(thread_id 已在 parse 层定型)。
    records.push(RolloutRecord::SessionMeta {
        session_id: session.thread_id,
        model: session.model.clone(),
        started_at: session.started_at,
        workspace: session.workspace.clone(),
    });

    for entry in &session.entries {
        match entry {
            Entry::Turn(turn) => {
                append_turn(&mut records, turn, session, opts);
            }
            Entry::Compaction { summary, .. } => {
                // strategy 语义对齐原生:摘要是 LLM 生成的替换性摘要。
                // removed_count / token 前后值源侧不可得,取 0。
                records.push(RolloutRecord::Compaction {
                    turn_id: last_turn_id(&records),
                    strategy: "llm_summarize".to_string(),
                    removed_count: 0,
                    summary: summary.clone(),
                    before_tokens: 0,
                    after_tokens: 0,
                });
            }
        }
    }

    records
}

/// 追加一个 turn(user 记录 + assistant 记录 + 可选 TokenCount)。
fn append_turn(
    records: &mut Vec<RolloutRecord>,
    turn: &ImportedTurn,
    session: &ImportedSession,
    opts: &ConvertOptions,
) {
    if turn.is_empty() {
        return;
    }

    // ── user 记录 ──
    // user 块映射为 ContentBlock 数组(与原生 submission_loop 落盘一致);
    // 全空(纯被过滤)则不写 user 记录。
    let user_blocks: Vec<ContentBlock> = turn
        .user
        .iter()
        .map(|b| match b {
            UserBlock::Text { text } => ContentBlock::Text { text: text.clone() },
            UserBlock::Image { data, mime_type } => ContentBlock::Image {
                data: data.clone(),
                mime_type: mime_type.clone(),
            },
        })
        .collect();
    if !user_blocks.is_empty() {
        records.push(RolloutRecord::Message {
            turn_id: turn.turn_id,
            role: MessageRole::User,
            content: serde_json::to_value(&user_blocks).unwrap_or(serde_json::Value::Null),
        });
    }

    // ── assistant 记录 ──
    // IrBlock 按源时序映射;Thinking 按 opts 折叠或丢弃;孤儿
    // tool_result(其 call_id 未在更早块中出现过)丢弃。
    let mut assistant_blocks: Vec<ContentBlock> = Vec::new();
    let mut seen_calls = std::collections::HashSet::new();
    let mut pending_calls = std::collections::HashSet::new();
    let mut after_result = false;
    for b in &turn.assistant {
        match b {
            IrBlock::Text { text } => {
                if !text.trim().is_empty() {
                    if after_result {
                        seen_calls.clear();
                        pending_calls.clear();
                        after_result = false;
                    }
                    assistant_blocks.push(ContentBlock::Text { text: text.clone() });
                }
            }
            IrBlock::Thinking { text } => {
                if opts.include_thinking && !text.trim().is_empty() {
                    if after_result {
                        seen_calls.clear();
                        pending_calls.clear();
                        after_result = false;
                    }
                    assistant_blocks.push(ContentBlock::Text {
                        text: format!("<thinking>{text}</thinking>"),
                    });
                }
            }
            IrBlock::ToolUse { id, name, args } => {
                if id.trim().is_empty() || name.trim().is_empty() {
                    tracing::debug!("导入:丢弃缺少调用 ID 或工具名的工具调用");
                    continue;
                }
                if after_result {
                    seen_calls.clear();
                    pending_calls.clear();
                    after_result = false;
                }
                if !seen_calls.insert(id.clone()) {
                    tracing::debug!(call_id = %id, "导入:丢弃重复的工具调用");
                    continue;
                }
                pending_calls.insert(id.clone());
                assistant_blocks.push(ContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                });
            }
            IrBlock::ToolResult {
                call_id,
                output,
                is_error,
            } => {
                if !pending_calls.remove(call_id) {
                    // 孤儿或重复结果:本轮没有尚未应答的对应调用。
                    // 保留会让回放消息序列被 provider 拒绝,丢弃并留痕。
                    tracing::debug!(call_id = %call_id, "导入:丢弃孤儿 tool_result");
                    continue;
                }
                after_result = true;
                assistant_blocks.push(ContentBlock::ToolResult {
                    call_id: call_id.clone(),
                    output: ToolOutput {
                        content: vec![ContentBlock::Text {
                            text: output.clone(),
                        }],
                        is_error: *is_error,
                        metadata: serde_json::Value::Object(Default::default()),
                        elapsed_ms: 0,
                    },
                });
            }
        }
    }
    if !assistant_blocks.is_empty() {
        records.push(RolloutRecord::Message {
            turn_id: turn.turn_id,
            role: MessageRole::Assistant,
            content: serde_json::to_value(&assistant_blocks).unwrap_or(serde_json::Value::Null),
        });
    }

    // ── TokenCount(可选)──
    // TokenUsage 字段是 u32,源侧 u64 值 saturating 收敛。
    if let Some(usage) = turn.usage {
        records.push(RolloutRecord::TokenCount {
            turn_id: turn.turn_id,
            usage: TokenUsage {
                input_tokens: usage.input_tokens.min(u32::MAX as u64) as u32,
                output_tokens: usage.output_tokens.min(u32::MAX as u64) as u32,
                cached_tokens: usage.cached_tokens.min(u32::MAX as u64) as u32,
                cache_write_tokens: 0,
                total_tokens: usage
                    .input_tokens
                    .saturating_add(usage.output_tokens)
                    .min(u32::MAX as u64) as u32,
            },
            cost_usd: None,
            model: Some(session.model.clone()),
            provider: Some(session.kind.as_str().to_string()),
            at: turn.started_at.unwrap_or(session.started_at),
        });
    }
}

/// 取当前已生成记录里最后出现的 turn_id(compaction 记录需要挂靠);
/// 一条都没有(会话以压缩摘要开头)则新分配。
fn last_turn_id(records: &[RolloutRecord]) -> zhuye_protocol::TurnId {
    records
        .iter()
        .rev()
        .find_map(|r| match r {
            RolloutRecord::Message { turn_id, .. }
            | RolloutRecord::Compaction { turn_id, .. }
            | RolloutRecord::TokenCount { turn_id, .. } => Some(*turn_id),
            _ => None,
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Entry, ImportedSession, ImportedTurn, IrBlock, UserBlock};
    use chrono::{TimeZone, Utc};
    use zhuye_protocol::{MessageRole, ThreadId, TurnId};

    fn session(entries: Vec<Entry>) -> ImportedSession {
        ImportedSession {
            kind: crate::model::HarnessKind::Codex,
            source_id: "src".into(),
            thread_id: ThreadId::new(),
            model: "test/model".into(),
            started_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            workspace: Some("/w".into()),
            title: None,
            entries,
            warnings: Vec::new(),
        }
    }

    fn turn(user: &str, assistant: Vec<IrBlock>) -> ImportedTurn {
        ImportedTurn {
            turn_id: TurnId::new(),
            started_at: None,
            user: vec![UserBlock::Text { text: user.into() }],
            assistant,
            usage: None,
        }
    }

    /// 时序折叠:Text → ToolUse → ToolResult 必须保持源顺序。
    #[test]
    fn folds_blocks_in_source_order() {
        let s = session(vec![Entry::Turn(turn(
            "你好",
            vec![
                IrBlock::Text {
                    text: "我看一下".into(),
                },
                IrBlock::ToolUse {
                    id: "c1".into(),
                    name: "bash".into(),
                    args: serde_json::json!({"cmd": "ls"}),
                },
                IrBlock::ToolResult {
                    call_id: "c1".into(),
                    output: "a\nb".into(),
                    is_error: false,
                },
                IrBlock::Text {
                    text: "结论".into(),
                },
            ],
        ))]);
        let records = imported_to_records(&s, &ConvertOptions::default());
        assert_eq!(records.len(), 3, "meta + user + assistant");
        assert!(matches!(records[0], RolloutRecord::SessionMeta { .. }));
        let (role, content) = match &records[1] {
            RolloutRecord::Message { role, content, .. } => (role, content),
            other => panic!("期望 user message,得到 {other:?}"),
        };
        assert_eq!(*role, MessageRole::User);
        assert_eq!(content[0]["type"], "text");

        let blocks = match &records[2] {
            RolloutRecord::Message { content, .. } => content,
            other => panic!("期望 assistant message,得到 {other:?}"),
        };
        let types: Vec<&str> = blocks
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["type"].as_str().unwrap())
            .collect();
        assert_eq!(types, ["text", "tool_use", "tool_result", "text"]);
        assert_eq!(blocks.as_array().unwrap()[1]["id"], "c1");
        assert_eq!(blocks.as_array().unwrap()[2]["call_id"], "c1");
        assert_eq!(blocks.as_array().unwrap()[2]["output"]["is_error"], false);
    }

    /// Thinking 默认丢弃,开关打开时折叠为文本。
    #[test]
    fn thinking_folded_only_when_enabled() {
        let s = session(vec![Entry::Turn(turn(
            "q",
            vec![
                IrBlock::Thinking {
                    text: "想一下".into(),
                },
                IrBlock::Text { text: "答".into() },
            ],
        ))]);
        let off = imported_to_records(&s, &ConvertOptions::default());
        let on = imported_to_records(
            &s,
            &ConvertOptions {
                include_thinking: true,
            },
        );
        let asst = |r: &[RolloutRecord]| match &r[2] {
            RolloutRecord::Message { content, .. } => content.as_array().unwrap().clone(),
            other => panic!("{other:?}"),
        };
        let a_off = asst(&off);
        let a_on = asst(&on);
        assert_eq!(a_off.len(), 1, "thinking 默认丢弃");
        assert_eq!(a_off[0]["text"], "答");
        assert_eq!(a_on.len(), 2);
        assert!(a_on[0]["text"].as_str().unwrap().contains("<thinking>"));
    }

    /// 孤儿 tool_result 丢弃。
    #[test]
    fn drops_orphan_tool_result() {
        let s = session(vec![Entry::Turn(turn(
            "q",
            vec![IrBlock::ToolResult {
                call_id: "ghost".into(),
                output: "x".into(),
                is_error: false,
            }],
        ))]);
        let records = imported_to_records(&s, &ConvertOptions::default());
        // assistant 块全空 → 该记录不写。
        assert_eq!(records.len(), 2, "meta + user,assistant 空块不落盘");
    }

    #[test]
    fn ignores_duplicate_and_cross_turn_tool_results() {
        let call = IrBlock::ToolUse {
            id: "c1".into(),
            name: "echo".into(),
            args: serde_json::json!({}),
        };
        let result = IrBlock::ToolResult {
            call_id: "c1".into(),
            output: "一次".into(),
            is_error: false,
        };
        let s = session(vec![
            Entry::Turn(turn(
                "第一问",
                vec![call.clone(), call, result.clone(), result.clone()],
            )),
            Entry::Turn(turn(
                "第二问",
                vec![
                    result,
                    IrBlock::Text {
                        text: "完成".into(),
                    },
                ],
            )),
        ]);
        let records = imported_to_records(&s, &ConvertOptions::default());
        let assistants: Vec<_> = records
            .iter()
            .filter_map(|record| match record {
                RolloutRecord::Message {
                    role: MessageRole::Assistant,
                    content,
                    ..
                } => Some(content),
                _ => None,
            })
            .collect();
        assert_eq!(assistants.len(), 2);
        assert_eq!(assistants[0].as_array().unwrap().len(), 2);
        assert_eq!(assistants[0][0]["type"], "tool_use");
        assert_eq!(assistants[0][1]["type"], "tool_result");
        assert_eq!(assistants[1].as_array().unwrap().len(), 1);
        assert_eq!(assistants[1][0]["text"], "完成");
    }

    #[test]
    fn permits_reused_call_id_in_later_tool_round() {
        let call = IrBlock::ToolUse {
            id: "same".into(),
            name: "echo".into(),
            args: serde_json::json!({}),
        };
        let result = IrBlock::ToolResult {
            call_id: "same".into(),
            output: "完成".into(),
            is_error: false,
        };
        let s = session(vec![Entry::Turn(turn(
            "问题",
            vec![call.clone(), result.clone(), call, result],
        ))]);
        let records = imported_to_records(&s, &ConvertOptions::default());
        let RolloutRecord::Message { content, .. } = &records[2] else {
            panic!("应有助手消息")
        };
        let kinds: Vec<_> = content
            .as_array()
            .unwrap()
            .iter()
            .map(|block| block["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            ["tool_use", "tool_result", "tool_use", "tool_result"]
        );
    }

    #[test]
    fn drops_tool_calls_with_missing_identity_and_their_results() {
        let s = session(vec![Entry::Turn(turn(
            "问题",
            vec![
                IrBlock::ToolUse {
                    id: "".into(),
                    name: "echo".into(),
                    args: serde_json::json!({}),
                },
                IrBlock::ToolResult {
                    call_id: "".into(),
                    output: "无 ID".into(),
                    is_error: false,
                },
                IrBlock::ToolUse {
                    id: "c1".into(),
                    name: " ".into(),
                    args: serde_json::json!({}),
                },
                IrBlock::ToolResult {
                    call_id: "c1".into(),
                    output: "无工具名".into(),
                    is_error: false,
                },
                IrBlock::Text {
                    text: "有效答复".into(),
                },
            ],
        ))]);
        let records = imported_to_records(&s, &ConvertOptions::default());
        let RolloutRecord::Message { content, .. } = &records[2] else {
            panic!("应有助手消息")
        };
        assert_eq!(content.as_array().unwrap().len(), 1);
        assert_eq!(content[0]["text"], "有效答复");
    }

    /// Compaction 位置保持,summary 透传。
    #[test]
    fn compaction_keeps_position() {
        let s = session(vec![
            Entry::Turn(turn("q1", vec![IrBlock::Text { text: "a".into() }])),
            Entry::Compaction {
                summary: "摘要".into(),
                at: None,
            },
            Entry::Turn(turn("q2", vec![IrBlock::Text { text: "b".into() }])),
        ]);
        let records = imported_to_records(&s, &ConvertOptions::default());
        let kinds: Vec<&str> = records
            .iter()
            .map(|r| match r {
                RolloutRecord::SessionMeta { .. } => "meta",
                RolloutRecord::Message { role, .. } if *role == MessageRole::User => "user",
                RolloutRecord::Message { role, .. } if *role == MessageRole::Assistant => "asst",
                RolloutRecord::Compaction { .. } => "compaction",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            ["meta", "user", "asst", "compaction", "user", "asst"]
        );
        match &records[3] {
            RolloutRecord::Compaction {
                summary, strategy, ..
            } => {
                assert_eq!(summary, "摘要");
                assert_eq!(strategy, "llm_summarize");
            }
            other => panic!("{other:?}"),
        }
    }

    /// 用量 → TokenCount,且 u64 溢出收敛。
    #[test]
    fn usage_becomes_token_count() {
        let mut t = turn("q", vec![IrBlock::Text { text: "a".into() }]);
        t.usage = Some(crate::model::ImportedUsage {
            input_tokens: 100,
            output_tokens: 20,
            cached_tokens: 60,
        });
        let s = session(vec![Entry::Turn(t)]);
        let records = imported_to_records(&s, &ConvertOptions::default());
        match records.last().unwrap() {
            RolloutRecord::TokenCount {
                usage, provider, ..
            } => {
                assert_eq!(usage.input_tokens, 100);
                assert_eq!(usage.output_tokens, 20);
                assert_eq!(usage.cached_tokens, 60);
                assert_eq!(usage.total_tokens, 120);
                assert_eq!(provider.as_deref(), Some("codex"));
            }
            other => panic!("{other:?}"),
        }
    }
}
