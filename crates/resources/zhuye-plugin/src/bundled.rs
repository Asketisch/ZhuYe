//! 门控型 bundled 插件的统一启用判定与工具注册门构造。
//!
//! 门控型是插件的第三种落地形态(先例 `goal-queue`):能力本体留在
//! 引擎 crate,注册与否由插件启用状态决定 —— 用于「默认开启且必须
//! 保持原工具名 / 审批下限 / 默认行为」的能力(外部用法零影响约束),
//! 区别于 MCP 型的子进程隔离(`mcp__plugin:` 前缀 + Prompt floor)。
//!
//! 判定语义(泛化并兼容 `goal_queue_plugin_enabled` 的旧口径):
//! 1. `[plugins].disabled_plugins` 显式禁用(条目 `name@marketplace`
//!    或裸 `name`,取 `@` 前比对)→ 关(用户显式意图最强);
//! 2. `[plugins].enabled_plugins` 显式启用 → 开;
//! 3. 否则取 [`BUNDLED_PLUGIN_DEFAULTS`] 表的默认开关。
//!
//! 注意:显式启用/禁用**不热重载**(与 goal-queue 语义一致,变更后需
//! 重启会话);热重载仅作用于插件挂载(`reload_plugins`)。

/// bundled 门控插件清单与默认开关。
///
/// 默认开 = 升级用户行为不变(此前无条件内置注册);默认关 = 原本就
/// 需要显式配置 / 安装才启用。**新增门控插件时同步此表**,并在
/// `bundled_tool_gates` 补对应工具门。
pub const BUNDLED_PLUGIN_DEFAULTS: &[(&str, bool)] = &[
    // web_search 工具(Brave 第三方 API)—— 原无条件内置,默认开。
    ("web-search", true),
    // notebook_edit 工具(Jupyter 域)—— 原无条件内置,默认开。
    ("notebook", true),
    // verification / test_runner hooks —— 原 config [hooks] 显式列出
    // 才启用,默认关(双路径:config 显式列出 OR 插件启用)。
    ("verify-hooks", false),
    // lsp 工具 —— 原 [lsp_servers] 配置门控,默认关(双路径)。
    ("lsp", false),
    // goal / queue 工具 —— 维持 v1.10 手动启用语义。
    ("goal-queue", false),
];

/// 查 bundled 插件的默认开关;未登记的名字一律默认关(保守)。
pub fn bundled_plugin_default_on(name: &str) -> bool {
    BUNDLED_PLUGIN_DEFAULTS
        .iter()
        .find(|(n, _)| *n == name)
        .is_some_and(|(_, on)| *on)
}

/// 该名字是否登记为门控型 bundled 插件。
///
/// CLI `plugin disable` 只对 bundled 插件写 `disabled_plugins`(对
/// marketplace / 第三方插件保持旧语义:仅从 enabled 列表移除,不留
/// 禁用痕迹)—— 否则卸载后的 config dump 会出现无意义的残留条目。
pub fn is_bundled_plugin(name: &str) -> bool {
    BUNDLED_PLUGIN_DEFAULTS.iter().any(|(n, _)| *n == name)
}

/// 门控型 bundled 插件的统一启用判定。
///
/// `enabled` / `disabled` 通常来自 `resolve_enabled_plugins` 与
/// `cfg.plugins.disabled_plugins`(`--plugin` 显式覆盖不隐含关闭默认开
/// 插件 —— builtin 行为保持不变,关闭走 disabled 列表)。
pub fn bundled_plugin_enabled(name: &str, enabled: &[String], disabled: &[String]) -> bool {
    if disabled.iter().any(|id| id.split('@').next() == Some(name)) {
        return false;
    }
    enabled.iter().any(|id| id.split('@').next() == Some(name)) || bundled_plugin_default_on(name)
}

/// 由插件启用/禁用列表构造 builtin 工具注册门(exec / serve / lib
/// facade 三条装配路径共用;插件名 → 工具的映射只在此处维护)。
pub fn bundled_tool_gates(
    enabled: &[String],
    disabled: &[String],
) -> zhuye_tools::BundledToolGates {
    zhuye_tools::BundledToolGates::new()
        .web_search(bundled_plugin_enabled("web-search", enabled, disabled))
        .notebook_edit(bundled_plugin_enabled("notebook", enabled, disabled))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认开关表口径:web-search / notebook 开,其余关。
    #[test]
    fn defaults_table_matches_plan() {
        assert!(bundled_plugin_default_on("web-search"));
        assert!(bundled_plugin_default_on("notebook"));
        assert!(!bundled_plugin_default_on("verify-hooks"));
        assert!(!bundled_plugin_default_on("lsp"));
        assert!(!bundled_plugin_default_on("goal-queue"));
        // 未登记名字保守关。
        assert!(!bundled_plugin_default_on("unknown-plugin"));
    }

    /// 判定优先级:显式禁用 > 显式启用 > 默认开关。
    #[test]
    fn disable_beats_enable_beats_default() {
        let enabled = vec!["web-search@inline".to_string()];
        let disabled: Vec<String> = vec![];
        assert!(bundled_plugin_enabled("web-search", &enabled, &disabled));

        // 显式禁用最强:即使同时在 enabled 里也关。
        let disabled = vec!["web-search@user".to_string()];
        assert!(!bundled_plugin_enabled("web-search", &enabled, &disabled));

        // 裸名(不带 @marketplace)同样命中。
        let disabled = vec!["web-search".to_string()];
        assert!(!bundled_plugin_enabled("web-search", &[], &disabled));

        // 默认关插件:空列表 → 关;显式启用 → 开(goal-queue 旧口径)。
        assert!(!bundled_plugin_enabled("goal-queue", &[], &[]));
        assert!(bundled_plugin_enabled(
            "goal-queue",
            &["goal-queue@inline".to_string()],
            &[]
        ));
        // 名字必须精确(@ 前段),前缀不误伤。
        assert!(!bundled_plugin_enabled(
            "goal",
            &["goal-queue@inline".to_string()],
            &[]
        ));
    }

    /// 工具注册门构造:默认开插件在空配置下保持全开(零影响)。
    #[test]
    fn gates_default_all_on_for_default_plugins() {
        let gates = bundled_tool_gates(&[], &[]);
        assert!(gates.web_search);
        assert!(gates.notebook_edit);

        let gates = bundled_tool_gates(&[], &["notebook".to_string()]);
        assert!(gates.web_search);
        assert!(!gates.notebook_edit);
    }
}
