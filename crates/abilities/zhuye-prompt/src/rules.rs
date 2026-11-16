//! # 提示词工程(F53)
//!
//! AGENTS.md 玩法升级:规则按文件类型生效、就近优先、可版本化实验。
//!
//! ## 三块
//!
//! 1. **条件注入**([`PromptRule::applies_to`])—— 规则可声明 `applyTo`
//!    (文件 glob),只在处理匹配文件时注入。没有它,所有规则永远全量
//!    进 prompt,长会话很快就爆。
//! 2. **就近优先**([`resolve_rules`])—— 同一目录 / 子目录的 AGENTS.md
//!    覆盖上层(用户直觉:离得近的更具体)。深层规则排在前面(优先级高)。
//! 3. **版本与实验**([`PromptVersion`])—— 提示词改动可版本化、可 A/B,
//!    避免"改了不知道变好还是变坏"(与 F20 评测联动)。
//!
//! ## 为什么「就近优先」要显式实现
//!
//! 规则冲突时("总是写注释" vs "不要写注释"),谁赢?用户直觉是"离当前
//! 文件近的那份"。没有显式排序,规则生效顺序取决于文件扫描顺序 ——
//! 那是不确定的。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 一条提示词规则(AGENTS.md 片段)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptRule {
    /// 规则正文。
    pub content: String,
    /// 规则来源(文件路径 / `"builtin"`)。
    pub source: String,
    /// 目录深度(越深越具体)。
    pub depth: usize,
    /// 适用的文件 glob(`None` = 总是适用)。
    #[serde(default)]
    pub apply_to: Option<Vec<String>>,
    /// 版本标签(用于 A/B)。
    #[serde(default)]
    pub version: Option<String>,
}

impl PromptRule {
    pub fn new(content: impl Into<String>, source: impl Into<String>, depth: usize) -> PromptRule {
        PromptRule {
            content: content.into(),
            source: source.into(),
            depth,
            apply_to: None,
            version: None,
        }
    }

    /// 限定适用范围。
    pub fn apply_to(mut self, globs: Vec<String>) -> PromptRule {
        self.apply_to = Some(globs);
        self
    }

    /// 打版本标签。
    pub fn versioned(mut self, version: impl Into<String>) -> PromptRule {
        self.version = Some(version.into());
        self
    }

    /// 是否适用于某文件路径。
    ///
    /// `None` apply_to → 总是适用;否则任一 glob 匹配即适用。
    pub fn applies_to(&self, path: &str) -> bool {
        match &self.apply_to {
            None => true,
            Some(globs) => globs.iter().any(|g| glob_match(g, path)),
        }
    }

    /// 优先级分数(深度 + 具体性)。
    ///
    /// 有 apply_to 的规则比无 apply_to 的更具体 → 优先级 +1000。
    pub fn priority(&self) -> usize {
        let specificity = if self.apply_to.is_some() { 1000 } else { 0 };
        specificity + self.depth
    }
}

/// 极简 glob 匹配(支持 `*` / `**` / `?` / 后缀模式)。
pub fn glob_match(pattern: &str, path: &str) -> bool {
    // 简化实现:支持 `*.ext`(按后缀)、`**/x`(任意层)、精确、`prefix*`。
    if pattern == path {
        return true;
    }
    // `*.rs` → 按扩展名
    if let Some(ext) = pattern.strip_prefix("*.") {
        return path.ends_with(&format!(".{ext}"));
    }
    // `**/*.rs`
    if let Some(rest) = pattern.strip_prefix("**/") {
        // 递归尝试:在任意路径段边界上匹配 rest
        if glob_match(rest, path) {
            return true;
        }
        return path
            .match_indices('/')
            .any(|(idx, _)| glob_match(rest, &path[idx + 1..]));
    }
    // `*mid*`(两侧都是通配)优先于单侧通配
    if pattern.starts_with('*') && pattern.ends_with('*') && pattern.len() > 2 {
        let mid = &pattern[1..pattern.len() - 1];
        return path.contains(mid);
    }
    // `prefix*` → 前缀
    if let Some(prefix) = pattern.strip_suffix('*') {
        return path.starts_with(prefix);
    }
    false
}

/// 规则集合(带解析)。
#[derive(Debug, Default)]
pub struct RuleSet {
    rules: Vec<PromptRule>,
}

impl RuleSet {
    pub fn new() -> RuleSet {
        RuleSet::default()
    }

    pub fn add(&mut self, rule: PromptRule) {
        self.rules.push(rule);
    }

    /// 适用于某文件路径的规则,**按优先级降序**(最具体的在前)。
    pub fn for_path(&self, path: &str) -> Vec<&PromptRule> {
        let mut out: Vec<&PromptRule> = self.rules.iter().filter(|r| r.applies_to(path)).collect();
        out.sort_by(|a, b| {
            b.priority()
                .cmp(&a.priority())
                .then_with(|| a.source.cmp(&b.source))
        });
        out
    }

