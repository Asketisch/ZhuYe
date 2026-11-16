//! # 用户画像与 Persona(F19)
//!
//! 两件事:
//!
//! 1. **用户建模** —— 从使用信号学习偏好(「他喜欢简短回答」「这个项目
//!    用 pnpm 不用 npm」),并带**审批门**:学到的偏好默认 pending,
//!    用户确认后才生效(防止模型把偶然行为当稳定偏好)。
//! 2. **Persona / SOUL** —— 给 agent 配人格档案(语气、价值观、边界),
//!    注入 system prompt。
//!
//! ## 为什么偏好要过审批门
//!
//! 自动学偏好最容易翻车的地方:**一次性的临时选择被当成长期偏好**。
//! 用户这次要「详细解释」,不代表以后都要。所以:
//!
//! - 观察到的偏好先进 [`PreferenceStore`] 的 `pending` 区;
//! - 累计出现次数达阈值才「建议确认」;
//! - 用户接受后才进 `active`,参与注入。
//!
//! 这与 F18 的技能 review 门同构 —— 自动学习的东西必须有确认环节。
//!
//! ## Persona 与偏好的关系
//!
//! Persona 是**用户写给 agent 的**;偏好是**agent 从行为里学的**。
//! 两者都进 system prompt,但来源与权威性不同:persona 优先级更高,
//! 冲突时 persona 胜(用户显式声明 > 模型推断)。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// 偏好状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreferenceStatus {
    /// 观察到但未确认。
    #[default]
    Pending,
    /// 用户已确认,参与注入。
    Active,
    /// 用户已拒绝。
    Rejected,
}

impl PreferenceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PreferenceStatus::Pending => "pending",
            PreferenceStatus::Active => "active",
            PreferenceStatus::Rejected => "rejected",
        }
    }
}

/// 一条用户偏好。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preference {
    /// 偏好键(`verbosity` / `package_manager` / `test_command` …)。
    pub key: String,
    /// 偏好值。
    pub value: String,
    /// 观察到的次数。
    pub observations: usize,
    pub status: PreferenceStatus,
    /// 证据(观察来源,便于用户判断是否接受)。
    pub evidence: Vec<String>,
}

impl Preference {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Preference {
        Preference {
            key: key.into(),
            value: value.into(),
            observations: 1,
            status: PreferenceStatus::Pending,
            evidence: Vec::new(),
        }
    }

    pub fn with_evidence(mut self, evidence: impl Into<String>) -> Preference {
        self.evidence.push(evidence.into());
        self
    }

    /// 是否达到「建议确认」阈值。
    pub fn is_ready(&self, min_observations: usize) -> bool {
        self.status == PreferenceStatus::Pending && self.observations >= min_observations
    }
}

/// 偏好库。
#[derive(Debug, Default)]
pub struct PreferenceStore {
    prefs: HashMap<String, Preference>,
}

impl PreferenceStore {
    pub fn new() -> PreferenceStore {
        PreferenceStore::default()
    }

    /// 观察到一个偏好。同 key 同值累加次数;同 key 不同值 → 以新值为准
    /// 重置计数(偏好变了,旧证据不再适用)。
    pub fn observe(&mut self, pref: Preference) {
        match self.prefs.get_mut(&pref.key) {
            Some(existing) => {
                if existing.status == PreferenceStatus::Rejected {
                    return; // 拒绝过的不再学习
                }
                if existing.value == pref.value {
                    existing.observations += pref.observations;
                    existing.evidence.extend(pref.evidence);
                    // 证据封顶,防无界增长。
                    if existing.evidence.len() > 20 {
                        let excess = existing.evidence.len() - 20;
                        existing.evidence.drain(0..excess);
                    }
                } else {
                    // 值变了:重置为新的 pending 观察。
                    existing.value = pref.value;
                    existing.observations = pref.observations;
                    existing.evidence = pref.evidence;
                    existing.status = PreferenceStatus::Pending;
                }
            }
            None => {
                self.prefs.insert(pref.key.clone(), pref);
            }
        }
    }

    /// 待确认列表(按观察次数降序)。
    pub fn pending(&self) -> Vec<&Preference> {
        let mut out: Vec<&Preference> = self
            .prefs
            .values()
            .filter(|p| p.status == PreferenceStatus::Pending)
            .collect();
        out.sort_by(|a, b| {
            b.observations
                .cmp(&a.observations)
                .then_with(|| a.key.cmp(&b.key))
        });
        out
    }

