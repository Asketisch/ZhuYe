//! Prompt cache 保温(cache warming)决策模块 —— 纯函数,零 I/O。
//!
//! 职责切分与 pi(`packages/ai/src/cache-warming.ts`)一致:**宿主管调度,
//! 本模块管数字**。调度器(zhuye-core `CacheWarmingScheduler`)决定何时
//! 询问、何时发送重放;本模块只回答两个问题:
//!
//! 1. 这一次刷新该不该发([`evaluate`])—— 期望收益是否覆盖成本;
//! 2. 刷新延时是多少([`refresh_delay_secs`])—— 在 TTL 的 90% 处触发,
//!    保留最小安全边际。
//!
//! ## 与 pi 决策公式的异同
//!
//! 同:`expected_savings = 延续概率 × miss成本 − warm成本`,≥ 阈值才保温;
//! 空闲期延续概率用经验常数(0.15),回合内为 1.0;失效重读单价按
//! 「有写入档用写入价,无写入档回退 input 价」(兼容 OpenAI 自动缓存)。
//!
//! 异(ZhuYe 有真实命中回执,决策消费观测值而非假设):
//! - **观测门控**:上次真实请求既无 cache_read 也无 cache_write
//!   (`CacheObservation` 全零)→ provider 实际没有缓存行为,保温必然
//!   无法命中,直接跳过 —— pi 假设整个 prompt 已缓存,ZhuYe 不做此假设;
//! - **warmCost 精化**:重放成本 = 存活段(cache_read 价)+ 失效段
//!   (重写价)+ 1 个输出 token。存活段按观测的 `cached + cache_write`
//!   估计(Anthropic 两段都有回执,估计准;OpenAI 只回执 read 段,
//!   write 段按失效保守计价 —— 宁可高估成本少保温,不白花钱)。
//!
//! ## 本模块不知道的事(调度器职责)
//!
//! `CacheBreakDetector` 指纹漂移检查、凭据一致性、代际失效、窗口计时、
//! 重放请求构造 —— 全部在 zhuye-core 调度器侧完成;本模块零 I/O、无
//! 任何引擎状态依赖,表驱动单测完全离线。

use std::time::Duration;

use crate::providers::{WarmingPrices, warming_prices};
use crate::request::{CacheTtl, ChatRequest};

/// 保温重放请求的元数据标记键。真实 provider 忽略未知 metadata 键
/// (wire 不变);mock provider 据此走「不消耗脚本行」的旁路。
pub const METADATA_WARM_REPLAY: &str = "warm_replay";

/// [`METADATA_WARM_REPLAY`] 的标记值。
pub const METADATA_WARM_REPLAY_VALUE: &str = "true";

/// 期望节省阈值(USD)默认值:一次刷新只有期望节省 ≥ 此值才发。
/// 与 pi `CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS` 一致。
pub const DEFAULT_MIN_EXPECTED_SAVINGS_USD: f64 = 0.05;

/// 空闲期延续概率默认值:「TTL 到期前会有下一次真实请求」的经验常数。
/// pi 注释:按会话逐一估计并不比全局常数更准。与 pi
/// `CACHE_WARMING_IDLE_CONTINUATION_PROBABILITY` 一致。
pub const DEFAULT_IDLE_CONTINUATION_PROBABILITY: f64 = 0.15;

/// 回合内保温最长窗口(秒,自候选请求发起起算)。与 pi
/// `CACHE_WARMING_MAX_AGE_MS`(60 分钟)一致。
pub const DEFAULT_MAX_TURN_WINDOW_SECS: u64 = 60 * 60;

/// 空闲保温最长窗口(秒)。越老延续估计越不可靠,地平线更短。与 pi
/// `CACHE_WARMING_MAX_IDLE_AGE_MS`(30 分钟)一致。
pub const DEFAULT_MAX_IDLE_WINDOW_SECS: u64 = 30 * 60;

/// prompt 规模门槛默认值(token):低于此值不保温。与 OpenAI 自动缓存
/// ≥1024 token 的门槛对齐;小上下文的 miss 增量本来就低于节省阈值。
pub const DEFAULT_MIN_CONTEXT_TOKENS: u32 = 1024;

