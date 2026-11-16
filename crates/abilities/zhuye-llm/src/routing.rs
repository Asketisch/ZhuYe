//! # 动态选模(F43)
//!
//! 每轮自动挑合适模型:改错别字用便宜快的、难的重构自动升旗舰。
//!
//! ## 三块
//!
//! 1. **回合分类**([`classify_turn`])—— 按任务特征(长度 / 关键词 /
//!    是否需要工具)估复杂度,映射到 [`ModelTier`]。
//! 2. **向上升级 + 迟滞**([`RoutingDecision`])—— 简单任务用低档,但
//!    **连续失败**或**用户显式要求**时升级;升级后不立刻降回(迟滞),
//!    避免在档位边界反复横跳。
//! 3. **角色绑定**([`RoleBinding`])—— 主会话 / 压缩 / 子代理各配各的
//!    模型(压缩用小模型省钱,子代理按分型)。
//!
//! ## 为什么需要迟滞(hysteresis)
//!
//! 没有迟滞时:第 N 轮判"复杂"升档,第 N+1 轮判"简单"降档 —— 模型
//! 每轮都换,缓存全失效、行为不连贯。迟滞要求"连续 K 轮都判简单才降档"。
//!
//! ## 为什么分类器可注入
//!
//! 生产环境可用小模型分类(更准),测试 / 离线用启发式(零成本)。
//! [`classify_turn`] 是启发式实现;[`ClassifierFn`] 允许替换。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 模型档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    /// 便宜快(小模型)。
    Cheap,
    /// 标准(中档)。
    Standard,
    /// 旗舰(最强)。
    Flagship,
}

impl ModelTier {
    pub const ALL: [ModelTier; 3] = [ModelTier::Cheap, ModelTier::Standard, ModelTier::Flagship];

    pub fn as_str(self) -> &'static str {
        match self {
            ModelTier::Cheap => "cheap",
            ModelTier::Standard => "standard",
            ModelTier::Flagship => "flagship",
        }
    }

    pub fn parse(s: &str) -> Option<ModelTier> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cheap" | "fast" | "small" => Some(ModelTier::Cheap),
            "standard" | "default" | "mid" => Some(ModelTier::Standard),
            "flagship" | "best" | "large" => Some(ModelTier::Flagship),
            _ => None,
        }
    }

    /// 向上一档(已是旗舰则不变)。
    pub fn upgrade(self) -> ModelTier {
        match self {
            ModelTier::Cheap => ModelTier::Standard,
            ModelTier::Standard | ModelTier::Flagship => ModelTier::Flagship,
        }
    }

    /// 向下一档(已是便宜则不变)。
    pub fn downgrade(self) -> ModelTier {
        match self {
            ModelTier::Flagship => ModelTier::Standard,
            ModelTier::Cheap | ModelTier::Standard => ModelTier::Cheap,
        }
    }
}

/// 任务复杂度信号。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComplexitySignals {
    /// 输入字符数。
    pub input_chars: usize,
    /// 是否含"重构 / 架构 / 设计"类词。
    pub mentions_architecture: bool,
    /// 是否含"改错别字 / 格式化"类轻任务词。
    pub mentions_trivial: bool,
    /// 是否预期需要多步工具调用。
    pub needs_tools: bool,
    /// 最近连续失败次数。
    #[serde(default)]
    pub recent_failures: u32,
}

impl ComplexitySignals {
    /// 从提示词启发式提取信号。
    pub fn from_prompt(prompt: &str, needs_tools: bool) -> ComplexitySignals {
        let lower = prompt.to_lowercase();
        const ARCH_WORDS: [&str; 8] = [
            "重构",
            "架构",
            "设计",
            "refactor",
            "architect",
            "design",
            "migrate",
            "redesign",
        ];
        const TRIVIAL_WORDS: [&str; 6] = [
            "错别字",
            "格式化",
            "typo",
            "format",
            "rename",
            "rename variable",
        ];
        ComplexitySignals {
            input_chars: prompt.chars().count(),
            mentions_architecture: ARCH_WORDS.iter().any(|w| lower.contains(w)),
            mentions_trivial: TRIVIAL_WORDS.iter().any(|w| lower.contains(w)),
            needs_tools,
            recent_failures: 0,
        }
    }
}

