//! # 技能长尾(F58)
//!
//! 四件事:
//!
//! 1. **从 CLI 帮助自动生成技能**([`skill_from_cli_help`])—— 把 `--help`
//!    输出转成技能正文,新工具零成本接入;
//! 2. **技能输出合同**([`OutputContract`])—— 技能声明"必须产出什么",
//!    由 VerificationHook 校验 marker;
//! 3. **价值度量**([`SkillMetrics`])—— 哪些技能真的有用、哪些在拖后腿
//!    (否则技能库只会越堆越胖)。
//!
//! 锁文件不在本模块:F58 与 F27 曾各实现一套结构相同的锁(见下方
//! [`SkillLockfile`]),现已合并为 F27 的单一实现 [`SkillsLock`]。
//!
//! ## 为什么需要「输出合同」
//!
//! 技能是给模型的**指导**,但"指导有没有被执行"没人知道。输出合同要求
//! 技能声明"完成后输出里必须出现某个 marker"(如 `[review-complete]`),
//! VerificationHook 检查它 —— 这是把"建议"变成"可验证"的最小机制。
//!
//! ## 为什么需要价值度量
//!
//! 技能库只增不减会拖垮上下文(每个技能描述都进 catalog)。度量
//! (被加载次数 / 加载后是否成功)让"清理"有依据。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 技能锁条目（F58 的命名，语义等同 F27 的 [`LockedSkill`]）。
///
/// 修复原因:F58（longtail）与 F27（supply_chain）各自实现了一套结构
/// 相同的锁文件（`SkillLockfile`/`SkillLockEntry` 与 `SkillsLock`/
/// `LockedSkill`），字段与 serde 格式完全一致，只是 `verify_frozen`
/// 的返回类型不同（`Vec<String>` vs `Vec<LockMismatch>`）。两套实现
/// 长期并存会让「锁文件」这个概念有两个入口 —— 正是 F58 模块文档里
/// 「与 F27 的 lock 合并成单一入口」那条自述要做却没做的事。
/// 现以类型别名收敛到 F27 的单一实现，`SkillLockfile` / `SkillLockEntry`
/// 两个旧名保留为别名，既有调用方无需改动。
pub type SkillLockEntry = crate::market::supply_chain::LockedSkill;

/// 统一的技能锁文件（F58 的命名，语义等同 F27 的 [`SkillsLock`]）。
pub type SkillLockfile = crate::market::supply_chain::SkillsLock;

/// 从 CLI `--help` 输出生成技能。
///
/// 把帮助文本转成「技能名 + 描述 + 用法正文」。`tool_name` 是 CLI 名
/// (如 `cargo`),`help_text` 是 `--help` 输出。
pub fn skill_from_cli_help(tool_name: &str, help_text: &str) -> CliSkill {
    let name = format!(
        "{}-cli",
        tool_name
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .trim_matches('-')
    );
    // 描述取第一段非空行(通常是 "Usage: ..." 之前的一句话)。
    let description = help_text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.to_ascii_lowercase().starts_with("usage"))
        .unwrap_or("")
        .to_string();
    CliSkill {
        name,
        description,
        body: format!("## {tool_name} CLI\n\n```\n{}\n```\n", help_text.trim_end()),
    }
}

/// CLI 生成的技能。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliSkill {
    pub name: String,
    pub description: String,
    pub body: String,
}

impl CliSkill {
    /// 技能名是否合法。
    pub fn name_is_valid(&self) -> bool {
        !self.name.is_empty()
            && !self.name.starts_with('-')
            && !self.name.ends_with('-')
            && self
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    }

    /// 渲染成 SKILL.md。
    pub fn render_skill_md(&self) -> String {
        format!(
            "---\nname: {}\ndescription: {}\n---\n\n{}\n",
            self.name,
            self.description.replace('\n', " "),
            self.body.trim_end()
        )
    }
}

/// 技能输出合同。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputContract {
    /// 技能名。
    pub skill: String,
    /// 完成后输出中必须出现的 marker(任一即满足)。
    pub required_markers: Vec<String>,
}

impl OutputContract {
    pub fn new(skill: impl Into<String>, markers: Vec<String>) -> OutputContract {
        OutputContract {
            skill: skill.into(),
            required_markers: markers,
        }
    }

    /// 校验输出是否满足合同。
    pub fn verify(&self, output: &str) -> ContractResult {
        if self.required_markers.is_empty() {
            return ContractResult {
                satisfied: true,
                found: Vec::new(),
                missing: Vec::new(),
            };
        }
        let found: Vec<String> = self
            .required_markers
            .iter()
            .filter(|m| output.contains(m.as_str()))
            .cloned()
            .collect();
        let missing: Vec<String> = self
            .required_markers
            .iter()
            .filter(|m| !output.contains(m.as_str()))
            .cloned()
            .collect();
        ContractResult {
            satisfied: !found.is_empty(),
            found,
            missing,
        }
    }
}

/// 合同校验结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractResult {
    pub satisfied: bool,
    pub found: Vec<String>,
    pub missing: Vec<String>,
}