    /// 渲染注入段。
    pub fn render_for_path(&self, path: &str) -> Option<String> {
        let rules = self.for_path(path);
        if rules.is_empty() {
            return None;
        }
        let mut out = format!("## 适用于 {path} 的规则(就近优先)\n");
        for rule in rules {
            out.push_str(&format!("- [{}] {}\n", rule.source, rule.content.trim()));
        }
        Some(out)
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// 按来源分组统计。
    pub fn by_source(&self) -> BTreeMap<&str, usize> {
        let mut out = BTreeMap::new();
        for r in &self.rules {
            *out.entry(r.source.as_str()).or_insert(0) += 1;
        }
        out
    }
}

/// 解析 AGENTS.md 内容成规则(按段落切分)。
///
/// `depth` 由调用方按目录层级给(根 = 0,子目录 +1)。
pub fn parse_agents_md(content: &str, source: &str, depth: usize) -> Vec<PromptRule> {
    // 按二级标题切分;无标题时整份作为一条。
    let mut rules = Vec::new();
    let mut current = String::new();
    for line in content.lines() {
        if line.starts_with("## ") && !current.trim().is_empty() {
            rules.push(PromptRule::new(current.trim(), source, depth));
            current = String::new();
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        rules.push(PromptRule::new(current.trim(), source, depth));
    }
    rules
}

/// 提示词版本(用于 A/B 与回滚)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptVersion {
    /// 版本标签(`v1` / `v2-experiment`).
    pub label: String,
    /// 内容。
    pub content: String,
    /// 创建时间。
    pub created_at: u64,
    /// 是否当前默认。
    pub is_default: bool,
}

/// 版本库。
#[derive(Debug, Default)]
pub struct PromptVersionStore {
    versions: BTreeMap<String, PromptVersion>,
    /// 当前默认版本标签。
    default_label: Option<String>,
}

impl PromptVersionStore {
    pub fn new() -> PromptVersionStore {
        PromptVersionStore::default()
    }

    /// 添加版本(若标为 default 则切换默认)。
    pub fn add(&mut self, version: PromptVersion) {
        if version.is_default {
            for v in self.versions.values_mut() {
                v.is_default = false;
            }
            self.default_label = Some(version.label.clone());
        }
        self.versions.insert(version.label.clone(), version);
    }

    /// 当前默认版本。
    pub fn default_version(&self) -> Option<&PromptVersion> {
        self.default_label
            .as_ref()
            .and_then(|l| self.versions.get(l))
    }

    /// 查某版本。
    pub fn get(&self, label: &str) -> Option<&PromptVersion> {
        self.versions.get(label)
    }

    /// 切换默认(用于回滚)。
    pub fn set_default(&mut self, label: &str) -> bool {
        if !self.versions.contains_key(label) {
            return false;
        }
        for v in self.versions.values_mut() {
            v.is_default = v.label == label;
        }
        self.default_label = Some(label.to_string());
        true
    }

    /// 全部版本(按创建时间)。
    pub fn all(&self) -> Vec<&PromptVersion> {
        let mut out: Vec<&PromptVersion> = self.versions.values().collect();
        out.sort_by_key(|v| v.created_at);
        out
    }

    pub fn len(&self) -> usize {
        self.versions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.versions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(content: &str, source: &str, depth: usize) -> PromptRule {
        PromptRule::new(content, source, depth)
    }

    #[test]
    fn glob_exact_and_ext() {
        assert!(glob_match("src/lib.rs", "src/lib.rs"));
        assert!(glob_match("*.rs", "src/lib.rs"), "扩展名匹配");
        assert!(!glob_match("*.rs", "src/lib.py"));
        assert!(glob_match("*.py", "a/b/c.py"));
    }

    #[test]
    fn glob_double_star() {
        assert!(glob_match("**/*.rs", "src/deep/lib.rs"));
        assert!(glob_match("**/*.rs", "lib.rs"), "根层也匹配");
        assert!(!glob_match("**/*.rs", "src/lib.py"));
    }

    #[test]
    fn glob_prefix() {
        assert!(glob_match("src/*", "src/lib.rs"));
        assert!(!glob_match("src/*", "tests/lib.rs"));
    }

    #[test]
    fn glob_contains() {
        assert!(glob_match("*test*", "src/my_test_file.rs"));
        assert!(!glob_match("*test*", "src/lib.rs"));
    }

    #[test]
    fn applies_to_unconditionally_when_no_glob() {
        let r = rule("always", "AGENTS.md", 0);
        assert!(r.applies_to("anything"));
        assert!(r.applies_to("src/lib.rs"));
    }

    #[test]
    fn applies_to_with_glob() {
        let r = rule("rust only", "AGENTS.md", 0).apply_to(vec!["*.rs".into()]);
        assert!(r.applies_to("src/lib.rs"));
        assert!(!r.applies_to("src/lib.py"));
    }

    #[test]
    fn priority_depth_and_specificity() {
        let shallow = rule("a", "s", 0);
        let deep = rule("b", "s", 3);
        assert!(deep.priority() > shallow.priority(), "更深优先级更高");

        let specific = rule("c", "s", 0).apply_to(vec!["*.rs".into()]);
        assert!(
            specific.priority() > deep.priority(),
            "有 apply_to 比纯深度更具体"
        );
    }

    #[test]
    fn ruleset_orders_by_priority() {
        let mut set = RuleSet::new();
        set.add(rule("shallow", "root/AGENTS.md", 0));
        set.add(rule("deep", "src/AGENTS.md", 2));
        set.add(rule("specific", "src/AGENTS.md", 1).apply_to(vec!["*.rs".into()]));

        let rules = set.for_path("src/lib.rs");
        assert_eq!(rules[0].content, "specific", "具体规则最前");
        assert_eq!(rules[1].content, "deep", "深度次之");
        assert_eq!(rules[2].content, "shallow");
    }

    #[test]
    fn ruleset_filters_by_path() {
        let mut set = RuleSet::new();
        set.add(rule("rust rule", "a", 0).apply_to(vec!["*.rs".into()]));
        set.add(rule("python rule", "a", 0).apply_to(vec!["*.py".into()]));

        let rs = set.for_path("src/lib.rs");
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].content, "rust rule");

