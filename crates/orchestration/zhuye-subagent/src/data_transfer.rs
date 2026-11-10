//! 父级与子代理之间的数据流。
//!
//! - [`DataTransferConfig::pass_context_messages`] —— 从父级复制到子 agent 初始消息列表
//!   的尾部消息数量。
//! - [`ResultExtractor`] —— 如何从 `TurnHandle` 事件流中抽取子 agent 的最终回答,
//!   并返回给父级的工具。

use zhuye_llm::ChatMessage;
use zhuye_protocol::{ContentBlock, Event, EventMsg};

/// 单次 `spawn()` 调用时父↔子数据流的配置。
#[derive(Debug, Clone)]
pub struct DataTransferConfig {
    /// 从父级 `state.messages` 末尾复制并前置到子 agent 初始 prompt 的消息数;
    /// 若截断边界落在工具结果中,会向前扩展以保留完整调用和结果。
    /// `0` 表示子 agent 仅接收自己的 system prompt + 工具传入的 user input。
    pub pass_context_messages: usize,
    /// 如何从子 agent 事件流中抽取最终回答。
    pub result_extractor: ResultExtractor,
}

impl Default for DataTransferConfig {
    fn default() -> Self {
        Self {
            pass_context_messages: 0,
            result_extractor: ResultExtractor::LastAssistantText,
        }
    }
}

/// 从子 agent 事件流中挑选回复的策略。
#[derive(Debug, Clone)]
pub enum ResultExtractor {
    /// 取最后一个助手消息的文本;工具调用前的过程文本不进入最终结果。
    LastAssistantText,
    /// 取 `call_id` 与给定字符串匹配的 `ToolResult` 的 `content`。
    /// 用于子 agent 应调用某个特定终止工具(例如 `finish_subagent`)的场景。
    LastToolResult(String),
}

/// 遍历 `TurnHandle` 的事件流,按所选 [`ResultExtractor`] 抽取回答。
/// 若流结束但没有匹配事件,返回 `None`。
pub fn extract_result(events: &[Event], extractor: &ResultExtractor) -> Option<String> {
    match extractor {
        ResultExtractor::LastAssistantText => {
            let mut acc = String::new();
            for e in events {
                match &e.msg {
                    EventMsg::AgentMessageDelta(d) => acc.push_str(&d.delta),
                    // 全量消息是当前助手消息的权威快照,后续阶段仍可能继续。
                    EventMsg::AgentMessage(m) => acc.clone_from(&m.text),
                    // 工具调用开启新阶段,后续最终回复不能混入调用前的说明。
                    EventMsg::ToolCallBegin(_) => acc.clear(),
                    _ => {}
                }
            }
            if acc.is_empty() { None } else { Some(acc) }
        }
        ResultExtractor::LastToolResult(want_id) => {
            for e in events.iter().rev() {
                if let EventMsg::ToolCallEnd(end) = &e.msg {
                    if &end.call_id == want_id {
                        for c in &end.output.content {
                            if let ContentBlock::Text { text } = c {
                                return Some(text.clone());
                            }
                        }
                    }
                }
            }
            None
        }
    }
}

/// 子代理返回后附加 `[Coordinator Principle]` footer(父协调者 reminder)。
pub fn append_coordinator_principle_footer(result: &str, footer: Option<&str>) -> String {
    let Some(footer) = footer.filter(|f| !f.trim().is_empty()) else {
        return result.to_string();
    };
    format!("{result}\n\n[Coordinator Principle]\n{footer}")
}

/// 构造子 agent 的初始消息列表:复制父级末尾 `pass_context_messages`
/// 条 chat 消息,然后追加新传入的 user input。
pub fn build_child_initial_messages(
    parent_tail: &[ChatMessage],
    user_input: ChatMessage,
) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = parent_tail.to_vec();
    out.push(user_input);
    out
}