impl ContractResult {
    /// 人读说明。
    pub fn describe(&self) -> String {
        if self.satisfied {
            format!("合同满足(命中 {:?})", self.found)
        } else {
            format!("合同未满足(缺少 {:?})", self.missing)
        }
    }
}

/// 技能价值度量。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SkillMetrics {
    /// 技能名 → (加载次数, 成功次数)。
    pub stats: BTreeMap<String, (u64, u64)>,
}

impl SkillMetrics {
    pub fn new() -> SkillMetrics {
        SkillMetrics::default()
    }

    /// 记录一次加载。
    pub fn record_load(&mut self, skill: &str) {
        self.stats.entry(skill.to_string()).or_insert((0, 0)).0 += 1;
    }

    /// 记录一次成功(加载后任务完成)。
    pub fn record_success(&mut self, skill: &str) {
        let entry = self.stats.entry(skill.to_string()).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += 1;
    }

    /// 某技能的成功率。
    pub fn success_rate(&self, skill: &str) -> Option<f64> {
        self.stats.get(skill).map(|(loads, success)| {
            if *loads == 0 {
                0.0
            } else {
                *success as f64 / *loads as f64
            }
        })
    }

    /// 加载次数。
    pub fn loads(&self, skill: &str) -> u64 {
        self.stats.get(skill).map_or(0, |(l, _)| *l)
    }

    /// 该淘汰的技能(加载少且成功率低)。
    ///
    /// 判据:加载次数 < `min_loads` 或成功率 < `min_rate`。
    pub fn prune_candidates(&self, min_loads: u64, min_rate: f64) -> Vec<String> {
        let mut out: Vec<String> = self
            .stats
            .iter()
            .filter(|(_, (loads, success))| {
                let rate = if *loads == 0 {
                    0.0
                } else {
                    *success as f64 / *loads as f64
                };
                *loads < min_loads || rate < min_rate
            })
            .map(|(name, _)| name.clone())
            .collect();
        out.sort();
        out
    }

    /// 最有价值的技能(按成功率降序,加载次数作次要)。
    pub fn top_skills(&self, k: usize) -> Vec<(&str, f64, u64)> {
        let mut all: Vec<(&str, f64, u64)> = self
            .stats
            .iter()
            .map(|(name, (loads, success))| {
                let rate = if *loads == 0 {
                    0.0
                } else {
                    *success as f64 / *loads as f64
                };
                (name.as_str(), rate, *loads)
            })
            .collect();
        all.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.2.cmp(&a.2))
                .then_with(|| a.0.cmp(b.0))
        });
        all.truncate(k);
        all
    }

    pub fn len(&self) -> usize {
        self.stats.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stats.is_empty()
    }
}

/// 多技能链分解(F58)。
///
/// 一个任务可能需要多个技能协作。`chain` 声明顺序,`decompose` 按
/// 依赖产出执行步骤。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillChain {
    /// 有序技能名。
    pub steps: Vec<String>,
}

impl SkillChain {
    pub fn new(steps: Vec<String>) -> SkillChain {
        SkillChain { steps }
    }

    /// 是否有效(非空、无重复)。
    pub fn validate(&self) -> Result<(), String> {
        if self.steps.is_empty() {
            return Err("skill chain 为空".to_string());
        }
        let mut seen = std::collections::HashSet::new();
        for s in &self.steps {
            if !seen.insert(s) {
                return Err(format!("skill chain 有重复项:{s}"));
            }
        }
        Ok(())
    }

    /// 执行步骤(带序号)。
    pub fn steps_numbered(&self) -> Vec<(usize, &str)> {
        self.steps
            .iter()
            .enumerate()
            .map(|(i, s)| (i + 1, s.as_str()))
            .collect()
    }

    /// 总步数。
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lockfile_upsert_and_get() {
        let mut lock = SkillLockfile::new();
        lock.record(SkillLockEntry {
            name: "s".into(),
            sha256: "h".into(),
            source: "o/r".into(),
            pinned: None,
        });
        assert_eq!(lock.len(), 1);
        assert!(lock.get("s").is_some());
        assert!(lock.remove("s"));
        assert!(lock.is_empty());
    }

