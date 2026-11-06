//! M10 / v0.2.4 讨论(collab)生命周期载荷。

use serde::{Deserialize, Serialize};

use super::turn::TokenUsage;

/// M10/v0.2.4: 讨论启动事件。`id` 是 `DiscussionId` UUID 的字符串形式;
/// `participants` 与 `mode` 直接镜像 orchestrator 启动时的配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollabStartedEvent {
    /// 讨论 id(`DiscussionId` UUID 的字符串形式)。
    pub id: String,
    /// 参与讨论的 agent id 列表(按配置顺序)。
    pub participants: Vec<String>,
    /// 讨论模式:`"sequential"`(每轮逐个 agent 串行发言)|
    /// `"concurrent"`(每轮所有 agent 并发发言)。
    pub mode: String,
}

/// M10/v0.2.4: 单条讨论消息事件。每次 `MessageBus::route` 成功投递后
/// orchestrator 会发出一次。`token_usage` 仅在 LLM 路径
///(`collect_result_with_usage` 提供 usage)非空时填入;老 caller 与
/// `run_noop` 路径发出的消息为 `None`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollabMessageEvent {
    /// 所属讨论 id(同 `CollabStarted.id`)。
    pub id: String,
    /// 发言 agent 的 id。
    pub from: String,
    /// 消息种类序列化形式:`"utterance"` | `"consensus"` | `"finish"`。
    /// 用字符串而非 enum 以避免 `protocol ↔ discussion` 循环依赖。
    pub kind: String,
    /// 消息文本内容。
    pub content: String,
    /// 该消息发出时所在的轮次(从 0 起)。
    pub round: u32,
    /// 生成该消息的 LLM token 用量;非 LLM 路径(或未上报用量)为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<TokenUsage>,
}

/// M10/v0.2.4: 讨论结束事件。`outcome` 序列化形式
/// `consensus` | `no_consensus` | `finished`;`rounds` 是实际跑过的轮数
///(可能小于 `max_rounds`)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollabFinishedEvent {
    /// 所属讨论 id(同 `CollabStarted.id`)。
    pub id: String,
    /// 讨论结局:`"consensus"`(达成共识)| `"no_consensus"`(跑满
    /// `max_rounds` 仍未共识)| `"finished"`(某 agent 主动结束讨论)。
    pub outcome: String,
    /// 实际跑过的轮数(轮次计数,而非 0 起的轮次序号)。
    pub rounds: u32,
}
