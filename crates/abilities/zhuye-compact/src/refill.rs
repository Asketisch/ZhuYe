//! Rapid-refill 熔断器 —— 阻断「压缩 → 立即填满 → 再压缩」死循环。
//!
//! 修复背景(借鉴同类 agent 的 compact rapid-refill 熔断):上下文满 → 压缩 →
//! 新一轮立刻又被塞满 → 再压缩……每一轮 LLM 摘要都是全上下文读,死循环
//! 纯粹烧钱且毫无进展。本守卫在**会话级**跟踪「最近一次实际压缩发生的
//! 模型步号」:压缩后 ≤[`RAPID_REFILL_WINDOW_STEPS`] 步内再次触发压缩 =
//! rapid refill,连续 [`RAPID_REFILL_MAX_CONSECUTIVE`] 次 → [`RefillDecision::Abort`]
//! (调用方应以结构化错误收口当前 turn);窗口内的前几次 → [`RefillDecision::Skip`]
//! (跳过本次压缩,调用方注入提醒,让模型自行收尾);窗口外 → 计数清零放行。
//!
//! 手动压缩(`/compact`,force)不走本守卫 —— 用户显式指令不被熔断拦截。

/// rapid refill 判定窗口(模型步):最近一次压缩后多少步内再触发算 refill。
pub const RAPID_REFILL_WINDOW_STEPS: u64 = 3;

/// 连续 rapid refill 触达该次数 → Abort(调用方收口当前 turn)。
pub const RAPID_REFILL_MAX_CONSECUTIVE: u32 = 3;

/// 单次压缩触发的裁决。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefillDecision {
    /// 窗口外,正常执行压缩。
    Allow,
    /// 窗口内 rapid refill:跳过本次压缩(计数已累加),调用方注入提醒。
    Skip,
    /// 连续 rapid refill 达上限:放弃压缩并收口当前 turn。
    Abort,
}

/// 会话级 rapid-refill 守卫状态(`Arc<Mutex<..>>` 挂 `AgentConfig`,
/// 跨 turn 共享;步号取 `AgentConfig::model_step_counter`)。
#[derive(Debug, Default)]
pub struct RapidRefillGuard {
    /// 最近一次**实际发生**压缩的模型步号。
    last_compact_step: Option<u64>,
    /// 连续 rapid refill 触发次数(窗口外触发即清零)。
    consecutive: u32,
}

impl RapidRefillGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// 压缩触发前的预检(在真正调用压缩器**之前**调用 —— Skip/Abort 时
    /// 本次不应执行压缩)。
    ///
    /// - `step`:当前模型步号(会话级单调计数);
    /// - 窗口内 → 计数累加,达 [`RAPID_REFILL_MAX_CONSECUTIVE`] 返回
    ///   `Abort`,否则 `Skip`;窗口外(或从未压缩过)→ 计数清零放行。
    pub fn pre_check(&mut self, step: u64, window: u64, max_consecutive: u32) -> RefillDecision {
        let in_window = self
            .last_compact_step
            .is_some_and(|last| step.saturating_sub(last) <= window);
        if !in_window {
            self.consecutive = 0;
            return RefillDecision::Allow;
        }
        self.consecutive += 1;
        if self.consecutive >= max_consecutive {
            RefillDecision::Abort
        } else {
            RefillDecision::Skip
        }
    }

    /// 压缩**实际发生**后登记步号(Noop 不算)。这是窗口计时的锚点。
    pub fn on_compacted(&mut self, step: u64) {
        self.last_compact_step = Some(step);
        // 窗口锚点前移本身不代表 refill 结束:consecutive 只在窗口外
        // 触发时清零(pre_check 内),与同类 agent 语义一致。
    }

    /// 当前连续 refill 计数(诊断 / 测试)。
    pub fn consecutive(&self) -> u32 {
        self.consecutive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u64 = RAPID_REFILL_WINDOW_STEPS;
    const M: u32 = RAPID_REFILL_MAX_CONSECUTIVE;

    #[test]
    fn 首次触发放行() {
        let mut g = RapidRefillGuard::new();
        assert_eq!(g.pre_check(10, W, M), RefillDecision::Allow);
        g.on_compacted(10);
    }

    #[test]
    fn 窗口内连续触发_skip升级到_abort() {
        let mut g = RapidRefillGuard::new();
        g.on_compacted(10);
        // 步 11/12/13 都在窗口(≤3 步)内:第 1、2 次 Skip,第 3 次 Abort。
        assert_eq!(g.pre_check(11, W, M), RefillDecision::Skip);
        assert_eq!(g.pre_check(12, W, M), RefillDecision::Skip);
        assert_eq!(g.pre_check(13, W, M), RefillDecision::Abort);
        assert_eq!(g.consecutive(), 3);
    }

    #[test]
    fn 窗口外触发清零计数() {
        let mut g = RapidRefillGuard::new();
        g.on_compacted(10);
        let _ = g.pre_check(11, W, M); // Skip,consecutive = 1
        // 步 14(距 10 差 4 > 3)在窗口外 → 放行并清零。
        assert_eq!(g.pre_check(14, W, M), RefillDecision::Allow);
        assert_eq!(g.consecutive(), 0);
        // 再次进入窗口从 1 重新起算,不继承旧计数。
        g.on_compacted(14);
        assert_eq!(g.pre_check(15, W, M), RefillDecision::Skip);
    }

    #[test]
    fn 手动压缩语义由调用方保证_守卫只认步号() {
        // 守卫自身不区分 force:调用方(force 路径)必须绕过 pre_check,
        // 且手动压缩实际发生后应 on_compacted 前移锚点。
        let mut g = RapidRefillGuard::new();
        g.pre_check(1, W, M);
        g.on_compacted(1);
        assert_eq!(g.pre_check(2, W, M), RefillDecision::Skip);
    }

    #[test]
    fn 相同步号重复触发视为窗口内() {
        // 同步号多次触发(pre_loop 重入等病态场景)不该被 saturating_sub
        // 判成窗口外:step - last = 0 ≤ window。
        let mut g = RapidRefillGuard::new();
        g.on_compacted(10);
        assert_eq!(g.pre_check(10, W, M), RefillDecision::Skip);
        assert_eq!(g.pre_check(10, W, M), RefillDecision::Skip);
        assert_eq!(g.pre_check(10, W, M), RefillDecision::Abort);
    }
}
