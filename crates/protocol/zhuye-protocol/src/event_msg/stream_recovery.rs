//! 流锚点恢复(stream recovery)事件族载荷。
//!
//! 修复背景:SSE 中断 / 网络瞬断越过 provider 层重试边界后,此前只有
//! `stream_error` + `agent_message_reset`(客户端清半成品缓冲后整段重来)
//! —— 丢弃了什么、从哪里重开、第几次尝试,客户端与审计侧均不可见。
//! 本事件族把恢复过程事件化:`started`(恢复启动)→ `anchor_selected`
//! (选定安全锚点)→ `tail_discarded`(丢弃的尾部已留痕)→
//! `retry_started`(从锚点重开请求)。四个变体共享同一载荷,按
//! `type` 区分;对客户端全部是**可观察诊断事件**,不改变既有
//! `stream_error` / `agent_message_reset` 的语义与顺序。

use serde::{Deserialize, Serialize};

/// 流恢复事件载荷。数值字段按 stage 可选,省略不出现在 wire 上
/// (与 `TokenCountEvent` 同策略)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamRecoveryEvent {
    /// 第几次恢复尝试(1 起)。
    pub attempt: u32,
    /// 失败原因码(与 `stream_error.code` 同源)。
    pub reason_code: String,
    /// 瞬态分类(`provider_timeout` / `provider_network_error` /
    /// `provider_stream_error`;无法分类时缺省)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transient: Option<String>,
    /// 选定的恢复锚点(`previous_message` = 最后一条已提交消息;
    /// 预留 `latest_committed_tool_result` 等)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    /// 丢弃尾部的文本字符数(仅 tail_discarded)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discarded_text_chars: Option<u64>,
    /// 丢弃尾部的思考字符数(仅 tail_discarded)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discarded_thinking_chars: Option<u64>,
    /// 建议重试等待毫秒(仅 retry_started;与 `agent_message_reset`
    /// 的 `retry_in_ms` 同值)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_in_ms: Option<u64>,
}
