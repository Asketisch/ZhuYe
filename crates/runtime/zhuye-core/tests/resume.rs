//! `resume::records_to_preload` — RolloutRecord → ChatMessage 映射的回归测试。
//!
//! 覆盖:legacy String 形态、图文 blocks 形态(user 侧 ToolUse 忽略)、
//! assistant 工具对重建顺序(Assistant 在前 / Tool 紧随)、compaction 摘要
//! 前置 System、审计记录忽略、空输入。

use zhuye_core::resume::records_to_preload;
use zhuye_llm::ChatMessage;
use zhuye_protocol::{ContentBlock, MessageRole, RolloutRecord, ToolOutput, TurnId};

fn msg(role: MessageRole, content: serde_json::Value) -> RolloutRecord {
    RolloutRecord::message(TurnId::new(), role, content)
}

fn tool_output(text: &str) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text { text: text.into() }],
        is_error: false,
        metadata: serde_json::json!({}),
        elapsed_ms: 1,
    }
}

#[test]
fn legacy_string_user_and_assistant() {
    let records = vec![
        msg(MessageRole::User, serde_json::json!("我叫 Alice")),
        msg(MessageRole::Assistant, serde_json::json!("你好 Alice")),
    ];
    let out = records_to_preload(&records);
    assert_eq!(out.len(), 2);
    assert!(matches!(&out[0], ChatMessage::User(u) if u.blocks.len() == 1));
    assert!(
        matches!(&out[1], ChatMessage::Assistant(a) if a.text.as_deref() == Some("你好 Alice"))
    );
}

#[test]
fn user_block_array_keeps_text_and_image_skips_tooluse() {
    let blocks = vec![
        ContentBlock::Text {
            text: "看这张图".into(),
        },
        ContentBlock::Image {
            data: vec![1, 2, 3],
            mime_type: "image/png".into(),
        },
        // user 侧不应出现 ToolUse;出现则忽略。
        ContentBlock::ToolUse {
            id: "t1".into(),
            name: "bash".into(),
            args: serde_json::json!({}),
        },
    ];
    let records = vec![msg(
        MessageRole::User,
        serde_json::to_value(&blocks).unwrap(),
    )];
    let out = records_to_preload(&records);
    assert_eq!(out.len(), 1);
    match &out[0] {
        ChatMessage::User(u) => {
            assert_eq!(u.blocks.len(), 2, "Text + Image 保留,ToolUse 忽略");
        }
        other => panic!("expected User, got {other:?}"),
    }
}

#[test]
fn assistant_tool_pair_rebuilt_in_order() {
    let blocks = vec![
        ContentBlock::Text {
            text: "我去查一下".into(),
        },
        ContentBlock::ToolUse {
            id: "call-1".into(),
            name: "grep".into(),
            args: serde_json::json!({"q": "foo"}),
        },
        ContentBlock::ToolResult {
            call_id: "call-1".into(),
            output: tool_output("3 hits"),
        },
    ];
    let records = vec![msg(
        MessageRole::Assistant,
        serde_json::to_value(&blocks).unwrap(),
    )];
    let out = records_to_preload(&records);
    // Assistant(含 tool_calls)在前,Tool(ToolResult)紧随其后。
    assert_eq!(out.len(), 2);
    match &out[0] {
        ChatMessage::Assistant(a) => {
            assert_eq!(a.text.as_deref(), Some("我去查一下"));
            assert_eq!(a.tool_calls.len(), 1);
            assert_eq!(a.tool_calls[0].id, "call-1");
            assert_eq!(a.tool_calls[0].name, "grep");
        }
        other => panic!("expected Assistant first, got {other:?}"),
    }
    match &out[1] {
        ChatMessage::Tool(t) => assert_eq!(t.call_id, "call-1"),
        other => panic!("expected Tool second, got {other:?}"),
    }
}

