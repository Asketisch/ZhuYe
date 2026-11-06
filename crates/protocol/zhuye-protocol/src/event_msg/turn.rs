//! Turn 生命周期载荷 + token 用量 / 状态子类型。

use serde::{Deserialize, Serialize};

use crate::item::TurnId;

/// turn 开始事件:core 受理 `Op::UserInput` 并为其创建新 turn 时发出。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnStartedEvent {
    /// 本 turn 的唯一标识。
    pub turn_id: TurnId,
    /// 触发本 turn 的用户消息 id(core 每轮新生成的 UUID 字符串);`None` = 未提供。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_message_id: Option<String>,
}

/// turn 收尾事件:自然结束、触达迭代上限或 token 预算耗尽时发出;
/// 被中断 / 拒绝的 turn 改发 `TurnAborted`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnCompleteEvent {
    /// 本 turn 的唯一标识(同 `TurnStarted.turn_id`)。
    pub turn_id: TurnId,
    /// 本 turn 内各次模型调用累计的 token 用量。
    pub usage: TokenUsage,
    /// turn 终止状态。
    pub status: TurnStatus,
}

/// turn 中止事件:turn 被用户中断或因错误(如 hook 拒绝 prompt)提前终止时发出。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnAbortedEvent {
    /// 被中止 turn 的标识;空闲期收到 `Op::Interrupt`(无在飞 turn)时为新生成的 id。
    pub turn_id: TurnId,
    /// 中止原因。
    pub reason: AbortReason,
}

/// 批次十九:`Op::Rewind` 成功后发出,让 TUI 裁剪显示到回退点。
/// `truncated_after` = 被丢弃的 turn 数(0 = 已是最近一条,无可回退)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnRewoundEvent {
    /// 回退到的 turn(`None` = 回退到 session 起 / 最近 user turn)。
    pub to_turn_id: Option<String>,
    /// 被丢弃的 turn 数(供 TUI Pill 文案)。
    pub truncated_after: usize,
}

/// Token 用量快照。镜像 `zhuye-llm` 的 `ChatEvent::Usage`。
///
/// **Token 计费口径(M8)**:`input_tokens` 是 LLM 上报的原始 `input`
/// 字段(Anthropic 的 `input_tokens` 已折入 `cache_creation_input_tokens`
/// 这段子片段)。`cached_tokens` 是 `cache_read_input_tokens` 折扣子片段,
/// 是 `input_tokens` 的一个**子集**,而非累加。`cache_write_tokens` 是
/// `cache_creation_input_tokens` 子片段,同样是 `input_tokens` 的子集。
/// 计费请使用 `pricing::price(model, usage)`,它会把四段按各自的倍率
/// 分摊。
///
/// `total_tokens = input_tokens + output_tokens` 对应线上可计费数量,
/// **故意保持与 M7 一致**;不要把 `cache_write_tokens` 或 `cached_tokens`
/// 加进去(否则会重复计费)。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    /// 输入 token 数(LLM 上报的原始 input,含 `cached_tokens` /
    /// `cache_write_tokens` 两段子片段)。
    pub input_tokens: u32,
    /// 输出 token 数。
    pub output_tokens: u32,
    /// 缓存读取(cache_read)命中的 token 数,是 `input_tokens` 的子集。
    pub cached_tokens: u32,
    /// M8:缓存写入分段(cache_creation)。已折入 `input_tokens`;**不要**
    /// 再计入 `total_tokens`。`#[serde(default)]` 保证与省略该字段的
    /// M7 消费者保持 wire 兼容。
    #[serde(default)]
    pub cache_write_tokens: u32,
    /// 可计费 token 总数,恒为 `input_tokens + output_tokens`。
    pub total_tokens: u32,
}

impl TokenUsage {
    /// 由输入 / 输出 / 缓存读取 token 数构造;`cache_write_tokens` 置 0,
    /// `total_tokens` 取 `input + output`。
    pub fn new(input: u32, output: u32, cached: u32) -> Self {
        Self {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            cache_write_tokens: 0,
            total_tokens: input + output,
        }
    }
}

/// turn 终止状态(`TurnComplete.status`)。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    /// 正常结束(模型不再发起工具调用,经 CheckStop 收尾)。
    Success,
    /// 触达迭代上限(`max_iterations` 安全阀)后强制收尾。
    MaxIterations,
    /// turn 被提前停止(当前引擎未产出该值,保留作协议兼容)。
    Stopped,
    /// v1.2 P1-12:会话级 token 预算耗尽(`[token_budget].session_total_tokens`
    /// 或 env `ZHUYE_TOKEN_BUDGET` 设的上限)后终止当前 turn。与
    /// `MaxIterations`(步数上限)区分,便于 TUI / 日志归因。
    TokenBudgetExceeded,
}

/// turn 中止原因(`TurnAborted.reason`)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AbortReason {
    /// 用户中断(`Op::Interrupt`)。
    UserInterrupt,
    /// 因错误中止(如 UserPromptSubmit hook 拒绝 prompt)。
    Error {
        /// 机器可读的错误码(如 `"prompt_rejected"`)。
        code: String,
        /// 人类可读的错误描述。
        message: String,
    },
    /// 会话关闭(`Op::Shutdown`)导致中止(当前引擎未产出该值,保留作协议兼容)。
    Shutdown,
}