/// 截取父会话尾部,移除尚未完成的委派调用,并保住跨截断边界的工具配对。
pub(crate) fn select_parent_context(
    mut history: Vec<ChatMessage>,
    current_call_id: &str,
    requested: usize,
) -> Vec<ChatMessage> {
    if requested == 0 {
        return Vec::new();
    }
    if let Some(ChatMessage::Assistant(last)) = history.last_mut() {
        if last
            .tool_calls
            .iter()
            .any(|call| call.id == current_call_id)
        {
            // 同一批次里的全部工具尚未返回,不能将半对调用传给子代理。
            last.tool_calls.clear();
            last.thinking = None;
            if last.text.as_ref().is_none_or(|text| text.is_empty()) {
                history.pop();
            }
        }
    }
    let mut start = history.len().saturating_sub(requested);
    if let Some(ChatMessage::Tool(result)) = history.get(start) {
        // 尾部从工具结果开始时,回溯同一批次的 assistant 调用和结果。
        let mut boundary = start;
        while boundary > 0 && matches!(history[boundary - 1], ChatMessage::Tool(_)) {
            boundary -= 1;
        }
        if boundary > 0 {
            if let ChatMessage::Assistant(assistant) = &history[boundary - 1] {
                if assistant
                    .tool_calls
                    .iter()
                    .any(|call| call.id == result.call_id)
                {
                    start = boundary - 1;
                }
            }
        }
    }
    zhuye_core::resume::repair_orphan_tool_calls(history.drain(start..).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zhuye_protocol::{AgentMessage, Event, ToolOutput};

    fn evt_text(t: &str) -> Event {
        Event::new(
            "sub",
            EventMsg::AgentMessage(AgentMessage { text: t.into() }),
        )
    }

    fn evt_delta(d: &str) -> Event {
        Event::new(
            "sub",
            EventMsg::AgentMessageDelta(zhuye_protocol::AgentMessageDelta { delta: d.into() }),
        )
    }

    fn evt_tool_result(call_id: &str, text: &str) -> Event {
        Event::new(
            "sub",
            EventMsg::ToolCallEnd(zhuye_protocol::ToolCallEndEvent {
                call_id: call_id.into(),
                output: ToolOutput {
                    content: vec![ContentBlock::Text { text: text.into() }],
                    is_error: false,
                    metadata: serde_json::Value::Null,
                    elapsed_ms: 0,
                },
                is_error: false,
                elapsed_ms: 0,
                child_id: None,
            }),
        )
    }

    fn evt_tool_begin(call_id: &str) -> Event {
        Event::new(
            "sub",
            EventMsg::ToolCallBegin(zhuye_protocol::ToolCallBeginEvent {
                call_id: call_id.into(),
                tool_name: "lookup".into(),
                args: serde_json::Value::Null,
                child_id: None,
            }),
        )
    }

    #[test]
    fn extract_last_assistant_text_from_delta_acc() {
        let events = vec![evt_delta("hel"), evt_delta("lo")];
        let got = extract_result(&events, &ResultExtractor::LastAssistantText);
        assert_eq!(got, Some("hello".into()));
    }

    #[test]
    fn extract_last_assistant_text_from_message_event() {
        let events = vec![evt_delta("hel"), evt_text("lo")];
        let got = extract_result(&events, &ResultExtractor::LastAssistantText);
        assert_eq!(got, Some("lo".into()));
    }

    #[test]
    fn 工具调用后的最终回复不包含前面的过程文本() {
        let events = vec![
            evt_delta("准备"),
            evt_tool_begin("c1"),
            evt_tool_result("c1", "工具结果"),
            evt_delta("最终"),
            evt_delta("答案"),
        ];
        assert_eq!(
            extract_result(&events, &ResultExtractor::LastAssistantText),
            Some("最终答案".into())
        );
        let no_final = vec![
            evt_text("过程说明"),
            evt_tool_begin("c1"),
            evt_tool_result("c1", "结果"),
        ];
        assert_eq!(
            extract_result(&no_final, &ResultExtractor::LastAssistantText),
            None
        );
    }

    #[test]
    fn extract_last_tool_result() {
        let events = vec![
            evt_tool_result("c1", "first"),
            evt_tool_result("c2", "second"),
        ];
        let got = extract_result(&events, &ResultExtractor::LastToolResult("c2".into()));
        assert_eq!(got, Some("second".into()));
    }

    #[test]
    fn extract_returns_none_when_no_match() {
        let events = vec![evt_tool_result("c1", "x")];
        let got = extract_result(&events, &ResultExtractor::LastToolResult("missing".into()));
        assert!(got.is_none());
    }

    #[test]
    fn build_child_messages_appends_user_input() {
        use zhuye_llm::{ContentBlock, UserContent};
        let parent = vec![ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::text("old1")],
        })];
        let new = ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::text("new")],
        });
        let msgs = build_child_initial_messages(&parent, new);
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn 父上下文跨工具结果边界时补齐整个调用批次() {
        use zhuye_llm::{AssistantContent, ToolCallRequest, ToolResult, UserContent};
        let call = |id: &str| ToolCallRequest {
            id: id.into(),
            name: "echo".into(),
            arguments: serde_json::json!({}),
        };
        let result = |id: &str| {
            ChatMessage::Tool(ToolResult {
                call_id: id.into(),
                content: vec![zhuye_llm::ContentBlock::text(id)],
                is_error: false,
            })
        };
        let history = vec![
            ChatMessage::Assistant(AssistantContent {
                tool_calls: vec![call("a"), call("b")],
                ..Default::default()
            }),
            result("a"),
            result("b"),
            ChatMessage::User(UserContent {
                blocks: vec![zhuye_llm::ContentBlock::text("继续")],
            }),
            ChatMessage::Assistant(AssistantContent {
                tool_calls: vec![call("delegate")],
                ..Default::default()
            }),
        ];
        let selected = select_parent_context(history, "delegate", 2);
        assert_eq!(selected.len(), 4);
        assert!(matches!(&selected[0], ChatMessage::Assistant(a) if a.tool_calls.len() == 2));
        assert!(matches!(&selected[1], ChatMessage::Tool(r) if r.call_id == "a"));
        assert!(matches!(&selected[2], ChatMessage::Tool(r) if r.call_id == "b"));
        assert!(matches!(&selected[3], ChatMessage::User(_)));
    }

    #[test]
    fn 父上下文按请求条数截取并保留委派前文本() {
        use zhuye_llm::{AssistantContent, ToolCallRequest, UserContent};
        let mut history: Vec<_> = (0..15)
            .map(|n| {
                ChatMessage::User(UserContent {
                    blocks: vec![zhuye_llm::ContentBlock::text(format!("消息{n}"))],
                })
            })
            .collect();
        history.push(ChatMessage::Assistant(AssistantContent {
            text: Some("准备委派".into()),
            tool_calls: vec![ToolCallRequest {
                id: "delegate".into(),
                name: "call_explorer".into(),
                arguments: serde_json::json!({}),
            }],
            ..Default::default()
        }));
        assert!(select_parent_context(history.clone(), "delegate", 0).is_empty());
        let selected = select_parent_context(history, "delegate", 12);
        assert_eq!(selected.len(), 12);
        assert!(matches!(&selected[0], ChatMessage::User(u)
            if matches!(&u.blocks[0], zhuye_llm::ContentBlock::Text { text } if text == "消息4")));
        assert!(matches!(selected.last(), Some(ChatMessage::Assistant(a))
            if a.text.as_deref() == Some("准备委派") && a.tool_calls.is_empty()));
    }
}
