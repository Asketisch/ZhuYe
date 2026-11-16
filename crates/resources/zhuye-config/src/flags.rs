//! # Feature flag 注册表(F70)
//!
//! 新功能用开关控制、按比例放量。此前 15 个 flag 恒关且没有切换途径 ——
//! 用户既看不到实验特性,也没法关掉不喜欢的。
//!
//! ## 三档发布阶段
//!
//! - `Dogfood` —— 只有开发者(或显式开启)能用;
//! - `Preview` —— 愿意尝鲜的用户可用;
//! - `Release` —— 默认开启。
//!
//! ## 生效优先级
//!
//! `env` > `config` > 阶段默认。
//!
//! - env `ZHUYE_FLAG_<NAME>=1/0` 可临时压开/压关(调试、CI);
//! - config `[feature_flags] name = true/false` 是用户持久选择;
//! - 都没表态 → 按阶段默认(`Release` 开,其余关)。
//!
//! ## 为什么要有「注册表」而不是散落布尔
//!
//! 散落的 `if cfg.some_flag` 无法回答「现在有哪些实验特性」。集中注册表
//! 让 `/flags` 能列出全部、让 UI 能渲染开关、让灰度可以按阶段统一调整。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 发布阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseStage {
    /// 内部试用(默认关闭)。
    #[default]
    Dogfood,
    /// 预览(默认关闭,但用户可开)。
    Preview,
    /// 正式(默认开启)。
    Release,
}

impl ReleaseStage {
    pub fn as_str(self) -> &'static str {
        match self {
            ReleaseStage::Dogfood => "dogfood",
            ReleaseStage::Preview => "preview",
            ReleaseStage::Release => "release",
        }
    }

    pub fn parse(s: &str) -> Option<ReleaseStage> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dogfood" => Some(ReleaseStage::Dogfood),
            "preview" => Some(ReleaseStage::Preview),
            "release" => Some(ReleaseStage::Release),
            _ => None,
        }
    }

    /// 该阶段下 flag 的默认值。
    pub fn default_enabled(self) -> bool {
        matches!(self, ReleaseStage::Release)
    }
}

/// 一个 feature flag 的声明。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlagDecl {
    /// 稳定名(`snake_case`)。
    pub name: String,
    /// 一句话说明(UI 展示)。
    pub description: String,
    pub stage: ReleaseStage,
    /// 是否允许用户关闭(某些实验特性必须开着才能用,不允许关)。
    #[serde(default = "default_true")]
    pub user_can_disable: bool,
}

fn default_true() -> bool {
    true
}

impl FlagDecl {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        stage: ReleaseStage,
    ) -> FlagDecl {
        FlagDecl {
            name: name.into(),
            description: description.into(),
            stage,
            user_can_disable: true,
        }
    }

    /// 声明不可关闭。
    pub fn locked(mut self) -> FlagDecl {
        self.user_can_disable = false;
        self
    }

    /// env 变量名(`ZHUYE_FLAG_<UPPER_SNAKE>`)。
    pub fn env_var(&self) -> String {
        format!(
            "ZHUYE_FLAG_{}",
            self.name.to_ascii_uppercase().replace('-', "_")
        )
    }
}

/// flag 的当前状态(供 `/flags` 展示)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlagState {
    pub name: String,
    pub description: String,
    pub stage: ReleaseStage,
    pub enabled: bool,
    /// 生效来源(`"env"` / `"config"` / `"stage_default"`)。
    pub source: String,
    pub user_can_disable: bool,
}

/// flag 注册表。
#[derive(Debug, Default)]
pub struct FlagRegistry {
    decls: BTreeMap<String, FlagDecl>,
}

impl FlagRegistry {
    pub fn new() -> FlagRegistry {
        FlagRegistry::default()
    }

    /// 注册(同名覆盖 —— 便于插件覆盖内置声明)。
    pub fn register(&mut self, decl: FlagDecl) {
        self.decls.insert(decl.name.clone(), decl);
    }

    /// 批量注册。
    pub fn register_all(&mut self, decls: Vec<FlagDecl>) {
        for d in decls {
            self.register(d);
        }
    }

    /// 全部 flag 名(排序)。
    pub fn names(&self) -> Vec<&str> {
        self.decls.keys().map(String::as_str).collect()
    }

    pub fn len(&self) -> usize {
        self.decls.len()
    }

    pub fn is_empty(&self) -> bool {
        self.decls.is_empty()
    }

    /// 查声明。
    pub fn get(&self, name: &str) -> Option<&FlagDecl> {
        self.decls.get(name)
    }

