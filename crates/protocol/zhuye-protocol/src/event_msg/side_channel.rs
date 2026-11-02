//! v1.x:side-channel agent 的协议事件载荷(WP9)。
//!
//! 形状与 `zhuye-core::side_channel` 的 registry 事件一一对应,但**不
//! 依赖 core crate**(protocol 是底层契约层)—— 由 core 侧转换后发出。

use serde::{Deserialize, Serialize};

/// side-channel 生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SideChannelStatusMirror {
    Running,
    Done,
    Cancelled,
    Error,
}

impl SideChannelStatusMirror {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
            Self::Error => "error",
        }
    }
}

/// side-channel 事件(Started / Done / Cancelled / Error 四态)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SideChannelEvent {
    Started {
        id: String,
        agent_name: String,
        prompt: String,
        started_at_ms: i64,
    },
    Done {
        id: String,
        finished_at_ms: i64,
        summary: Option<String>,
    },
    Cancelled {
        id: String,
        finished_at_ms: i64,
    },
    Error {
        id: String,
        finished_at_ms: i64,
        message: String,
    },
}

/// side-channel 公开视图快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SideChannelInfo {
    pub id: String,
    pub agent_name: String,
    pub prompt: String,
    pub started_at_ms: i64,
    /// `"running"` / `"done"` / `"cancelled"` / `"error"`。
    pub status: String,
    /// 持续时间(毫秒);运行中为 `None`。
    pub duration_ms: Option<i64>,
}

/// `Op::QuerySideChannels` 的应答载荷。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SideChannelStatusSnapshot {
    pub channels: Vec<SideChannelInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serde_uses_snake_case_kind_tag() {
        let ev = SideChannelEvent::Done {
            id: "side-00000001".into(),
            finished_at_ms: 7,
            summary: Some("ok".into()),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"kind\":\"done\""), "snake_case tag:{json}");
        let back: SideChannelEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, SideChannelEvent::Done { .. }));
    }

    #[test]
    fn info_serde_is_camel_case() {
        let info = SideChannelInfo {
            id: "side-00000001".into(),
            agent_name: "a".into(),
            prompt: "p".into(),
            started_at_ms: 1,
            status: "running".into(),
            duration_ms: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("agentName"), "camelCase:{json}");
        assert!(json.contains("startedAtMs"), "camelCase:{json}");
    }

    #[test]
    fn status_mirror_as_str() {
        assert_eq!(SideChannelStatusMirror::Running.as_str(), "running");
        assert_eq!(SideChannelStatusMirror::Error.as_str(), "error");
    }
}