/// 启发式分类:信号 → 档位。
pub fn classify_turn(signals: &ComplexitySignals) -> ModelTier {
    // 失败累积 → 强制升级(模型搞不定,换强的)。
    if signals.recent_failures >= 2 {
        return ModelTier::Flagship;
    }
    if signals.recent_failures == 1 {
        return ModelTier::Standard;
    }
    // 架构类 / 长输入 / 多工具 → 旗舰。
    if signals.mentions_architecture
        || signals.input_chars > 4000
        || (signals.needs_tools && signals.input_chars > 1000)
    {
        return ModelTier::Flagship;
    }
    // 轻任务 → 便宜。
    if signals.mentions_trivial && signals.input_chars < 500 {
        return ModelTier::Cheap;
    }
    ModelTier::Standard
}

/// 分类器函数签名(可注入模型分类器)。
pub type ClassifierFn = Box<dyn Fn(&ComplexitySignals) -> ModelTier + Send + Sync>;

/// 路由决策。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingDecision {
    pub tier: ModelTier,
    /// 是否发生了升级。
    pub upgraded: bool,
    /// 是否因迟滞保持原档(本可降档但没降)。
    pub held_by_hysteresis: bool,
    /// 理由。
    pub reason: String,
}

/// 带迟滞的路由器。
#[derive(Debug)]
pub struct TierRouter {
    current: ModelTier,
    /// 连续判"可降档"的次数。
    downgrade_streak: u32,
    /// 连续多少次判简单才降档。
    hysteresis: u32,
}

impl TierRouter {
    pub fn new(initial: ModelTier, hysteresis: u32) -> TierRouter {
        TierRouter {
            current: initial,
            downgrade_streak: 0,
            hysteresis: if hysteresis == 0 { 2 } else { hysteresis },
        }
    }

    pub fn current(&self) -> ModelTier {
        self.current
    }

    /// 按信号决定本轮档位。
    pub fn route(&mut self, signals: &ComplexitySignals) -> RoutingDecision {
        let target = classify_turn(signals);
        self.route_to(target, signals.recent_failures > 0)
    }

    /// 按目标档位路由(带迟滞)。
    pub fn route_to(&mut self, target: ModelTier, forced_upgrade: bool) -> RoutingDecision {
        if target > self.current {
            // 升级立即生效(保守:宁可多花钱也别卡住)。
            let from = self.current;
            self.current = target;
            self.downgrade_streak = 0;
            return RoutingDecision {
                tier: target,
                upgraded: true,
                held_by_hysteresis: false,
                reason: format!(
                    "升级 {} → {}({})",
                    from.as_str(),
                    target.as_str(),
                    if forced_upgrade {
                        "失败累积"
                    } else {
                        "复杂度"
                    }
                ),
            };
        }
        if target < self.current {
            // 降档需连续 K 次。
            self.downgrade_streak += 1;
            if self.downgrade_streak >= self.hysteresis {
                let from = self.current;
                self.current = target;
                self.downgrade_streak = 0;
                return RoutingDecision {
                    tier: target,
                    upgraded: false,
                    held_by_hysteresis: false,
                    reason: format!(
                        "降档 {} → {}(连续 {} 轮判简单)",
                        from.as_str(),
                        target.as_str(),
                        self.hysteresis
                    ),
                };
            }
            return RoutingDecision {
                tier: self.current,
                upgraded: false,
                held_by_hysteresis: true,
                reason: format!(
                    "迟滞保持 {}(连续 {}/{} 轮判简单)",
                    self.current.as_str(),
                    self.downgrade_streak,
                    self.hysteresis
                ),
            };
        }
        // 同档
        self.downgrade_streak = 0;
        RoutingDecision {
            tier: self.current,
            upgraded: false,
            held_by_hysteresis: false,
            reason: format!("保持 {}", self.current.as_str()),
        }
    }
}

/// 角色绑定(不同角色各配各的模型)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleBinding {
    /// `role -> model spec`。
    pub bindings: BTreeMap<String, String>,
}

impl RoleBinding {
    pub fn new() -> RoleBinding {
        RoleBinding::default()
    }

