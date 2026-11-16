//! # 记忆自动抽取与生命周期(F17)
//!
//! 会话结束自动提炼有用事实 / 偏好 / 教训进长期记忆,并管理它们的
//! **生命周期**:去重、冲突消解、分层、衰减、驱逐。
//!
//! ## 分层(working / episodic / semantic / procedural)
//!
//! - **working** —— 当前任务临时状态,短命(会话结束即失效);
//! - **episodic** —— 「上次我们这样修好了」这类事件记忆,中等寿命;
//! - **semantic** —— 稳定事实(「这个项目的测试命令是 make test-fast」);
//! - **procedural** —— 做法 / 流程(「改 schema 后要跑 migrate」)。
//!
//! 分层的价值在**衰减速率**:semantic 不该像 working 一样快速过期。
//! [`LayerPolicy`] 给出每层的半衰期与容量上限。
//!
//! ## 抽取(extract)与固化(consolidate)分离
//!
//! - [`extract_candidates`] 从对话里抽候选事实(可用模型,也可用启发式);
//! - [`consolidate`] 把候选合并进既有记忆:去重、冲突(supersession)、
//!   容量驱逐。
//!
//! 分离的理由:抽取可能走模型(有成本、可能失败),固化是纯本地逻辑
//! (必须可靠)。抽取失败不应该让记忆库处于半写状态。
//!
//! ## 注入治理
//!
//! [`injection_budget`] 按层给注入预算 —— working 不该挤占 semantic 的
//! 位置。没有预算的话,一堆会话级临时状态会把长期事实挤出上下文。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// 记忆分层。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLayer {
    /// 当前任务临时状态(最短命)。
    Working,
    /// 事件记忆(「上次这样做」)。
    Episodic,
    /// 稳定事实。
    Semantic,
    /// 做法 / 流程。
    Procedural,
}

impl MemoryLayer {
    pub const ALL: [MemoryLayer; 4] = [
        MemoryLayer::Working,
        MemoryLayer::Episodic,
        MemoryLayer::Semantic,
        MemoryLayer::Procedural,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            MemoryLayer::Working => "working",
            MemoryLayer::Episodic => "episodic",
            MemoryLayer::Semantic => "semantic",
            MemoryLayer::Procedural => "procedural",
        }
    }

    pub fn parse(s: &str) -> Option<MemoryLayer> {
        match s.trim().to_ascii_lowercase().as_str() {
            "working" => Some(MemoryLayer::Working),
            "episodic" => Some(MemoryLayer::Episodic),
            "semantic" => Some(MemoryLayer::Semantic),
            "procedural" => Some(MemoryLayer::Procedural),
            _ => None,
        }
    }
}

/// 每层的衰减与容量策略。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerPolicy {
    /// 半衰期(天)。0 = 不衰减。
    pub half_life_days: f64,
    /// 容量上限(超出按「最旧 + 最低分」驱逐)。
    pub capacity: usize,
    /// 注入预算占比(0.0–1.0,同层内分配)。
    pub injection_weight: f64,
}

impl LayerPolicy {
    /// 默认策略:working 快速过期,semantic/procedural 长期保留。
    pub fn defaults(layer: MemoryLayer) -> LayerPolicy {
        match layer {
            MemoryLayer::Working => LayerPolicy {
                half_life_days: 0.5,
                capacity: 32,
                injection_weight: 0.1,
            },
            MemoryLayer::Episodic => LayerPolicy {
                half_life_days: 14.0,
                capacity: 256,
                injection_weight: 0.2,
            },
            MemoryLayer::Semantic => LayerPolicy {
                half_life_days: 0.0, // 不衰减
                capacity: 512,
                injection_weight: 0.45,
            },
            MemoryLayer::Procedural => LayerPolicy {
                half_life_days: 0.0,
                capacity: 256,
                injection_weight: 0.25,
            },
        }
    }

    /// 计算 `age_days` 天后的保留权重(0.0–1.0)。
    pub fn decay_factor(&self, age_days: f64) -> f64 {
        if self.half_life_days <= 0.0 || age_days <= 0.0 {
            return 1.0;
        }
        0.5f64.powf(age_days / self.half_life_days)
    }

    /// 是否应因衰减而驱逐(权重低于阈值)。
    pub fn should_evict(&self, age_days: f64) -> bool {
        self.decay_factor(age_days) < 0.05
    }
}

/// 一条记忆条目。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryFact {
    /// 唯一键(去重 / supersession 用)。
    pub key: String,
    /// 事实正文。
    pub content: String,
    pub layer: MemoryLayer,
    /// 置信度(0.0–1.0)。低置信可被高置信覆盖。
    pub confidence: f64,
    /// 创建时的年龄(天,由调用方维护)。
    pub age_days: f64,
    /// 被谁取代(冲突消解后保留的历史线索)。
    #[serde(default)]
    pub superseded_by: Option<String>,
}

