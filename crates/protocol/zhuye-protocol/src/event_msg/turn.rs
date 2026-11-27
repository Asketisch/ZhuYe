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

/// 任务树预算的一个时点快照;同一账本的多个快照不得累加。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskBudgetSnapshotEvent {
    /// 任务树账本标识。
    pub task_id: String,
    /// 发出快照的回合。
    pub turn_id: TurnId,
    /// 已成功预留的各次调用估算值之和。
    pub estimated_tokens: u64,
    /// provider 已报告的 token 总量。
    pub actual_tokens: u64,
    /// 在途调用预留的估算 token 数。
    pub reserved_tokens: u64,
    /// 无用量回执调用的估算 token 数。
    pub unknown_estimated_tokens: u64,
    /// 没有用量回执的调用数。
    pub unknown_calls: u64,
    /// 实际用量超出单次预留的累计差额。
    pub estimate_overrun_tokens: u64,
    /// 已成功派发的子代理总数。
    pub dispatched_subagents: u64,
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
/// **Token 计费口径(M8)**:`input_tokens` 是本次 prompt 的**全部**输入
/// token,含 `cached_tokens` / `cache_write_tokens` 两个子片段。
/// `cached_tokens` 是 `cache_read_input_tokens` 折扣子片段,
/// 是 `input_tokens` 的一个**子集**,而非累加。`cache_write_tokens` 是
/// `cache_creation_input_tokens` 子片段,同样是 `input_tokens` 的子集。
/// 计费请使用 `pricing::price(model, usage)`,它会把四段按各自的倍率
/// 分摊。
///
/// **provider 方言归一**:Anthropic 线缆的 `input_tokens` 只报「既未从
/// 缓存读出、也未为缓存写入」的部分,**不含**上面两个缓存分段,与本契约
/// 相反。provider 适配层负责在上报前折入(见
/// [`TokenUsage::fold_cache_segments`]),引擎内一律按本契约的「子集」
/// 口径消费 —— 否则 `cached > input` 会让缓存命中率算出 > 100% 的荒谬值。
///
/// `total_tokens = input_tokens + output_tokens` 对应线上可计费数量,
/// **故意保持与 M7 一致**;不要把 `cache_write_tokens` 或 `cached_tokens`
/// 加进去(否则会重复计费)。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    /// 输入 token 数(本次 prompt 的全部输入,含 `cached_tokens` /
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

    /// 把两个缓存分段折入 `input_tokens`,得到「本次 prompt 的全部输入」。
    ///
    /// 供 provider 适配层在 Anthropic 方言(线缆 `input_tokens` 不含缓存
    /// 分段)上报前归一口径使用 —— 折入后 `cached` / `cache_write` 才是
    /// `input_tokens` 的真子集。
    pub fn fold_cache_segments(input: u32, cached: u32, cache_write: u32) -> u32 {
        input.saturating_add(cached).saturating_add(cache_write)
    }

    /// 旧口径修复:把**未折入**的缓存分段补进 `input_tokens`。
    ///
    /// 旧版 Anthropic 适配器原样透传线缆值,落盘的历史记录里
    /// `input_tokens` 不含缓存分段 —— 特征是**缓存分段之和大于
    /// `input_tokens`**。此方法在该特征成立时补齐并重算 `total_tokens`;
    /// 已折入的新记录满足 `cached + cache_write ≤ input_tokens`,不会被
    /// 二次补齐(幂等)。
    ///
    /// 仅供读取历史 rollout 的兼容路径使用;新记录一律走
    /// [`TokenUsage::fold_cache_segments`]。
    pub fn repair_unfolded_cache_segments(&self) -> Self {
        let segments = self.cached_tokens.saturating_add(self.cache_write_tokens);
        if segments <= self.input_tokens {
            return self.clone();
        }
        let input = Self::fold_cache_segments(
            self.input_tokens,
            self.cached_tokens,
            self.cache_write_tokens,
        );
        Self {
            input_tokens: input,
            output_tokens: self.output_tokens,
            cached_tokens: self.cached_tokens,
            cache_write_tokens: self.cache_write_tokens,
            total_tokens: input.saturating_add(self.output_tokens),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_cache_segments_makes_cached_a_subset_of_input() {
        // Anthropic 线缆:input=122 只报未命中缓存的部分,cache_read=7440。
        let input = TokenUsage::fold_cache_segments(122, 7440, 0);
        assert_eq!(input, 7562);
        assert!(7440 <= input, "缓存读分段必须是 input 的子集");
    }

    #[test]
    fn repair_unfolded_cache_segments_folds_legacy_records() {
        // 旧口径记录:input 不含缓存分段(缓存分段之和 > input 是特征)。
        let legacy = TokenUsage {
            input_tokens: 122,
            output_tokens: 136,
            cached_tokens: 7440,
            cache_write_tokens: 0,
            total_tokens: 258,
        };
        let fixed = legacy.repair_unfolded_cache_segments();
        assert_eq!(fixed.input_tokens, 7562);
        assert_eq!(fixed.total_tokens, 7698, "total 应重算为 input + output");
        assert_eq!(fixed.cached_tokens, 7440);
    }

    #[test]
    fn repair_unfolded_cache_segments_is_idempotent() {
        // 已折入的新记录:缓存分段之和 ≤ input → 不再补齐。
        let folded = TokenUsage {
            input_tokens: 7562,
            output_tokens: 136,
            cached_tokens: 7440,
            cache_write_tokens: 0,
            total_tokens: 7698,
        };
        assert_eq!(folded.repair_unfolded_cache_segments(), folded);
        // 连续两次修复结果一致(幂等)。
        assert_eq!(
            legacy_usage().repair_unfolded_cache_segments(),
            legacy_usage().repair_unfolded_cache_segments()
        );
    }

    #[test]
    fn repair_unfolded_cache_segments_keeps_normal_records_untouched() {
        // OpenAI 方言:prompt_tokens 本就含 cached → 缓存分段之和 ≤ input。
        let openai = TokenUsage {
            input_tokens: 1000,
            output_tokens: 200,
            cached_tokens: 250,
            cache_write_tokens: 0,
            total_tokens: 1200,
        };
        assert_eq!(openai.repair_unfolded_cache_segments(), openai);
    }

    fn legacy_usage() -> TokenUsage {
        TokenUsage {
            input_tokens: 122,
            output_tokens: 136,
            cached_tokens: 7440,
            cache_write_tokens: 0,
            total_tokens: 258,
        }
    }
}