    /// 绑定角色到模型。
    pub fn bind(&mut self, role: impl Into<String>, model: impl Into<String>) {
        self.bindings.insert(role.into(), model.into());
    }

    /// 查角色模型。
    pub fn model_for(&self, role: &str) -> Option<&str> {
        self.bindings.get(role).map(String::as_str)
    }

    /// 内置默认绑定(压缩用小模型)。
    pub fn defaults() -> RoleBinding {
        let mut b = RoleBinding::new();
        b.bind("main", "flagship");
        b.bind("compact", "cheap");
        b.bind("subagent", "standard");
        b.bind("title", "cheap");
        b
    }
}

/// 模型能力槽位(按能力需求选模型)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilitySlots {
    /// `capability -> model`。
    pub slots: BTreeMap<String, String>,
}

impl CapabilitySlots {
    pub fn new() -> CapabilitySlots {
        CapabilitySlots::default()
    }

    pub fn set(&mut self, capability: impl Into<String>, model: impl Into<String>) {
        self.slots.insert(capability.into(), model.into());
    }

    /// 需要某能力的模型。
    pub fn model_for(&self, capability: &str) -> Option<&str> {
        self.slots.get(capability).map(String::as_str)
    }

    /// 需要多能力时取第一个有绑定的(优先级:调用方按顺序给)。
    pub fn first_available<'a>(
        &'a self,
        capabilities: &'a [&'a str],
    ) -> Option<(&'a str, &'a str)> {
        for cap in capabilities {
            if let Some(model) = self.model_for(cap) {
                return Some((cap, model));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals(chars: usize, arch: bool, trivial: bool, tools: bool) -> ComplexitySignals {
        ComplexitySignals {
            input_chars: chars,
            mentions_architecture: arch,
            mentions_trivial: trivial,
            needs_tools: tools,
            recent_failures: 0,
        }
    }

    #[test]
    fn tier_roundtrip_and_ordering() {
        for t in ModelTier::ALL {
            assert_eq!(ModelTier::parse(t.as_str()), Some(t));
        }
        assert!(ModelTier::Flagship > ModelTier::Standard);
        assert!(ModelTier::Standard > ModelTier::Cheap);
        assert_eq!(ModelTier::parse("fast"), Some(ModelTier::Cheap));
        assert_eq!(ModelTier::parse("bogus"), None);
    }

    #[test]
    fn tier_upgrade_downgrade() {
        assert_eq!(ModelTier::Cheap.upgrade(), ModelTier::Standard);
        assert_eq!(ModelTier::Standard.upgrade(), ModelTier::Flagship);
        assert_eq!(ModelTier::Flagship.upgrade(), ModelTier::Flagship, "已顶格");
        assert_eq!(ModelTier::Flagship.downgrade(), ModelTier::Standard);
        assert_eq!(ModelTier::Cheap.downgrade(), ModelTier::Cheap);
    }

    #[test]
    fn classify_trivial_short_prompt() {
        let s = signals(100, false, true, false);
        assert_eq!(classify_turn(&s), ModelTier::Cheap);
    }

    #[test]
    fn classify_architecture_prompt() {
        let s = signals(500, true, false, false);
        assert_eq!(classify_turn(&s), ModelTier::Flagship);
    }

    #[test]
    fn classify_very_long_prompt() {
        let s = signals(5000, false, false, false);
        assert_eq!(classify_turn(&s), ModelTier::Flagship);
    }

    #[test]
    fn classify_tools_with_long_prompt() {
        let s = signals(1500, false, false, true);
        assert_eq!(classify_turn(&s), ModelTier::Flagship);
    }

    #[test]
    fn classify_default_standard() {
        let s = signals(800, false, false, false);
        assert_eq!(classify_turn(&s), ModelTier::Standard);
    }

    #[test]
    fn classify_failures_force_upgrade() {
        let mut s = signals(100, false, true, false);
        s.recent_failures = 1;
        assert_eq!(classify_turn(&s), ModelTier::Standard);
        s.recent_failures = 2;
        assert_eq!(
            classify_turn(&s),
            ModelTier::Flagship,
            "连续失败 2 次升旗舰"
        );
    }

    #[test]
    fn signals_from_prompt_chinese() {
        let s = ComplexitySignals::from_prompt("帮我重构这个模块", false);
        assert!(s.mentions_architecture);
        let s = ComplexitySignals::from_prompt("修个错别字", false);
        assert!(s.mentions_trivial);
        assert!(!s.mentions_architecture);
    }

    #[test]
    fn router_upgrades_immediately() {
        let mut r = TierRouter::new(ModelTier::Cheap, 2);
        let d = r.route(&signals(500, true, false, false));
        assert!(d.upgraded);
        assert_eq!(d.tier, ModelTier::Flagship);
        assert_eq!(r.current(), ModelTier::Flagship);
    }

    #[test]
    fn router_hysteresis_holds_downgrade() {
        let mut r = TierRouter::new(ModelTier::Flagship, 2);
        // 第一次判简单 → 保持(迟滞)
        let d = r.route(&signals(100, false, true, false));
        assert!(d.held_by_hysteresis);
        assert_eq!(d.tier, ModelTier::Flagship, "第一轮不降");
        // 第二次 → 降档
        let d = r.route(&signals(100, false, true, false));
        assert!(!d.held_by_hysteresis);
        assert_eq!(d.tier, ModelTier::Cheap);
    }

    #[test]
    fn router_upgrade_resets_downgrade_streak() {
        let mut r = TierRouter::new(ModelTier::Flagship, 2);
        // 一次简单(计数 1)
        r.route(&signals(100, false, true, false));
        // 然后升级需求(虽然已在旗舰,但 target 同档 → 重置计数)
        let d = r.route(&signals(500, true, false, false));
        assert_eq!(d.tier, ModelTier::Flagship);
        // 再判简单 → 计数应从 1 开始(因为同档重置了)
        let d = r.route(&signals(100, false, true, false));
        assert!(d.held_by_hysteresis, "计数已重置,不应立即降档");
    }

    #[test]
    fn router_same_tier_stable() {
        let mut r = TierRouter::new(ModelTier::Standard, 2);
        let d = r.route(&signals(800, false, false, false));
        assert_eq!(d.tier, ModelTier::Standard);
        assert!(!d.upgraded);
        assert!(!d.held_by_hysteresis);
    }

    #[test]
    fn router_zero_hysteresis_defaults_to_two() {
        let mut r = TierRouter::new(ModelTier::Flagship, 0);
        r.route(&signals(100, false, true, false));
        let d = r.route(&signals(100, false, true, false));
        assert_eq!(d.tier, ModelTier::Cheap, "0 应视为默认 2");
    }

    #[test]
    fn role_binding() {
        let b = RoleBinding::defaults();
        assert_eq!(b.model_for("main"), Some("flagship"));
        assert_eq!(b.model_for("compact"), Some("cheap"), "压缩用小模型省钱");
        assert_eq!(b.model_for("title"), Some("cheap"));
        assert!(b.model_for("unknown").is_none());
    }

    #[test]
    fn role_binding_custom() {
        let mut b = RoleBinding::new();
        b.bind("reviewer", "claude-opus");
        assert_eq!(b.model_for("reviewer"), Some("claude-opus"));
    }

    #[test]
    fn capability_slots() {
        let mut slots = CapabilitySlots::new();
        slots.set("vision", "gpt-4o");
        slots.set("long_context", "claude-sonnet");
        assert_eq!(slots.model_for("vision"), Some("gpt-4o"));
        // 按优先级取第一个可用的
        let found = slots.first_available(&["audio", "vision", "long_context"]);
        assert_eq!(found, Some(("vision", "gpt-4o")));
        assert!(slots.first_available(&["audio"]).is_none());
    }

    #[test]
    fn routing_decision_serializes() {
        let d = RoutingDecision {
            tier: ModelTier::Standard,
            upgraded: true,
            held_by_hysteresis: false,
            reason: "test".into(),
        };
        let json = serde_json::to_string(&d).expect("serialize");
        let back: RoutingDecision = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, d);
    }

    #[test]
    fn signals_serialize() {
        let s = signals(100, true, false, true);
        let json = serde_json::to_string(&s).expect("serialize");
        let back: ComplexitySignals = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, s);
    }
}