/// Anthropic wire 词表的断点 TTL 秒值换算(5m / 1h)。
/// 修复备注:消息锚点 / 工具尾部 / system 兜底断点的 TTL 此前在
/// provider wire 层硬编码 5m,现已由请求层逐断点携带(见 anthropic.rs
/// 的 `configured_ttl`)—— 推导必须读请求里的真实值,不得假设常量。
fn ttl_secs(ttl: CacheTtl) -> u64 {
    match ttl {
        CacheTtl::FiveMinutes => 300,
        CacheTtl::OneHour => 3600,
    }
}

/// system 块断点的默认存活时长(秒):与 zhuye-prompt `system_cache_ttl`
/// 的默认值(1h,env `ZHUYE_CACHE_TTL` 可改)一致。仅在请求无任何显式
/// 断点时作兜底。
pub const SYSTEM_BREAK_TTL_SECS: u64 = 3600;

/// 保温策略设置(全部有默认;config `[cache_warming]` 逐字段覆盖)。
#[derive(Debug, Clone, PartialEq)]
pub struct CacheWarmingSettings {
    /// 总开关。**默认关闭** —— 保温是引擎自发花钱的行为,必须显式 opt-in。
    pub enabled: bool,
    /// 期望节省阈值(USD)。
    pub min_expected_savings_usd: f64,
    /// 空闲期延续概率(0.0-1.0)。
    pub idle_continuation_probability: f64,
    /// prompt 规模门槛(token),低于此值不保温。
    pub min_context_tokens: u32,
    /// TTL 显式覆盖(秒)。`None` = 按请求断点推导(见
    /// [`effective_ttl_secs`])。
    pub ttl_secs: Option<u64>,
    /// 刷新安全边际(秒):在 `ttl − margin` 之前触发重放。
    pub refresh_margin_secs: u64,
    /// 回合内保温最长窗口(秒)。
    pub max_turn_window_secs: u64,
    /// 空闲保温最长窗口(秒)。
    pub max_idle_window_secs: u64,
    /// 测试逃生门:跳过定价 / 观测命中 / 规模门控,决策恒为 Warm
    /// (配合 mock provider 的端到端测试)。生产配置不应开启。
    pub force: bool,
}

impl Default for CacheWarmingSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            min_expected_savings_usd: DEFAULT_MIN_EXPECTED_SAVINGS_USD,
            idle_continuation_probability: DEFAULT_IDLE_CONTINUATION_PROBABILITY,
            min_context_tokens: DEFAULT_MIN_CONTEXT_TOKENS,
            ttl_secs: None,
            refresh_margin_secs: 10,
            max_turn_window_secs: DEFAULT_MAX_TURN_WINDOW_SECS,
            max_idle_window_secs: DEFAULT_MAX_IDLE_WINDOW_SECS,
            force: false,
        }
    }
}

/// 保温阶段。与 pi 的 `streaming` / `idle` 两相位一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmingPhase {
    /// 回合内:上次模型调用产生了工具调用,ToolExec 执行期间保温
    /// (下一次模型调用近乎必然,延续概率 1.0)。
    Turn,
    /// 回合间:turn 正常完成后的空闲期(延续概率取设置值)。
    Idle,
}

impl WarmingPhase {
    /// 本阶段的延续概率。
    fn continuation_probability(&self, settings: &CacheWarmingSettings) -> f64 {
        match self {
            WarmingPhase::Turn => 1.0,
            WarmingPhase::Idle => settings.idle_continuation_probability,
        }
    }

    /// 本阶段的最长保温窗口(秒)。
    fn max_window_secs(&self, settings: &CacheWarmingSettings) -> u64 {
        match self {
            WarmingPhase::Turn => settings.max_turn_window_secs,
            WarmingPhase::Idle => settings.max_idle_window_secs,
        }
    }
}