    /// 判断 flag 是否启用。
    ///
    /// `config` 是用户配置里的显式值;`env_lookup` 是 env 读取回调
    /// (便于测试注入,不直接读进程环境)。
    pub fn is_enabled<F>(&self, name: &str, config: &BTreeMap<String, bool>, env_lookup: F) -> bool
    where
        F: Fn(&str) -> Option<String>,
    {
        let Some(decl) = self.decls.get(name) else {
            // 未注册的 flag 恒关(拼错名字不会意外开启功能)。
            return false;
        };
        // ① env 优先(locked flag 忽略"关闭"意图,与 config 层一致)。
        if let Some(raw) = env_lookup(&decl.env_var()) {
            match raw.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => return true,
                "0" | "false" | "no" | "off" => {
                    if !decl.user_can_disable {
                        return true;
                    }
                    return false;
                }
                _ => {} // 无效值忽略,落到下一层
            }
        }
        // ② config
        if let Some(&v) = config.get(name) {
            // 不允许关闭的 flag 忽略用户关闭意图。
            if !v && !decl.user_can_disable {
                return true;
            }
            return v;
        }
        // ③ 阶段默认
        decl.stage.default_enabled()
    }

    /// 全部 flag 的当前状态(UI 列表)。
    pub fn states<F>(&self, config: &BTreeMap<String, bool>, env_lookup: F) -> Vec<FlagState>
    where
        F: Fn(&str) -> Option<String>,
    {
        self.decls
            .values()
            .map(|decl| {
                let (enabled, source) = self.resolve_with_source(decl, config, &env_lookup);
                FlagState {
                    name: decl.name.clone(),
                    description: decl.description.clone(),
                    stage: decl.stage,
                    enabled,
                    source,
                    user_can_disable: decl.user_can_disable,
                }
            })
            .collect()
    }

    fn resolve_with_source<F>(
        &self,
        decl: &FlagDecl,
        config: &BTreeMap<String, bool>,
        env_lookup: &F,
    ) -> (bool, String)
    where
        F: Fn(&str) -> Option<String>,
    {
        if let Some(raw) = env_lookup(&decl.env_var()) {
            match raw.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => return (true, "env".to_string()),
                "0" | "false" | "no" | "off" => {
                    if !decl.user_can_disable {
                        return (true, "locked".to_string());
                    }
                    return (false, "env".to_string());
                }
                _ => {}
            }
        }
        if let Some(&v) = config.get(&decl.name) {
            if !v && !decl.user_can_disable {
                return (true, "locked".to_string());
            }
            return (v, "config".to_string());
        }
        (decl.stage.default_enabled(), "stage_default".to_string())
    }

    /// 按阶段统计(灰度进度视图)。
    pub fn count_by_stage(&self) -> BTreeMap<&'static str, usize> {
        let mut out = BTreeMap::new();
        for decl in self.decls.values() {
            *out.entry(decl.stage.as_str()).or_insert(0) += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> FlagRegistry {
        let mut r = FlagRegistry::new();
        r.register_all(vec![
            FlagDecl::new("code_mode", "Code Mode", ReleaseStage::Preview),
            FlagDecl::new("stable_feature", "stable", ReleaseStage::Release),
            FlagDecl::new("experiment", "exp", ReleaseStage::Dogfood),
            FlagDecl::new("must_on", "locked", ReleaseStage::Preview).locked(),
        ]);
        r
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn stage_defaults() {
        let r = registry();
        let cfg = BTreeMap::new();
        assert!(!r.is_enabled("code_mode", &cfg, no_env), "preview 默认关");
        assert!(
            r.is_enabled("stable_feature", &cfg, no_env),
            "release 默认开"
        );
        assert!(!r.is_enabled("experiment", &cfg, no_env), "dogfood 默认关");
    }

    #[test]
    fn config_overrides_stage() {
        let r = registry();
        let mut cfg = BTreeMap::new();
        cfg.insert("code_mode".to_string(), true);
        assert!(r.is_enabled("code_mode", &cfg, no_env));
        cfg.insert("stable_feature".to_string(), false);
        assert!(
            !r.is_enabled("stable_feature", &cfg, no_env),
            "release 可被关"
        );
    }

    #[test]
    fn env_overrides_config() {
        let r = registry();
        let mut cfg = BTreeMap::new();
        cfg.insert("code_mode".to_string(), false);
        let env = |name: &str| {
            if name == "ZHUYE_FLAG_CODE_MODE" {
                Some("1".to_string())
            } else {
                None
            }
        };
        assert!(r.is_enabled("code_mode", &cfg, env), "env 应压过 config");
    }

    #[test]
    fn env_zero_disables() {
        let r = registry();
        let cfg = BTreeMap::new();
        let env = |name: &str| {
            if name == "ZHUYE_FLAG_STABLE_FEATURE" {
                Some("0".to_string())
            } else {
                None
            }
        };
        assert!(!r.is_enabled("stable_feature", &cfg, env));
    }

    #[test]
    fn invalid_env_value_falls_through() {
        let r = registry();
        let cfg = BTreeMap::new();
        let env = |_: &str| Some("maybe".to_string());
        // 无效值应忽略 → 落到阶段默认(release 开)。
        assert!(r.is_enabled("stable_feature", &cfg, env));
    }

    #[test]
    fn unknown_flag_is_disabled() {
        let r = registry();
        assert!(
            !r.is_enabled("typo_flag", &BTreeMap::new(), no_env),
            "拼错名字不应开启"
        );
    }

    #[test]
    fn locked_flag_cannot_be_disabled_by_config() {
        let r = registry();
        let mut cfg = BTreeMap::new();
        cfg.insert("must_on".to_string(), false);
        assert!(
            r.is_enabled("must_on", &cfg, no_env),
            "locked flag 忽略用户关闭"
        );
    }

    #[test]
    fn locked_flag_cannot_be_disabled_by_env() {
        let r = registry();
        let env = |name: &str| {
            if name == "ZHUYE_FLAG_MUST_ON" {
                Some("0".to_string())
            } else {
                None
            }
        };
        assert!(r.is_enabled("must_on", &BTreeMap::new(), env));
    }

    #[test]
    fn env_var_naming() {
        let d = FlagDecl::new("code-mode", "x", ReleaseStage::Preview);
        assert_eq!(d.env_var(), "ZHUYE_FLAG_CODE_MODE");
    }

    #[test]
    fn states_report_source() {
        let r = registry();
        let mut cfg = BTreeMap::new();
        cfg.insert("code_mode".to_string(), true);
        let env = |name: &str| {
            if name == "ZHUYE_FLAG_EXPERIMENT" {
                Some("1".to_string())
            } else {
                None
            }
        };
        let states = r.states(&cfg, env);
        let by_name: BTreeMap<&str, &FlagState> =
            states.iter().map(|s| (s.name.as_str(), s)).collect();
        assert_eq!(by_name["code_mode"].source, "config");
        assert_eq!(by_name["experiment"].source, "env");
        assert_eq!(by_name["stable_feature"].source, "stage_default");
        assert_eq!(by_name["must_on"].source, "stage_default");
    }

    #[test]
    fn locked_state_source_when_user_disables() {
        let r = registry();
        let mut cfg = BTreeMap::new();
        cfg.insert("must_on".to_string(), false);
        let states = r.states(&cfg, no_env);
        let locked = states.iter().find(|s| s.name == "must_on").expect("state");
        assert!(locked.enabled);
        assert_eq!(locked.source, "locked");
    }

    #[test]
    fn count_by_stage() {
        let r = registry();
        let counts = r.count_by_stage();
        assert_eq!(counts["preview"], 2, "code_mode + must_on");
        assert_eq!(counts["release"], 1);
        assert_eq!(counts["dogfood"], 1);
    }

    #[test]
    fn register_overwrites_same_name() {
        let mut r = FlagRegistry::new();
        r.register(FlagDecl::new("f", "v1", ReleaseStage::Dogfood));
        r.register(FlagDecl::new("f", "v2", ReleaseStage::Release));
        assert_eq!(r.len(), 1);
        assert_eq!(r.get("f").unwrap().stage, ReleaseStage::Release);
    }

    #[test]
    fn names_sorted() {
        let r = registry();
        let names = r.names();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[test]
    fn stage_parse_roundtrip() {
        for s in [
            ReleaseStage::Dogfood,
            ReleaseStage::Preview,
            ReleaseStage::Release,
        ] {
            assert_eq!(ReleaseStage::parse(s.as_str()), Some(s));
        }
        assert_eq!(ReleaseStage::parse("bogus"), None);
    }

    #[test]
    fn flag_state_serializes() {
        let state = FlagState {
            name: "x".into(),
            description: "d".into(),
            stage: ReleaseStage::Preview,
            enabled: true,
            source: "config".into(),
            user_can_disable: true,
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let back: FlagState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, state);
    }
}
