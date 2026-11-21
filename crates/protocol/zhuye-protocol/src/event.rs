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
    /// v1.12:会话扇出流上的单调序号(从 1 起)。由 `submission_loop` 的
    /// 会话扇出出口统一赋号,**逐会话**单调、跨进程生命周期不保证连续
    /// (进程重启归零)。客户端据此做缺口检测:收到 `seq > last + 1` 即
    /// 丢弃了事件,应走既有 replay 链路重同步;`seq == 0` 表示该事件未经
    /// 会话扇出出口(per-turn 专属流,如逐 token delta 的 SDK 快路),
    /// 不参与缺口判定。`#[serde(default)]` 双向兼容:旧 producer 的
    /// wire 无此字段,新 consumer 解析为 0;新 producer 的字段旧 consumer
    /// 忽略。
    #[serde(default)]
    pub seq: u64,
}

impl Event {
    /// 以关联 id 与载荷构造事件;全局事件的 `id` 传 [`EVENT_ID_NONE`]。
    /// `seq` 初始为 0(未赋号),由会话扇出出口统一 stamp。
    pub fn new(id: impl Into<String>, msg: EventMsg) -> Self {
        Self {
            id: id.into(),
            msg,
            seq: 0,
        }
    }

    /// 赋会话扇出序号(扇出出口专用;每条事件只赋一次)。
    pub fn with_seq(mut self, seq: u64) -> Self {
        self.seq = seq;
        self
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

    #[test]
    fn seq_defaults_to_zero_and_roundtrips() {
        // v1.12:seq 缺省 0(未赋号);with_seq 赋号后 roundtrip 保持。
        let e = Event::new("sub-1", EventMsg::ShutdownComplete);
        assert_eq!(e.seq, 0, "new() 不赋号,seq 必须为 0");
        let stamped = e.with_seq(7);
        assert_eq!(stamped.seq, 7);
        let json = serde_json::to_string(&stamped).unwrap();
        assert!(json.contains(r#""seq":7"#), "got: {json}");
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, 7);

        // 旧 producer 的 wire 无 seq 字段 → 反序列化为 0。
        let legacy: Event =
            serde_json::from_str(r#"{"id":"s","msg":{"type":"shutdown_complete"}}"#).unwrap();
        assert_eq!(legacy.seq, 0, "缺字段必须 defaults 为 0(wire 兼容)");
    }
}