#[test]
fn compaction_summary_becomes_leading_system() {
    let records = vec![
        msg(MessageRole::User, serde_json::json!("旧问题")),
        RolloutRecord::Compaction {
            turn_id: TurnId::new(),
            strategy: "microcompact".into(),
            removed_count: 4,
            summary: "会话摘要:讨论了 X".into(),
            before_tokens: 0,
            after_tokens: 0,
        },
        // 后续消息保留(全量历史 + 摘要前置是既定语义)。
        msg(MessageRole::User, serde_json::json!("新问题")),
    ];
    let out = records_to_preload(&records);
    // System(摘要) + 旧 user + 新 user。
    assert_eq!(out.len(), 3);
    assert!(matches!(&out[0], ChatMessage::System(s) if s.contains("会话摘要")));
}

#[test]
fn audit_records_ignored() {
    let records = vec![
        RolloutRecord::SessionMeta {
            session_id: zhuye_protocol::ThreadId::new(),
            model: "m".into(),
            started_at: chrono::Utc::now(),
            workspace: None,
        },
        RolloutRecord::Checkpoint {
            turn_id: TurnId::new(),
            sha: "abc".into(),
            label: None,
            created_at: chrono::Utc::now(),
        },
        RolloutRecord::TokenCount {
            turn_id: TurnId::new(),
            usage: Default::default(),
            cost_usd: None,
            model: None,
            provider: None,
            at: chrono::Utc::now(),
        },
        msg(MessageRole::User, serde_json::json!("唯一有效消息")),
    ];
    let out = records_to_preload(&records);
    assert_eq!(out.len(), 1, "仅对话消息参与,审计轨迹忽略");
}

#[test]
fn empty_input_empty_output() {
    assert!(records_to_preload(&[]).is_empty());
}

#[test]
fn missing_result_is_repaired_before_next_message_even_when_id_is_reused() {
    let call = ContentBlock::ToolUse {
        id: "shared".into(),
        name: "echo".into(),
        args: serde_json::json!({}),
    };
    let records = vec![
        msg(
            MessageRole::Assistant,
            serde_json::to_value(vec![call.clone()]).unwrap(),
        ),
        msg(MessageRole::User, serde_json::json!("下一问")),
        msg(
            MessageRole::Assistant,
            serde_json::to_value(vec![
                call,
                ContentBlock::ToolResult {
                    call_id: "shared".into(),
                    output: tool_output("真实结果"),
                },
                ContentBlock::ToolResult {
                    call_id: "shared".into(),
                    output: tool_output("重复结果"),
                },
                ContentBlock::ToolResult {
                    call_id: "orphan".into(),
                    output: tool_output("孤儿结果"),
                },
            ])
            .unwrap(),
        ),
    ];
    let out = records_to_preload(&records);
    assert_eq!(out.len(), 5, "{out:?}");
    assert!(matches!(&out[0], ChatMessage::Assistant(a) if a.tool_calls.len() == 1));
    assert!(matches!(&out[1], ChatMessage::Tool(t) if t.call_id == "shared" && t.is_error));
    assert!(matches!(&out[2], ChatMessage::User(_)));
    assert!(matches!(&out[3], ChatMessage::Assistant(a) if a.tool_calls.len() == 1));
    assert!(matches!(&out[4], ChatMessage::Tool(t) if t.call_id == "shared" && !t.is_error));
}

#[test]
fn partial_tool_batch_keeps_results_contiguous_before_user() {
    let records = vec![
        msg(
            MessageRole::Assistant,
            serde_json::to_value(vec![
                ContentBlock::ToolUse {
                    id: "one".into(),
                    name: "echo".into(),
                    args: serde_json::json!({}),
                },
                ContentBlock::ToolUse {
                    id: "two".into(),
                    name: "echo".into(),
                    args: serde_json::json!({}),
                },
                ContentBlock::ToolResult {
                    call_id: "two".into(),
                    output: tool_output("完成"),
                },
            ])
            .unwrap(),
        ),
        msg(MessageRole::User, serde_json::json!("继续")),
    ];
    let out = records_to_preload(&records);
    assert_eq!(out.len(), 4, "{out:?}");
    assert!(matches!(&out[0], ChatMessage::Assistant(a) if a.tool_calls.len() == 2));
    assert!(matches!(&out[1], ChatMessage::Tool(t) if t.call_id == "two" && !t.is_error));
    assert!(matches!(&out[2], ChatMessage::Tool(t) if t.call_id == "one" && t.is_error));
    assert!(matches!(&out[3], ChatMessage::User(_)));
}

