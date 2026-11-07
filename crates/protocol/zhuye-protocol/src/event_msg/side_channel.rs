//! v1.x:side-channel agent 的协议事件载荷(WP9)。
//!
//! 形状与 `zhuye-core::side_channel` 的 registry 事件一一对应,但**不
//! 依赖 core crate**(protocol 是底层契约层)—— 由 core 侧转换后发出。

use serde::{Deserialize, Serialize};

/// side-channel 生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SideChannelStatusMirror {
    /// 正在执行。
    Running,
    /// 自然完成(成功或结果为空)。
    Done,
    /// 已被用户取消。
    Cancelled,
    /// 因错误退出。
    Error,
}

impl SideChannelStatusMirror {
    /// 小写字符串,与 serde 序列化一致(`"running"` / `"done"` /
    /// `"cancelled"` / `"error"`)。
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
    /// 已启动(作为 `Op::StartSideChannel` 的应答发出)。
    Started {
        /// side-channel id(`side-<8 位十六进制>`,如 `side-00000001`)。
        id: String,
        /// 执行该 side-channel 的 agent 定义名(同 `Op::StartSideChannel.agent`)。
        agent_name: String,
        /// 注入的初始 prompt(同 `Op::StartSideChannel.prompt`)。
        prompt: String,
        /// 启动时间(UNIX epoch 毫秒)。
        started_at_ms: i64,
    },
    /// 已正常完成。
    Done {
        /// side-channel id(同 `Started.id`)。
        id: String,
        /// 进入终态的时间(UNIX epoch 毫秒)。
        finished_at_ms: i64,
        /// 最终 assistant 回复文本(最后一次工具调用之后累积的文本),供 UI
        /// 在频道标题显示;文本为空白时为 `None`。
        summary: Option<String>,
    },
    /// 已被取消(见 `Op::CancelSideChannel`)。
    Cancelled {
        /// side-channel id(同 `Started.id`)。
        id: String,
        /// 进入终态的时间(UNIX epoch 毫秒)。
        finished_at_ms: i64,
    },
    /// 因错误退出;启动即失败(driver 未配置 / 并发已满等)时也以本变体应答。
    Error {
        /// side-channel id(同 `Started.id`);启动即失败时尚未分配,为空串。
        id: String,
        /// 进入终态的时间(UNIX epoch 毫秒)。
        finished_at_ms: i64,
        /// 错误描述。
        message: String,
    },
}

/// side-channel 公开视图快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SideChannelInfo {
    /// side-channel id(`side-<8 位十六进制>`)。
    pub id: String,
    /// 执行该 side-channel 的 agent 定义名。
    pub agent_name: String,
    /// 注入的初始 prompt。
    pub prompt: String,
    /// 启动时间(UNIX epoch 毫秒)。
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
    /// 匹配的 side-channel 快照(按启动时间升序);指定 id 未命中或
    /// driver 未配置时为空。
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