    /// 达到阈值的待确认项。
    pub fn ready(&self, min_observations: usize) -> Vec<&Preference> {
        self.pending()
            .into_iter()
            .filter(|p| p.is_ready(min_observations))
            .collect()
    }

    /// 生效中的偏好(参与注入)。
    pub fn active(&self) -> Vec<&Preference> {
        let mut out: Vec<&Preference> = self
            .prefs
            .values()
            .filter(|p| p.status == PreferenceStatus::Active)
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }

    /// 用户确认。
    pub fn accept(&mut self, key: &str) -> bool {
        if let Some(p) = self.prefs.get_mut(key) {
            p.status = PreferenceStatus::Active;
            return true;
        }
        false
    }

    /// 用户拒绝(不再学习)。
    pub fn reject(&mut self, key: &str) -> bool {
        if let Some(p) = self.prefs.get_mut(key) {
            p.status = PreferenceStatus::Rejected;
            return true;
        }
        false
    }

    /// 删除(用户「忘掉这条」)。
    pub fn forget(&mut self, key: &str) -> bool {
        self.prefs.remove(key).is_some()
    }

    pub fn get(&self, key: &str) -> Option<&Preference> {
        self.prefs.get(key)
    }

    pub fn len(&self) -> usize {
        self.prefs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.prefs.is_empty()
    }

    /// 渲染成注入 system prompt 的段落(仅 active)。
    ///
    /// 空库返回 `None`(不注入空段落,避免污染提示词)。
    pub fn render_prompt_section(&self) -> Option<String> {
        let active = self.active();
        if active.is_empty() {
            return None;
        }
        let mut out = String::from("## 用户偏好(从使用中学习,已确认)\n");
        for p in active {
            out.push_str(&format!("- {}: {}\n", p.key, p.value));
        }
        Some(out)
    }
}

/// Persona / SOUL 档案。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Persona {
    /// 名字。
    pub name: String,
    /// 语气 / 风格描述。
    pub voice: String,
    /// 价值观 / 边界(不可逾越的原则)。
    #[serde(default)]
    pub principles: Vec<String>,
    /// 自由格式的补充(SOUL 正文)。
    #[serde(default)]
    pub soul: String,
}

impl Persona {
    pub fn new(name: impl Into<String>, voice: impl Into<String>) -> Persona {
        Persona {
            name: name.into(),
            voice: voice.into(),
            principles: Vec::new(),
            soul: String::new(),
        }
    }

    pub fn with_principles(mut self, principles: Vec<String>) -> Persona {
        self.principles = principles;
        self
    }

    pub fn with_soul(mut self, soul: impl Into<String>) -> Persona {
        self.soul = soul.into();
        self
    }

    /// 是否有实质内容(全空则不注入)。
    pub fn is_empty(&self) -> bool {
        self.name.trim().is_empty()
            && self.voice.trim().is_empty()
            && self.principles.is_empty()
            && self.soul.trim().is_empty()
    }

    /// 渲染成 system prompt 段落。
    pub fn render_prompt_section(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut out = String::from("## Persona\n");
        if !self.name.trim().is_empty() {
            out.push_str(&format!("名字:{}\n", self.name.trim()));
        }
        if !self.voice.trim().is_empty() {
            out.push_str(&format!("语气:{}\n", self.voice.trim()));
        }
        if !self.principles.is_empty() {
            out.push_str("原则:\n");
            for p in &self.principles {
                out.push_str(&format!("- {}\n", p.trim()));
            }
        }
        if !self.soul.trim().is_empty() {
            out.push_str("\n");
            out.push_str(self.soul.trim());
            out.push('\n');
        }
        Some(out)
    }
}