#[test]
fn invalid_tool_calls_and_their_results_are_removed() {
    let records = vec![msg(
        MessageRole::Assistant,
        serde_json::to_value(vec![
            ContentBlock::ToolUse {
                id: "".into(),
                name: "echo".into(),
                args: serde_json::json!({}),
            },
            ContentBlock::ToolResult {
                call_id: "".into(),
                output: tool_output("无 ID"),
            },
            ContentBlock::ToolUse {
                id: "valid-id".into(),
                name: " ".into(),
                args: serde_json::json!({}),
            },
            ContentBlock::ToolResult {
                call_id: "valid-id".into(),
                output: tool_output("无工具名"),
            },
            ContentBlock::Text {
                text: "保留文本".into(),
            },
        ])
        .unwrap(),
    )];
    let out = records_to_preload(&records);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(
        matches!(&out[0], ChatMessage::Assistant(a) if a.text.as_deref() == Some("保留文本") && a.tool_calls.is_empty())
    );
}

#[test]
fn assistant_text_after_tool_result_stays_after_result() {
    let records = vec![msg(
        MessageRole::Assistant,
        serde_json::to_value(vec![
            ContentBlock::Text {
                text: "先查询".into(),
            },
            ContentBlock::ToolUse {
                id: "c1".into(),
                name: "echo".into(),
                args: serde_json::json!({}),
            },
            ContentBlock::ToolResult {
                call_id: "c1".into(),
                output: tool_output("查询结果"),
            },
            ContentBlock::Text {
                text: "查询结论".into(),
            },
        ])
        .unwrap(),
    )];
    let out = records_to_preload(&records);
    assert_eq!(out.len(), 3, "{out:?}");
    assert!(
        matches!(&out[0], ChatMessage::Assistant(a) if a.text.as_deref() == Some("先查询") && a.tool_calls.len() == 1)
    );
    assert!(matches!(&out[1], ChatMessage::Tool(t) if t.call_id == "c1"));
    assert!(
        matches!(&out[2], ChatMessage::Assistant(a) if a.text.as_deref() == Some("查询结论") && a.tool_calls.is_empty())
    );
}

#[test]
fn consecutive_tool_rounds_keep_their_own_results() {
    let call = ContentBlock::ToolUse {
        id: "same".into(),
        name: "echo".into(),
        args: serde_json::json!({}),
    };
    let records = vec![msg(
        MessageRole::Assistant,
        serde_json::to_value(vec![
            call.clone(),
            ContentBlock::ToolResult {
                call_id: "same".into(),
                output: tool_output("第一轮"),
            },
            call,
            ContentBlock::ToolResult {
                call_id: "same".into(),
                output: tool_output("第二轮"),
            },
        ])
        .unwrap(),
    )];
    let out = records_to_preload(&records);
    assert_eq!(out.len(), 4, "{out:?}");
    assert!(matches!(&out[0], ChatMessage::Assistant(a) if a.tool_calls.len() == 1));
    assert!(matches!(&out[1], ChatMessage::Tool(t) if t.call_id == "same" && !t.is_error));
    assert!(matches!(&out[2], ChatMessage::Assistant(a) if a.tool_calls.len() == 1));
    assert!(matches!(&out[3], ChatMessage::Tool(t) if t.call_id == "same" && !t.is_error));
}
