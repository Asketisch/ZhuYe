//! `zhuye hooks` —— 内置 hook 的启停持久化(list / enable / disable)。
//!
//! 收编 ReflectDesktop `src-tauri/src/hook_store.rs`。`HookEngine` 的
//! `disabled` 状态此前只活在内存里 —— 运行期 `disable()` 一调就生效,
//! 但进程退出即丢;桌面端自己存了一份 `~/.zhuye/hook_state.json`
//! (`HashMap<String, bool>`),于是 hook 配置出现第三处口径。
//!
//! 本模块把状态收进统一配置的 `[hooks].disabled`(名单形式),写回复用
//! `login::write_config_to`(与 `plugin enable` 同路径 —— 单一 config
//! 写入口)。hook 名口径:
//! - 内置 hook:短名(`search_budget` / `test_runner` / `plan_completion` /
//!   `verification` / `langfuse_tracker`);
//! - 插件 hook:`plugin:<插件id>:<事件>#<序号>`(挂载时由 loader 生成,
//!   确定性,可直接写进名单)。
//!
//! `ls` 从配置构建一个一次性 `HookEngine` 来列注册名与启用状态 ——
//! 与实际运行期同一 `build_engine` 路径,口径不会漂移。

use anyhow::{Context, Result};
use zhuye_config::{default_config_path, load_default};
use zhuye_hooks::config::HooksConfig;

/// `zhuye hooks ls` —— 列全部已注册 hook 与启停状态。
pub fn ls() -> Result<()> {
    let cfg = load_default();
    let engine = HooksConfig::from_reflect_section(&cfg.hooks).build_engine();
    let names = engine.hook_names();
    if names.is_empty() {
        println!("(no hooks registered; check [hooks].enabled in config.toml)");
        return Ok(());
    }
    let (col, state) = ("hook", "state");
    println!("{col:<48}  {state}");
    for name in &names {
        let state = if engine.is_disabled(name) {
            "disabled"
        } else {
            "enabled"
        };
        println!("{name:<48}  {state}");
    }
    Ok(())
}

/// `zhuye hooks disable <name>` —— 把 hook 名加入 `[hooks].disabled`。
pub fn disable(name: &str) -> Result<()> {
    let mut cfg = load_default();
    let section = &mut cfg.hooks;
    let list = section.disabled.get_or_insert_with(Vec::new);
    if list.iter().any(|n| n == name) {
        println!("hook `{name}` already disabled (no-op)");
        return Ok(());
    }
    list.push(name.to_string());
    save(&cfg)?;
    println!("disabled hook `{name}` (config.toml#hooks.disabled)");
    Ok(())
}

/// `zhuye hooks enable <name>` —— 从 `[hooks].disabled` 移除(幂等)。
pub fn enable(name: &str) -> Result<()> {
    let mut cfg = load_default();
    let before = cfg
        .hooks
        .disabled
        .as_ref()
        .map(|l| l.len())
        .unwrap_or_default();
    if let Some(list) = cfg.hooks.disabled.as_mut() {
        list.retain(|n| n != name);
    }
    let after = cfg
        .hooks
        .disabled
        .as_ref()
        .map(|l| l.len())
        .unwrap_or_default();
    if before == after {
        println!("hook `{name}` not disabled (no-op)");
        return Ok(());
    }
    save(&cfg)?;
    println!("enabled hook `{name}` (removed from config.toml#hooks.disabled)");
    Ok(())
}

fn save(cfg: &zhuye_config::ReflectConfig) -> Result<()> {
    let path = default_config_path().context("HOME unset; cannot locate config.toml")?;
    crate::login::write_config_to(cfg, &path)?;
    Ok(())
}