impl MemoryFact {
    pub fn new(
        key: impl Into<String>,
        content: impl Into<String>,
        layer: MemoryLayer,
        confidence: f64,
    ) -> MemoryFact {
        MemoryFact {
            key: key.into(),
            content: content.into(),
            layer,
            confidence: confidence.clamp(0.0, 1.0),
            age_days: 0.0,
            superseded_by: None,
        }
    }

    /// 当前有效分数(置信度 × 衰减)。
    pub fn effective_score(&self, policy: &LayerPolicy) -> f64 {
        self.confidence * policy.decay_factor(self.age_days)
    }
}

/// 抽取出的候选事实(尚未固化)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateFact {
    pub key: String,
    pub content: String,
    pub layer: MemoryLayer,
    pub confidence: f64,
}

/// 从对话文本启发式抽取候选事实。
///
/// 启发式(不依赖模型):识别「记住 / 以后 / 下次 / 注意 / 总是 / 不要」
/// 这类指令性表述所在的句子,按关键词归类到层。
///
/// 这是**兜底路径**:没有模型时也能抽;有模型时可在其之上覆盖。
pub fn extract_candidates(text: &str) -> Vec<CandidateFact> {
    const CUES: [(&str, MemoryLayer); 8] = [
        ("记住", MemoryLayer::Semantic),
        ("remember", MemoryLayer::Semantic),
        ("以后", MemoryLayer::Procedural),
        ("下次", MemoryLayer::Procedural),
        ("注意", MemoryLayer::Procedural),
        ("总是", MemoryLayer::Procedural),
        ("不要", MemoryLayer::Procedural),
        ("never", MemoryLayer::Procedural),
    ];

    let mut out = Vec::new();
    for sentence in split_sentences(text) {
        let lower = sentence.to_lowercase();
        let Some((cue, layer)) = CUES.iter().find(|(c, _)| lower.contains(c)) else {
            continue;
        };
        let content = sentence.trim().to_string();
        if content.chars().count() < 4 {
            continue;
        }
        // key 用「层 + 归一化正文前 24 字」—— 同义改写可被 consolidate 识别。
        let key = format!("{}:{}", layer.as_str(), normalize_key(&content));
        out.push(CandidateFact {
            key,
            content,
            layer: *layer,
            // 启发式置信度偏低:给模型/用户留出覆盖空间。
            confidence: 0.5,
        });
        let _ = cue;
    }
    out
}

fn split_sentences(text: &str) -> Vec<&str> {
    text.split(['。', '!', '?', '\n', '.', ';'])
        .filter(|s| !s.trim().is_empty())
        .collect()
}

/// 归一化:小写 + 去空白 + 截断。
fn normalize_key(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .take(24)
        .collect::<String>()
        .to_lowercase()
}

/// 固化结果。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsolidateOutcome {
    /// 新增条目数。
    pub added: usize,
    /// 被更高置信度覆盖(supersession)的条目数。
    pub superseded: usize,
    /// 因容量驱逐的条目数。
    pub evicted: usize,
    /// 因衰减过期被驱逐的条目数。
    pub decayed: usize,
}