/// 决策为不保温(或无法保温)的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// 总开关未开启。
    Disabled,
    /// 经济学不可用:模型未定价 / 全零价(本地模型)/ prompt 低于规模门槛。
    NoEconomics,
    /// 上次真实请求没有任何缓存读或写回执 —— provider 实际没有缓存
    /// 行为(网关不透传 / 模型不支持),保温无法命中。
    NoCacheHits,
    /// 期望节省低于阈值。
    BelowThreshold,
    /// 超过本阶段的最长保温窗口。
    WindowExhausted,
    /// 请求带显式 thinking 预算(Anthropic `Enabled{budget_tokens}`):
    /// API 要求 `max_tokens > budget_tokens`,1-token 重放会被拒绝。
    ThinkingBudget,
}

impl SkipReason {
    /// 稳定的字符串判别值(进 `CacheWarmingEvent.reason`)。
    pub fn as_str(&self) -> &'static str {
        match self {
            SkipReason::Disabled => "disabled",
            SkipReason::NoEconomics => "no_economics",
            SkipReason::NoCacheHits => "no_cache_hits",
            SkipReason::BelowThreshold => "below_threshold",
            SkipReason::WindowExhausted => "window_exhausted",
            SkipReason::ThinkingBudget => "thinking_budget",
        }
    }
}

/// 决策动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmingAction {
    /// 发送保温重放。
    Warm,
    /// 不保温(调度周期随之结束,直到下一次真实请求重新排程)。
    Stop,
}

/// 单次保温决策的输入观测:上次真实请求流回的缓存段回执
/// ([`crate::event::UsageSnapshot`] 的 cache 两字段)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheObservation {
    /// 上次请求的 cache_read 段(命中存量缓存的部分)。
    pub cached_tokens: u32,
    /// 上次请求的 cache_write 段(新写入缓存的部分;OpenAI 不回执,恒 0)。
    pub cache_write_tokens: u32,
}

impl CacheObservation {
    /// 是否有任何缓存行为回执(读或写)。
    fn any(&self) -> bool {
        self.cached_tokens > 0 || self.cache_write_tokens > 0
    }

    /// 估计当前仍存活于缓存中的前缀长度。Anthropic 的 read + write 两段
    /// 都已入缓存;超过 `prompt_tokens` 时按饱和处理。
    fn alive_tokens(&self, prompt_tokens: u32) -> u32 {
        self.cached_tokens
            .saturating_add(self.cache_write_tokens)
            .min(prompt_tokens)
    }
}

/// 单次保温决策的产出。
#[derive(Debug, Clone, PartialEq)]
pub struct CacheWarmingDecision {
    /// 动作:发或不发。
    pub action: WarmingAction,
    /// 不保温的原因(action = Stop 时必有)。
    pub skip_reason: Option<SkipReason>,
    /// 重放成本估算(USD;经济学不可用时为 None)。
    pub warm_cost_usd: Option<f64>,
    /// miss 增量估算(USD;经济学不可用时为 None)。
    pub miss_cost_usd: Option<f64>,
    /// 期望节省估算(USD;经济学不可用时为 None)。
    pub expected_savings_usd: Option<f64>,
}

fn stop(
    reason: SkipReason,
    warm: Option<f64>,
    miss: Option<f64>,
    savings: Option<f64>,
) -> CacheWarmingDecision {
    CacheWarmingDecision {
        action: WarmingAction::Stop,
        skip_reason: Some(reason),
        warm_cost_usd: warm,
        miss_cost_usd: miss,
        expected_savings_usd: savings,
    }
}

