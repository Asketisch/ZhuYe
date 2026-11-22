//! Prompt cache 保温(cache warming)状态事件载荷。
//!
//! 引擎主动推送的诊断事件(无对应 Op,镜像 `ContextComposition` 形态):
//! 在 turn 间空闲期 / 回合内工具执行期,以一条廉价重放请求刷新 provider
//! prompt cache 的 TTL。决策与执行状态经本事件对客户端可见,便于与
//! `TokenCount` 的 `cached_tokens` 对照验证保温收益。

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// 保温动作类型。`scheduled` 仅在空闲期保温排程建立时发一次;
/// `cancelled` 仅在重放请求在途中被取消时发(正常静默取消不发声)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheWarmingAction {
    /// 空闲期保温周期已建立(每个 turn 完成后至多一次)。
    Scheduled,
    /// 已发送一次保温重放并成功完成。
    Executed,
    /// 决策不保温(周期随之结束);`reason` 给出原因。
    Skipped,
    /// 在途重放被取消(新请求到达 / 会话关闭)。
    Cancelled,
}

/// 保温阶段:回合内(工具执行期间,下一次模型调用近乎必然)或
/// 回合间空闲(延续概率按经验常数估计)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheWarmingPhase {
    /// 回合内:上次模型调用产生了工具调用,ToolExec 执行期间保温。
    Turn,
    /// 回合间:turn 正常完成后的空闲期保温。
    Idle,
}

/// v1.x:prompt cache 保温状态事件。所有数值字段可选(按动作填充),
/// 省略时不出现在 wire 上(与 `TokenCountEvent` 同策略)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheWarmingEvent {
    /// 保温动作(见 [`CacheWarmingAction`])。
    pub action: CacheWarmingAction,
    /// 保温阶段(见 [`CacheWarmingPhase`])。
    pub phase: CacheWarmingPhase,
    /// `skipped` / `cancelled` 的原因字符串(`no_economics` /
    /// `no_cache_hits` / `below_threshold` / `window_exhausted` /
    /// `thinking_budget` / `stale` / `prefix_drift` / `disabled` /
    /// `superseded` / `cancelled_by_request` / `cancelled_by_shutdown`)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// 候选请求的 prompt tokens(上次真实请求的 input_tokens)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    /// 本次重放流回的真实缓存命中 tokens(仅 executed 有值;与
    /// `TokenCount.cached_tokens` 同语义,cached ⊆ input)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_cached_tokens: Option<u32>,
    /// 本次保温重放的成本(USD;仅 executed 有值)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_cost_usd: Option<f64>,
    /// 决策期的期望节省估算(USD;executed / skipped 均可能携带)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_savings_usd: Option<f64>,
    /// 模型 spec(如 `"anthropic/claude-sonnet-5-5"`)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 发生时间(序列化为 UNIX 秒数,复用 config 模块的适配器)。
    #[serde(with = "super::config::systemtime_serde")]
    pub at: SystemTime,
}
