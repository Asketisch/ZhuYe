//! 协议属性测试:随机输入下 `Op` / `Submission` / `Event` 的 wire 往返。

use proptest::prelude::*;
use serde_json::Value;
use zhuye_protocol::{
    Event, EventMsg, Op, ReviewDecision, Submission, ThreadSettingsOverrides, UserInputItem,
};

fn arb_text() -> impl Strategy<Value = String> {
    "[\\p{L}\\p{N}\\p{P}\\p{Z}]{0,80}"
}

fn arb_user_input_item() -> impl Strategy<Value = UserInputItem> {
    arb_text().prop_map(|text| UserInputItem::Text { text })
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        prop::collection::vec(arb_user_input_item(), 0..8).prop_map(|items| Op::UserInput {
            items,
            thread_settings: ThreadSettingsOverrides::default(),
        }),
        Just(Op::Compact),
        arb_text().prop_map(|child_id| Op::Interrupt {
            child_id: (!child_id.is_empty()).then_some(child_id),
        }),
        arb_text().prop_map(|to_turn_id| Op::Rewind {
            to_turn_id: (!to_turn_id.is_empty()).then_some(to_turn_id),
        }),
        Just(Op::Shutdown),
        (
            arb_text(),
            prop_oneof![
                Just(ReviewDecision::Approve),
                arb_text().prop_map(|reason| ReviewDecision::Deny { reason }),
                Just(ReviewDecision::ApproveForSession),
            ]
        )
            .prop_map(|(id, decision)| Op::ToolApproval { id, decision }),
        arb_text().prop_map(|id| Op::HookApproval {
            id,
            decision: ReviewDecision::Approve,
        }),
    ]
}

fn wire_type(value: &Value) -> Option<&str> {
    value.get("type").and_then(Value::as_str)
}

proptest! {
    /// 任意受支持 Op 经 JSON 往返后判别子与关键 wire 名称保持。
    #[test]
    fn op_json_roundtrip_preserves_discriminant(op in arb_op()) {
        let wire = serde_json::to_value(&op).expect("Op 序列化失败");
        let decoded: Op = serde_json::from_value(wire.clone()).expect("Op 反序列化失败");
        prop_assert_eq!(decoded.discriminant(), op.discriminant());
        let reencoded = serde_json::to_value(&decoded).expect("Op 二次序列化失败");
        prop_assert_eq!(wire_type(&wire), wire_type(&reencoded));
    }

    /// Submission 外层信封在任意 id / Op 文本下完整往返。
    #[test]
    fn submission_envelope_roundtrip_preserves_id(id in arb_text(), op in arb_op()) {
        let submission = Submission::with_id(id.clone(), op);
        let wire = serde_json::to_value(&submission).expect("Submission 序列化失败");
        let decoded: Submission = serde_json::from_value(wire).expect("Submission 反序列化失败");
        prop_assert_eq!(decoded.id, id);
        prop_assert_eq!(decoded.op.discriminant(), submission.op.discriminant());
    }

    /// Event 信封保留关联 id;终态事件的最小合法 wire 不丢判别子。
    #[test]
    fn event_envelope_roundtrip_preserves_id(id in arb_text()) {
        let event = Event::new(id.clone(), EventMsg::ShutdownComplete);
        let wire = serde_json::to_value(&event).expect("Event 序列化失败");
        let decoded: Event = serde_json::from_value(wire).expect("Event 反序列化失败");
        prop_assert_eq!(decoded.id, id);
        prop_assert!(matches!(decoded.msg, EventMsg::ShutdownComplete));
    }
}