/// 组装最终注入段:persona 优先(在前),偏好其次。
///
/// 冲突时的权威顺序由**位置**表达:persona 在前,模型读到冲突时会以
/// 更靠前、更明确的用户显式声明为准。
pub fn compose_prompt_sections(persona: &Persona, prefs: &PreferenceStore) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(p) = persona.render_prompt_section() {
        parts.push(p);
    }
    if let Some(p) = prefs.render_prompt_section() {
        parts.push(p);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_accumulates_same_value() {
        let mut s = PreferenceStore::new();
        s.observe(Preference::new("verbosity", "concise"));
        s.observe(Preference::new("verbosity", "concise"));
        let p = s.get("verbosity").expect("pref");
        assert_eq!(p.observations, 2);
        assert_eq!(p.status, PreferenceStatus::Pending);
    }

    #[test]
    fn changing_value_resets_to_pending() {
        let mut s = PreferenceStore::new();
        s.observe(Preference::new("verbosity", "concise"));
        s.accept("verbosity");
        assert_eq!(s.get("verbosity").unwrap().status, PreferenceStatus::Active);
        // 值变了 → 重新变 pending。
        s.observe(Preference::new("verbosity", "detailed"));
        let p = s.get("verbosity").unwrap();
        assert_eq!(p.value, "detailed");
        assert_eq!(p.status, PreferenceStatus::Pending, "偏好变化应重新确认");
        assert_eq!(p.observations, 1);
    }

    #[test]
    fn rejected_not_relearned() {
        let mut s = PreferenceStore::new();
        s.observe(Preference::new("verbosity", "concise"));
        s.reject("verbosity");
        s.observe(Preference::new("verbosity", "concise"));
        assert_eq!(
            s.get("verbosity").unwrap().observations,
            1,
            "拒绝后不再累加"
        );
        assert_eq!(
            s.get("verbosity").unwrap().status,
            PreferenceStatus::Rejected
        );
    }

    #[test]
    fn ready_threshold() {
        let mut s = PreferenceStore::new();
        s.observe(Preference::new("a", "x"));
        s.observe(Preference::new("b", "y"));
        s.observe(Preference::new("b", "y"));
        s.observe(Preference::new("b", "y"));
        let ready = s.ready(3);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].key, "b");
    }

    #[test]
    fn active_only_injected() {
        let mut s = PreferenceStore::new();
        s.observe(Preference::new("verbosity", "concise"));
        s.observe(Preference::new("pm", "pnpm"));
        assert!(s.render_prompt_section().is_none(), "无 active 不注入");
        s.accept("verbosity");
        let section = s.render_prompt_section().expect("section");
        assert!(section.contains("verbosity"), "{section}");
        assert!(!section.contains("pnpm"), "pending 不该注入:{section}");
    }

    #[test]
    fn forget_removes() {
        let mut s = PreferenceStore::new();
        s.observe(Preference::new("k", "v"));
        assert!(s.forget("k"));
        assert!(!s.forget("k"));
        assert!(s.is_empty());
    }

    #[test]
    fn accept_reject_unknown_returns_false() {
        let mut s = PreferenceStore::new();
        assert!(!s.accept("ghost"));
        assert!(!s.reject("ghost"));
    }

    #[test]
    fn evidence_capped() {
        let mut s = PreferenceStore::new();
        for i in 0..30 {
            s.observe(Preference::new("k", "v").with_evidence(format!("e{i}")));
        }
        assert!(s.get("k").unwrap().evidence.len() <= 20, "证据应封顶");
    }

    #[test]
    fn persona_empty_not_injected() {
        let p = Persona::default();
        assert!(p.is_empty());
        assert!(p.render_prompt_section().is_none());
    }

    #[test]
    fn persona_renders_all_parts() {
        let p = Persona::new("烛夜", "简洁直接")
            .with_principles(vec!["不编造事实".into()])
            .with_soul("你是本地 coding agent。");
        let section = p.render_prompt_section().expect("section");
        assert!(section.contains("烛夜"), "{section}");
        assert!(section.contains("简洁直接"), "{section}");
        assert!(section.contains("不编造事实"), "{section}");
        assert!(section.contains("本地 coding agent"), "{section}");
    }

    #[test]
    fn compose_persona_before_prefs() {
        let mut s = PreferenceStore::new();
        s.observe(Preference::new("verbosity", "concise"));
        s.accept("verbosity");
        let persona = Persona::new("烛夜", "简洁");
        let composed = compose_prompt_sections(&persona, &s).expect("composed");
        let persona_idx = composed.find("Persona").expect("persona");
        let pref_idx = composed.find("用户偏好").expect("prefs");
        assert!(persona_idx < pref_idx, "persona 应在偏好之前(权威顺序)");
    }

    #[test]
    fn compose_empty_returns_none() {
        assert!(compose_prompt_sections(&Persona::default(), &PreferenceStore::new()).is_none());
    }

    #[test]
    fn persona_serializes() {
        let p = Persona::new("x", "y").with_principles(vec!["p".into()]);
        let json = serde_json::to_string(&p).expect("serialize");
        let back: Persona = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, p);
    }
}