/// 把候选固化进记忆库。
///
/// 三步:① 去重/冲突(同 key 比置信度)② 容量驱逐 ③ 衰减驱逐。
pub fn consolidate(
    store: &mut Vec<MemoryFact>,
    candidates: Vec<CandidateFact>,
    policies: &HashMap<MemoryLayer, LayerPolicy>,
) -> ConsolidateOutcome {
    let mut outcome = ConsolidateOutcome::default();

    // ① 去重 / 冲突
    for cand in candidates {
        match store.iter_mut().find(|f| f.key == cand.key) {
            Some(existing) => {
                if cand.confidence > existing.confidence + 1e-9 {
                    // 新信息更可信 → 覆盖内容,旧条目记 supersession 线索。
                    existing.content = cand.content;
                    existing.confidence = cand.confidence;
                    existing.age_days = 0.0;
                    existing.superseded_by = None;
                    outcome.superseded += 1;
                }
                // 否则保留既有(低置信候选不覆盖高置信事实)。
            }
            None => {
                store.push(MemoryFact {
                    key: cand.key,
                    content: cand.content,
                    layer: cand.layer,
                    confidence: cand.confidence,
                    age_days: 0.0,
                    superseded_by: None,
                });
                outcome.added += 1;
            }
        }
    }

    // ② 容量驱逐(按层)
    for layer in MemoryLayer::ALL {
        let policy = policies
            .get(&layer)
            .copied()
            .unwrap_or_else(|| LayerPolicy::defaults(layer));
        let mut in_layer: Vec<usize> = store
            .iter()
            .enumerate()
            .filter(|(_, f)| f.layer == layer)
            .map(|(i, _)| i)
            .collect();
        if in_layer.len() <= policy.capacity {
            continue;
        }
        // 分数升序(最低分先驱逐);同分更老的先走。
        in_layer.sort_by(|&a, &b| {
            let sa = store[a].effective_score(&policy);
            let sb = store[b].effective_score(&policy);
            sa.partial_cmp(&sb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    store[b]
                        .age_days
                        .partial_cmp(&store[a].age_days)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });
        let to_remove = in_layer.len() - policy.capacity;
        let mut doomed: Vec<usize> = in_layer.into_iter().take(to_remove).collect();
        doomed.sort_unstable_by(|a, b| b.cmp(a)); // 从后往前删
        for idx in doomed {
            store.remove(idx);
            outcome.evicted += 1;
        }
    }

    // ③ 衰减驱逐(半衰期过短的层里,权重低于阈值的条目)
    let before = store.len();
    store.retain(|f| {
        let policy = policies
            .get(&f.layer)
            .copied()
            .unwrap_or_else(|| LayerPolicy::defaults(f.layer));
        !policy.should_evict(f.age_days)
    });
    outcome.decayed = before - store.len();

    outcome
}

/// 计算注入预算(按层分配 `total` 个条目名额)。
///
/// 返回 `layer -> 名额`。权重归一化后向下取整,余数按权重小数部分分配,
/// 保证总和 == `total`(不浪费预算)。
pub fn injection_budget(
    total: usize,
    policies: &HashMap<MemoryLayer, LayerPolicy>,
) -> HashMap<MemoryLayer, usize> {
    let mut weights: Vec<(MemoryLayer, f64)> = MemoryLayer::ALL
        .iter()
        .map(|&l| {
            let w = policies
                .get(&l)
                .map(|p| p.injection_weight)
                .unwrap_or_else(|| LayerPolicy::defaults(l).injection_weight);
            (l, w.max(0.0))
        })
        .collect();
    let sum: f64 = weights.iter().map(|(_, w)| w).sum();
    if sum <= 0.0 {
        // 无权重 → 平均分。
        let each = total / MemoryLayer::ALL.len();
        return MemoryLayer::ALL.iter().map(|&l| (l, each)).collect();
    }
    let mut out: HashMap<MemoryLayer, usize> = HashMap::new();
    let mut assigned = 0usize;
    let mut remainders: Vec<(MemoryLayer, f64)> = Vec::new();
    for (layer, w) in weights.drain(..) {
        let exact = total as f64 * w / sum;
        let floor = exact.floor() as usize;
        out.insert(layer, floor);
        assigned += floor;
        remainders.push((layer, exact - floor as f64));
    }
    // 余数分配:小数部分大的优先。
    remainders.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut leftover = total.saturating_sub(assigned);
    for (layer, _) in remainders {
        if leftover == 0 {
            break;
        }
        *out.entry(layer).or_insert(0) += 1;
        leftover -= 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policies() -> HashMap<MemoryLayer, LayerPolicy> {
        MemoryLayer::ALL
            .iter()
            .map(|&l| (l, LayerPolicy::defaults(l)))
            .collect()
    }

    #[test]
    fn layer_roundtrip() {
        for l in MemoryLayer::ALL {
            assert_eq!(MemoryLayer::parse(l.as_str()), Some(l));
        }
        assert_eq!(MemoryLayer::parse("bogus"), None);
    }

    #[test]
    fn decay_working_faster_than_semantic() {
        let w = LayerPolicy::defaults(MemoryLayer::Working);
        let s = LayerPolicy::defaults(MemoryLayer::Semantic);
        assert!(w.decay_factor(3.0) < 0.1, "working 3 天后应大幅衰减");
        assert_eq!(s.decay_factor(3650.0), 1.0, "semantic 不衰减");
    }

    #[test]
    fn decay_half_life_exact() {
        let p = LayerPolicy {
            half_life_days: 10.0,
            capacity: 10,
            injection_weight: 0.5,
        };
        assert!((p.decay_factor(10.0) - 0.5).abs() < 1e-9);
        assert!((p.decay_factor(20.0) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn eviction_threshold() {
        let p = LayerPolicy::defaults(MemoryLayer::Working);
        assert!(p.should_evict(30.0));
        assert!(!p.should_evict(0.0));
    }

    #[test]
    fn extract_heuristic_cues() {
        let text =
            "记住这个项目的测试命令是 make test-fast。下次改 schema 后要跑 migrate。今天天气不错。";
        let cands = extract_candidates(text);
        assert!(cands.len() >= 2, "{cands:?}");
        assert!(cands.iter().any(|c| c.layer == MemoryLayer::Semantic));
        assert!(cands.iter().any(|c| c.layer == MemoryLayer::Procedural));
        // 无 cue 的句子不抽。
        assert!(!cands.iter().any(|c| c.content.contains("天气")));
    }

    #[test]
    fn extract_ignores_too_short() {
        let cands = extract_candidates("记住。");
        assert!(cands.is_empty(), "过短句子不抽:{cands:?}");
    }

    #[test]
    fn consolidate_adds_new() {
        let mut store = Vec::new();
        let cands = vec![CandidateFact {
            key: "semantic:test".into(),
            content: "测试命令是 make test-fast".into(),
            layer: MemoryLayer::Semantic,
            confidence: 0.9,
        }];
        let out = consolidate(&mut store, cands, &policies());
        assert_eq!(out.added, 1);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn consolidate_dedups_same_key() {
        let mut store = vec![MemoryFact::new("k", "old", MemoryLayer::Semantic, 0.9)];
        let cands = vec![CandidateFact {
            key: "k".into(),
            content: "old".into(),
            layer: MemoryLayer::Semantic,
            confidence: 0.5,
        }];
        let out = consolidate(&mut store, cands, &policies());
        assert_eq!(out.added, 0, "同 key 不重复添加");
        assert_eq!(store.len(), 1);
        assert_eq!(store[0].content, "old", "低置信不覆盖高置信");
    }

    #[test]
    fn consolidate_supersedes_with_higher_confidence() {
        let mut store = vec![MemoryFact::new("k", "old", MemoryLayer::Semantic, 0.4)];
        let cands = vec![CandidateFact {
            key: "k".into(),
            content: "new".into(),
            layer: MemoryLayer::Semantic,
            confidence: 0.95,
        }];
        let out = consolidate(&mut store, cands, &policies());
        assert_eq!(out.superseded, 1);
        assert_eq!(store[0].content, "new");
        assert_eq!(store[0].confidence, 0.95);
    }

    #[test]
    fn consolidate_evicts_over_capacity() {
        let mut store: Vec<MemoryFact> = (0..5)
            .map(|i| {
                let mut f = MemoryFact::new(
                    format!("w{i}"),
                    format!("working {i}"),
                    MemoryLayer::Working,
                    // 分数递增:i 越大越高
                    0.1 * (i as f64 + 1.0),
                );
                f.age_days = i as f64;
                f
            })
            .collect();
        let mut pols = policies();
        pols.insert(
            MemoryLayer::Working,
            LayerPolicy {
                half_life_days: 0.0, // 关衰减,专测容量
                capacity: 3,
                injection_weight: 0.1,
            },
        );
        let out = consolidate(&mut store, vec![], &pols);
        assert_eq!(out.evicted, 2);
        assert_eq!(store.len(), 3);
        // 最低分的被驱逐(w0/w1)。
        assert!(!store.iter().any(|f| f.key == "w0"), "{store:?}");
    }

    #[test]
    fn consolidate_decay_evicts_old_working() {
        let mut store = vec![{
            let mut f = MemoryFact::new("w", "old working", MemoryLayer::Working, 0.9);
            f.age_days = 100.0;
            f
        }];
        let out = consolidate(&mut store, vec![], &policies());
        assert_eq!(out.decayed, 1);
        assert!(store.is_empty());
    }

    #[test]
    fn budget_sums_to_total() {
        let b = injection_budget(20, &policies());
        let sum: usize = b.values().sum();
        assert_eq!(sum, 20, "{b:?}");
        // semantic 权重最高 → 名额最多。
        assert!(b[&MemoryLayer::Semantic] >= b[&MemoryLayer::Working]);
    }

    #[test]
    fn budget_zero_total() {
        let b = injection_budget(0, &policies());
        assert_eq!(b.values().sum::<usize>(), 0);
    }

    #[test]
    fn budget_with_zero_weights_splits_evenly() {
        let mut pols = HashMap::new();
        for l in MemoryLayer::ALL {
            pols.insert(
                l,
                LayerPolicy {
                    half_life_days: 0.0,
                    capacity: 10,
                    injection_weight: 0.0,
                },
            );
        }
        let b = injection_budget(8, &pols);
        assert_eq!(b.values().sum::<usize>(), 8);
    }

    #[test]
    fn effective_score_combines_confidence_and_decay() {
        let mut f = MemoryFact::new("k", "c", MemoryLayer::Working, 1.0);
        f.age_days = 0.5; // 恰好一个半衰期
        let p = LayerPolicy::defaults(MemoryLayer::Working);
        assert!((f.effective_score(&p) - 0.5).abs() < 1e-6);
    }
}