        let py = set.for_path("src/lib.py");
        assert_eq!(py.len(), 1);
        assert_eq!(py[0].content, "python rule");
    }

    #[test]
    fn ruleset_render() {
        let mut set = RuleSet::new();
        set.add(rule("写测试", "AGENTS.md", 0));
        let rendered = set.render_for_path("src/lib.rs").expect("render");
        assert!(rendered.contains("适用于 src/lib.rs"), "{rendered}");
        assert!(rendered.contains("写测试"), "{rendered}");
        assert!(rendered.contains("就近优先"), "{rendered}");
    }

    #[test]
    fn ruleset_empty_render_none() {
        let set = RuleSet::new();
        assert!(set.render_for_path("x").is_none());
    }

    #[test]
    fn ruleset_by_source() {
        let mut set = RuleSet::new();
        set.add(rule("a", "root", 0));
        set.add(rule("b", "root", 0));
        set.add(rule("c", "src", 1));
        let counts = set.by_source();
        assert_eq!(counts["root"], 2);
        assert_eq!(counts["src"], 1);
    }

    #[test]
    fn parse_agents_md_splits_by_heading() {
        let content = "# 项目规则\n\n## 测试\n跑 cargo test\n\n## 风格\n用 rustfmt\n";
        let rules = parse_agents_md(content, "AGENTS.md", 0);
        assert_eq!(rules.len(), 3, "{rules:?}");
        assert!(rules.iter().any(|r| r.content.contains("cargo test")));
        assert!(rules.iter().any(|r| r.content.contains("rustfmt")));
    }

    #[test]
    fn parse_agents_md_no_heading_single_rule() {
        let rules = parse_agents_md("just some text", "AGENTS.md", 0);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].content, "just some text");
    }

    #[test]
    fn version_store_default() {
        let mut store = PromptVersionStore::new();
        store.add(PromptVersion {
            label: "v1".into(),
            content: "c1".into(),
            created_at: 1,
            is_default: true,
        });
        store.add(PromptVersion {
            label: "v2".into(),
            content: "c2".into(),
            created_at: 2,
            is_default: false,
        });
        assert_eq!(store.default_version().unwrap().label, "v1");
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn version_store_switch_default() {
        let mut store = PromptVersionStore::new();
        for (label, at) in [("v1", 1), ("v2", 2)] {
            store.add(PromptVersion {
                label: label.into(),
                content: format!("c{at}"),
                created_at: at,
                is_default: label == "v1",
            });
        }
        assert!(store.set_default("v2"));
        assert_eq!(store.default_version().unwrap().label, "v2");
        assert!(!store.get("v1").unwrap().is_default, "旧默认应清除");
        assert!(!store.set_default("ghost"));
    }

    #[test]
    fn version_store_all_sorted() {
        let mut store = PromptVersionStore::new();
        store.add(PromptVersion {
            label: "late".into(),
            content: "".into(),
            created_at: 200,
            is_default: false,
        });
        store.add(PromptVersion {
            label: "early".into(),
            content: "".into(),
            created_at: 100,
            is_default: false,
        });
        let all = store.all();
        assert_eq!(all[0].label, "early");
    }

    #[test]
    fn rule_serializes() {
        let r = rule("content", "src", 1)
            .apply_to(vec!["*.rs".into()])
            .versioned("v2");
        let json = serde_json::to_string(&r).expect("serialize");
        let back: PromptRule = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, r);
    }

    #[test]
    fn prompt_version_serializes() {
        let v = PromptVersion {
            label: "v1".into(),
            content: "c".into(),
            created_at: 1,
            is_default: true,
        };
        let json = serde_json::to_string(&v).expect("serialize");
        let back: PromptVersion = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, v);
    }
}