/// 单次保温决策(纯函数)。
///
/// - `model_id`:完整 spec(容忍 `provider/` 前缀)或裸模型名均可;
/// - `prompt_tokens`:上次真实请求的 `input_tokens`;
/// - `observation`:上次真实请求的缓存段回执;
/// - `age_secs`:距候选请求(上次真实模型调用)发起经过的秒数,用于
///   窗口判定。
///
/// `settings.force = true` 时(测试逃生门)跳过全部经济学 / 观测 / 规模
/// 门控,直接返回 Warm(成本字段为 None)。
pub fn evaluate(
    model_id: &str,
    prompt_tokens: u32,
    observation: CacheObservation,
    phase: WarmingPhase,
    age_secs: u64,
    settings: &CacheWarmingSettings,
) -> CacheWarmingDecision {
    if !settings.enabled {
        return stop(SkipReason::Disabled, None, None, None);
    }
    if settings.force {
        return CacheWarmingDecision {
            action: WarmingAction::Warm,
            skip_reason: None,
            warm_cost_usd: None,
            miss_cost_usd: None,
            expected_savings_usd: None,
        };
    }
    // 窗口:超过本阶段上限后延续估计不再可靠,停到下一次真实请求。
    if age_secs >= phase.max_window_secs(settings) {
        return stop(SkipReason::WindowExhausted, None, None, None);
    }
    // 观测门控:没有任何缓存回执 → provider 没有缓存行为,保温必不命中。
    if !observation.any() {
        return stop(SkipReason::NoCacheHits, None, None, None);
    }
    let Some(p) = warming_prices(model_id) else {
        return stop(SkipReason::NoEconomics, None, None, None);
    };
    // 全零价(本地模型)没有经济学意义。
    if p.input <= 0.0 {
        return stop(SkipReason::NoEconomics, None, None, None);
    }
    if prompt_tokens < settings.min_context_tokens {
        return stop(SkipReason::NoEconomics, None, None, None);
    }
    let (warm, miss, savings) = economics(p, prompt_tokens, observation, phase, settings);
    if savings >= settings.min_expected_savings_usd {
        CacheWarmingDecision {
            action: WarmingAction::Warm,
            skip_reason: None,
            warm_cost_usd: Some(warm),
            miss_cost_usd: Some(miss),
            expected_savings_usd: Some(savings),
        }
    } else {
        stop(
            SkipReason::BelowThreshold,
            Some(warm),
            Some(miss),
            Some(savings),
        )
    }
}

/// 经济学算式:返回 `(warm_cost, miss_cost, expected_savings)`。
///
/// - 失效重读单价:有显式写入档(Anthropic 1.25×)按写入价;无写入档
///   (OpenAI)回退非缓存 input 价 —— 与 pi 的
///   `cacheMissCost = cacheWrite > 0 ? write : input` 同构;
/// - warmCost = 存活段(观测值)× read 价 + 失效段 × 重写价 + 1 输出 token;
/// - missCost = 下一次真实请求「缓存已死 vs 存活」的差价 × prompt 规模;
/// - expectedSavings = 延续概率 × missCost − warmCost。
fn economics(
    p: WarmingPrices,
    prompt_tokens: u32,
    observation: CacheObservation,
    phase: WarmingPhase,
    settings: &CacheWarmingSettings,
) -> (f64, f64, f64) {
    let miss_full_price = if p.cache_write > 0.0 {
        p.cache_write
    } else {
        p.input
    };
    let alive = observation.alive_tokens(prompt_tokens) as f64;
    let dead = (prompt_tokens - observation.alive_tokens(prompt_tokens)) as f64;
    let warm_cost = p.cache_read * alive + miss_full_price * dead + p.output;
    let miss_cost = (miss_full_price - p.cache_read).max(0.0) * prompt_tokens as f64;
    let probability = phase.continuation_probability(settings);
    let expected_savings = probability * miss_cost - warm_cost;
    (warm_cost, miss_cost, expected_savings)
}

/// 推导一次请求的保温 TTL(秒)。
///
/// 优先级:`settings.ttl_secs` 显式覆盖 > 请求断点推导。断点推导口径:
/// 取请求内**全部显式断点**(system 块 cache_control + 消息锚点
/// CacheBreak)TTL 的最小值 —— 最短命的断点决定缓存整体失效时刻。
/// 修复备注:TTL 现由请求层逐断点携带(anthropic.rs 已透传配置值,
/// 默认 1h),不得假设某一类断点是固定常量。无任何显式断点时按 system
/// 默认 1h 兜底。
pub fn effective_ttl_secs(request: &ChatRequest, settings: &CacheWarmingSettings) -> u64 {
    if let Some(t) = settings.ttl_secs {
        return t;
    }
    let mut min = SYSTEM_BREAK_TTL_SECS;
    for b in &request.system.0 {
        if let Some(cc) = &b.cache_control
            && let Some(ttl) = cc.ttl
        {
            min = min.min(ttl_secs(ttl));
        }
    }
    for br in &request.cache_control {
        min = min.min(ttl_secs(br.ttl));
    }
    min
}

