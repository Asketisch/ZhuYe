//! Event —— core → client 的状态单元。

use serde::{Deserialize, Serialize};

use crate::event_msg::EventMsg;

/// 用于标识不与具体 Submission 绑定的全局事件(如 SessionConfigured、ShutdownComplete)。
pub const EVENT_ID_NONE: &str = "";

/// Event 表示 core 发往客户端的一条事件(serve 模式下即 stdout 的一行 JSON)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// 与 `Submission.id` 对齐;不绑定 Submission 时取 `EVENT_ID_NONE`。
    pub id: String,
    /// 事件载荷(按 `type` 判别的 [`EventMsg`] 变体)。
    pub msg: EventMsg,
}

impl Event {
    /// 以关联 id 与载荷构造事件;全局事件的 `id` 传 [`EVENT_ID_NONE`]。
    pub fn new(id: impl Into<String>, msg: EventMsg) -> Self {
        Self { id: id.into(), msg }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_msg::EventMsg;

    #[test]
    fn event_id_none_sentinel() {
        let e = Event::new(EVENT_ID_NONE, EventMsg::ShutdownComplete);
        assert_eq!(e.id, "");
    }

    #[test]
    fn serde_roundtrip() {
        let e = Event::new("sub-1", EventMsg::ShutdownComplete);
        let json = serde_json::to_string(&e).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "sub-1");
    }
}