    #[test]
    fn lockfile_frozen_verify() {
        let mut lock = SkillLockfile::new();
        lock.record(SkillLockEntry {
            name: "a".into(),
            sha256: "aaa".into(),
            source: "o".into(),
            pinned: None,
        });
        lock.record(SkillLockEntry {
            name: "b".into(),
            sha256: "bbb".into(),
            source: "o".into(),
            pinned: None,
        });
        let mut installed = BTreeMap::new();
        installed.insert("a".to_string(), "aaa".to_string());
        installed.insert("b".to_string(), "wrong".to_string());
        let bad = lock.verify_frozen(&installed);
        let names: Vec<&str> = bad.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["b"]);
    }

    #[test]
    fn lockfile_json_roundtrip() {
        let mut lock = SkillLockfile::new();
        lock.record(SkillLockEntry {
            name: "s".into(),
            sha256: "h".into(),
            source: "o".into(),
            pinned: Some("v1".into()),
        });
        let json = lock.to_json();
        let back = SkillLockfile::from_json(&json).expect("parse");
        assert_eq!(back, lock);
    }

    #[test]
    fn lockfile_bad_json() {
        assert!(SkillLockfile::from_json("nope").is_err());
    }

    #[test]
    fn cli_skill_name_normalized() {
        let skill = skill_from_cli_help("Cargo", "Usage: cargo [opts]");
        assert_eq!(skill.name, "cargo-cli");
        assert!(skill.name_is_valid());
    }

    #[test]
    fn cli_skill_name_strips_odd_chars() {
        let skill = skill_from_cli_help("my.tool!", "help");
        assert!(skill.name_is_valid(), "{}", skill.name);
        assert!(!skill.name.contains('.'), "{}", skill.name);
    }

    #[test]
    fn cli_skill_description_from_help() {
        let help = "A build tool for Rust.\n\nUsage: cargo <command>";
        let skill = skill_from_cli_help("cargo", help);
        assert_eq!(skill.description, "A build tool for Rust.");
        assert!(!skill.description.to_ascii_lowercase().starts_with("usage"));
    }

    #[test]
    fn cli_skill_body_includes_help() {
        let help = "Usage: cargo build";
        let skill = skill_from_cli_help("cargo", help);
        assert!(skill.body.contains("Usage: cargo build"), "{}", skill.body);
    }

    #[test]
    fn cli_skill_render() {
        let skill = skill_from_cli_help("cargo", "A tool\n\nUsage: x");
        let md = skill.render_skill_md();
        assert!(md.starts_with("---\n"), "{md}");
        assert!(md.contains("name: cargo-cli"), "{md}");
        assert!(md.contains("description: A tool"), "{md}");
    }

    #[test]
    fn contract_satisfied() {
        let c = OutputContract::new("review", vec!["[review-complete]".into()]);
        let result = c.verify("done [review-complete]");
        assert!(result.satisfied);
        assert_eq!(result.found.len(), 1);
        assert!(result.describe().contains("合同满足"));
    }

    #[test]
    fn contract_not_satisfied() {
        let c = OutputContract::new("review", vec!["[review-complete]".into()]);
        let result = c.verify("done nothing");
        assert!(!result.satisfied);
        assert_eq!(result.missing, vec!["[review-complete]"]);
        assert!(result.describe().contains("合同未满足"));
    }

    #[test]
    fn contract_any_marker_satisfies() {
        let c = OutputContract::new("s", vec!["[a]".into(), "[b]".into()]);
        assert!(c.verify("only [b] here").satisfied, "任一命中即满足");
    }

    #[test]
    fn contract_empty_markers_always_satisfied() {
        let c = OutputContract::new("s", vec![]);
        assert!(c.verify("anything").satisfied);
    }

    #[test]
    fn metrics_record_and_rate() {
        let mut m = SkillMetrics::new();
        m.record_success("good");
        m.record_success("good");
        m.record_load("bad");
        assert!((m.success_rate("good").unwrap() - 1.0).abs() < 1e-9);
        assert!((m.success_rate("bad").unwrap() - 0.0).abs() < 1e-9);
        assert_eq!(m.loads("good"), 2);
        assert!(m.success_rate("ghost").is_none());
    }

    #[test]
    fn metrics_prune_candidates() {
        let mut m = SkillMetrics::new();
        // 高价值
        for _ in 0..5 {
            m.record_success("valuable");
        }
        // 低价值(加载 1 次未成功)
        m.record_load("useless");
        let prune = m.prune_candidates(3, 0.5);
        assert!(prune.contains(&"useless".to_string()));
        assert!(!prune.contains(&"valuable".to_string()));
    }

    #[test]
    fn metrics_top_skills() {
        let mut m = SkillMetrics::new();
        for _ in 0..3 {
            m.record_success("best");
        }
        m.record_load("worst");
        let top = m.top_skills(1);
        assert_eq!(top[0].0, "best");
        assert!((top[0].1 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn metrics_empty_rate() {
        let m = SkillMetrics::new();
        assert!(m.is_empty());
        assert!(m.top_skills(5).is_empty());
    }

    #[test]
    fn skill_chain_validation() {
        assert!(
            SkillChain::new(vec!["a".into(), "b".into()])
                .validate()
                .is_ok()
        );
        assert!(SkillChain::new(vec![]).validate().is_err());
        assert!(
            SkillChain::new(vec!["a".into(), "a".into()])
                .validate()
                .is_err(),
            "重复项应拒"
        );
    }

    #[test]
    fn skill_chain_numbered() {
        let chain = SkillChain::new(vec!["review".into(), "test".into()]);
        let steps = chain.steps_numbered();
        assert_eq!(steps[0], (1, "review"));
        assert_eq!(steps[1], (2, "test"));
    }

    #[test]
    fn lock_entry_serializes() {
        let e = SkillLockEntry {
            name: "s".into(),
            sha256: "h".into(),
            source: "o".into(),
            pinned: Some("v".into()),
        };
        let json = serde_json::to_string(&e).expect("serialize");
        let back: SkillLockEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, e);
    }
}