/// 距 TTL 到期的刷新延时:在 90% TTL 处触发,保留至少
/// `settings.refresh_margin_secs` 边际。TTL 太短(不足边际 + 1s)返回
/// `None` —— 不值得保温。与 pi `getCacheWarmingDelayMs` 同构(秒版)。
pub fn refresh_delay_secs(ttl_secs: u64, settings: &CacheWarmingSettings) -> Option<u64> {
    let margin = settings.refresh_margin_secs.max(1);
    if ttl_secs <= margin + 1 {
        return None;
    }
    Some((ttl_secs * 9 / 10).max(1).min(ttl_secs - margin))
}

/// 由 TTL 构造的刷新延时便捷形式([`Duration`])。
pub fn refresh_delay(ttl_secs: u64, settings: &CacheWarmingSettings) -> Option<Duration> {
    refresh_delay_secs(ttl_secs, settings).map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{CacheBreak, CacheControl, CacheControlKind, CacheTtl, SystemBlock};

    fn settings() -> CacheWarmingSettings {
        CacheWarmingSettings {
            enabled: true,
            ..Default::default()
        }
    }

    fn obs(cached: u32, write: u32) -> CacheObservation {
        CacheObservation {
            cached_tokens: cached,
            cache_write_tokens: write,
        }
    }

    #[test]
    fn disabled_settings_stop_with_disabled_reason() {
        let s = CacheWarmingSettings::default();
        let d = evaluate(
            "anthropic/claude-sonnet-4-latest",
            100_000,
            obs(90_000, 0),
            WarmingPhase::Idle,
            0,
            &s,
        );
        assert_eq!(d.action, WarmingAction::Stop);
        assert_eq!(d.skip_reason, Some(SkipReason::Disabled));
    }

    #[test]
    fn no_cache_observation_skips() {
        // 上次请求零回执:provider 没有缓存行为,保温必不命中。
        let s = settings();
        let d = evaluate(
            "anthropic/claude-sonnet-4-latest",
            100_000,
            obs(0, 0),
            WarmingPhase::Turn,
            0,
            &s,
        );
        assert_eq!(d.skip_reason, Some(SkipReason::NoCacheHits));
    }

    #[test]
    fn unknown_model_skips_no_economics() {
        let s = settings();
        let d = evaluate(
            "some/unknown-model",
            100_000,
            obs(90_000, 0),
            WarmingPhase::Turn,
            0,
            &s,
        );
        assert_eq!(d.skip_reason, Some(SkipReason::NoEconomics));
    }

    #[test]
    fn zero_price_model_skips_no_economics() {
        // Ollama 本地模型:input_price = 0,没有经济学意义。
        let s = settings();
        let d = evaluate(
            "ollama/llama3.2",
            100_000,
            obs(0, 100_000),
            WarmingPhase::Turn,
            0,
            &s,
        );
        assert_eq!(d.skip_reason, Some(SkipReason::NoEconomics));
    }

    #[test]
    fn small_context_skips_no_economics() {
        let s = settings();
        let d = evaluate(
            "anthropic/claude-sonnet-4-latest",
            500,
            obs(500, 0),
            WarmingPhase::Turn,
            0,
            &s,
        );
        assert_eq!(d.skip_reason, Some(SkipReason::NoEconomics));
    }

    #[test]
    fn turn_phase_anthropic_warm_math() {
        // claude-sonnet-4-latest:input $3/M、read 0.1×($0.3/M)、
        // write 1.25×($3.75/M)、output $15/M。
        // pt=100_000,cached=80_000,write=10_000 → alive=90_000,dead=10_000。
        // warm = 0.3e-6×90k + 3.75e-6×10k + 15e-6×1 = 0.027 + 0.0375 + 0.000015
        // miss = (3.75-0.3)e-6×100k = 0.345
        // Turn 概率 1.0 → savings = 0.345 - 0.064515 = 0.280485 ≥ 0.05 → Warm。
        let s = settings();
        let d = evaluate(
            "anthropic/claude-sonnet-4-latest",
            100_000,
            obs(80_000, 10_000),
            WarmingPhase::Turn,
            0,
            &s,
        );
        assert_eq!(d.action, WarmingAction::Warm);
        let warm = d.warm_cost_usd.unwrap();
        let miss = d.miss_cost_usd.unwrap();
        assert!((warm - 0.064515).abs() < 1e-9, "warm = {warm}");
        assert!((miss - 0.345).abs() < 1e-9, "miss = {miss}");
        assert!((d.expected_savings_usd.unwrap() - 0.280485).abs() < 1e-9);
    }

    #[test]
    fn idle_phase_small_context_below_threshold() {
        // 同样上下文,Idle 概率 0.15 → savings = 0.15×0.345 − 0.064515 < 0
        // → BelowThreshold(但成本字段仍给出,便于观测)。
        let s = settings();
        let d = evaluate(
            "anthropic/claude-sonnet-4-latest",
            100_000,
            obs(80_000, 10_000),
            WarmingPhase::Idle,
            0,
            &s,
        );
        assert_eq!(d.action, WarmingAction::Stop);
        assert_eq!(d.skip_reason, Some(SkipReason::BelowThreshold));
        assert!(d.expected_savings_usd.unwrap() < 0.0);
    }

    #[test]
    fn big_context_idle_warms() {
        // 足够大的上下文:pt=1M、全存活 → warm = 0.3×1 + 15e-6 ≈ 0.300015,
        // miss = 3.45 × 0.15 = 0.5175 → savings ≈ 0.217 ≥ 0.05 → Warm。
        let s = settings();
        let d = evaluate(
            "anthropic/claude-sonnet-4-latest",
            1_000_000,
            obs(1_000_000, 0),
            WarmingPhase::Idle,
            0,
            &s,
        );
        assert_eq!(d.action, WarmingAction::Warm);
        assert!(d.expected_savings_usd.unwrap() >= 0.05);
    }

    #[test]
    fn openai_miss_full_price_falls_back_to_input() {
        // gpt-4o:read 0.5×、write 0 → miss_full_price = input($5/M)。
        // pt=1M、cached=500k、write=0 → alive=500k。
        // warm = 2.5e-6×500k + 5e-6×500k + 20e-6 = 1.25 + 2.5 + 0.00002。
        // miss = (5-2.5)e-6×1M = 2.5;Turn 概率 1 → savings = 2.5 - 3.75002 < 0
        // → BelowThreshold(OpenAI 写入段无回执,保守高估 warm 成本 ——
        // 宁可少保温不白花钱)。
        let s = settings();
        let d = evaluate(
            "openai/gpt-4o",
            1_000_000,
            obs(500_000, 0),
            WarmingPhase::Turn,
            0,
            &s,
        );
        assert_eq!(d.action, WarmingAction::Stop);
        assert_eq!(d.skip_reason, Some(SkipReason::BelowThreshold));
        let warm = d.warm_cost_usd.unwrap();
        assert!((warm - 3.75002).abs() < 1e-9, "warm = {warm}");
        assert!((d.miss_cost_usd.unwrap() - 2.5).abs() < 1e-9);
    }

    #[test]
    fn force_bypasses_all_gates() {
        // force:disabled 之外的一切门都跳过(mock + 端到端测试用),
        // 成本字段 None(无经济学)。
        let s = CacheWarmingSettings {
            force: true,
            ..settings()
        };
        let d = evaluate(
            "unknown/model",
            10,
            obs(0, 0),
            WarmingPhase::Idle,
            999_999,
            &s,
        );
        assert_eq!(d.action, WarmingAction::Warm);
        assert_eq!(d.skip_reason, None);
        assert_eq!(d.warm_cost_usd, None);
    }

    #[test]
    fn still_disabled_with_force() {
        // force 不能越过总开关:enabled=false 时永远 Stop。
        let s = CacheWarmingSettings {
            force: true,
            ..CacheWarmingSettings::default()
        };
        let d = evaluate("unknown/model", 10, obs(0, 0), WarmingPhase::Idle, 0, &s);
        assert_eq!(d.skip_reason, Some(SkipReason::Disabled));
    }

    #[test]
    fn window_exhausted_by_age() {
        let s = settings();
        // Idle 窗口 1800s:age 达到上限即停。
        let d = evaluate(
            "anthropic/claude-sonnet-4-latest",
            1_000_000,
            obs(1_000_000, 0),
            WarmingPhase::Idle,
            1800,
            &s,
        );
        assert_eq!(d.skip_reason, Some(SkipReason::WindowExhausted));
        // Turn 窗口 3600s:同样的 age 在 Turn 内仍在窗口内。
        let d = evaluate(
            "anthropic/claude-sonnet-4-latest",
            1_000_000,
            obs(1_000_000, 0),
            WarmingPhase::Turn,
            1800,
            &s,
        );
        assert_ne!(d.skip_reason, Some(SkipReason::WindowExhausted));
    }

    #[test]
    fn effective_ttl_reads_actual_break_ttl() {
        let s = CacheWarmingSettings::default();
        let mut req = ChatRequest::default();
        req.cache_control.push(CacheBreak {
            after_message_index: 0,
            ttl: CacheTtl::OneHour,
        });
        // 修复后:TTL 由请求层逐断点携带(anthropic.rs 已透传配置值),
        // 1h 锚点 → 3600s(修复前此处错误假设 wire 硬编码 5m 而恒取 300)。
        assert_eq!(effective_ttl_secs(&req, &s), 3600);
    }

    #[test]
    fn effective_ttl_takes_min_across_breaks() {
        // 最短命断点决定缓存整体失效时刻:system 5m + 锚点 1h → 300s。
        let s = CacheWarmingSettings::default();
        let mut req = ChatRequest::default();
        req.system.0.push(SystemBlock {
            text: "sys".into(),
            cache_control: Some(CacheControl {
                kind: CacheControlKind::Ephemeral,
                ttl: Some(CacheTtl::FiveMinutes),
            }),
            ephemeral: true,
        });
        req.cache_control.push(CacheBreak {
            after_message_index: 0,
            ttl: CacheTtl::OneHour,
        });
        assert_eq!(effective_ttl_secs(&req, &s), 300);
    }

    #[test]
    fn effective_ttl_from_system_block() {
        let s = CacheWarmingSettings::default();
        let mut req = ChatRequest::default();
        req.system.0.push(SystemBlock {
            text: "sys".into(),
            cache_control: Some(CacheControl {
                kind: CacheControlKind::Ephemeral,
                ttl: Some(CacheTtl::FiveMinutes),
            }),
            ephemeral: true,
        });
        assert_eq!(effective_ttl_secs(&req, &s), 300);

        let mut req_1h = ChatRequest::default();
        req_1h.system.0.push(SystemBlock {
            text: "sys".into(),
            cache_control: Some(CacheControl {
                kind: CacheControlKind::Ephemeral,
                ttl: Some(CacheTtl::OneHour),
            }),
            ephemeral: true,
        });
        assert_eq!(effective_ttl_secs(&req_1h, &s), 3600);

        // 无断点:回落 system 默认 1h。
        assert_eq!(effective_ttl_secs(&ChatRequest::default(), &s), 3600);
    }

    #[test]
    fn effective_ttl_settings_override_wins() {
        let s = CacheWarmingSettings {
            ttl_secs: Some(42),
            ..Default::default()
        };
        assert_eq!(effective_ttl_secs(&ChatRequest::default(), &s), 42);
    }

    #[test]
    fn refresh_delay_ninety_percent_with_margin() {
        let s = CacheWarmingSettings::default();
        // 300s TTL → min(270, 290) = 270s。
        assert_eq!(refresh_delay_secs(300, &s), Some(270));
        // 3600s → 3240s。
        assert_eq!(refresh_delay_secs(3600, &s), Some(3240));
        // TTL ≤ margin+1 → None(不值得保温)。
        assert_eq!(refresh_delay_secs(10, &s), None);
        assert_eq!(refresh_delay_secs(11, &s), None);
        // 极小但足够的 TTL:取 min(90% , ttl − margin) = min(10, 2) = 2。
        assert_eq!(refresh_delay_secs(12, &s), Some(2));
    }
}
