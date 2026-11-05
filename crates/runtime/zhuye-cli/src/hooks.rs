//! `zhuye hooks` —— 内置 hook 的启停持久化(list / enable / disable)。
//!
//! 收编 ReflectDesktop `src-tauri/src/hook_store.rs`。`HookEngine` 的
//! `disabled` 状态此前只活在内存里 —— 运行期 `disable()` 一调就生效,
//! 但进程退出即丢;桌面端自己存了一份 `~/.zhuye/hook_state.json`
//! (`HashMap<String, bool>`),于是 hook 配置出现第三处口径。
//!
//! 本模块把状态收进统一配置的 `[hooks].disabled`(名单形式),只修改
//! 该字段,保留其它配置和注释。hook 名口径:
//! - 内置 hook:短名(`search_budget` / `test_runner` / `plan_completion` /
//!   `verification` / `langfuse_tracker`);
//! - 插件 hook:`plugin:<插件id>:<事件>#<序号>`(挂载时由 loader 生成,
//!   确定性,可直接写进名单)。
//!
//! `ls` 从配置构建一个一次性 `HookEngine` 来列注册名与启用状态 ——
//! 与实际运行期同一 `build_engine` 路径,口径不会漂移。

use anyhow::{Context, Result};
use zhuye_config::default_config_path;
use zhuye_hooks::config::HooksConfig;

/// `zhuye hooks ls` —— 列全部已注册 hook 与启停状态。
pub fn ls() -> Result<()> {
    let path = default_config_path().context("cannot locate config.toml")?;
    let (_, cfg) = read_config(&path)?;
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
    if !change_disabled(name, true)? {
        println!("hook `{name}` already disabled (no-op)");
        return Ok(());
    }
    println!("disabled hook `{name}` (config.toml#hooks.disabled)");
    Ok(())
}

/// `zhuye hooks enable <name>` —— 从 `[hooks].disabled` 移除(幂等)。
pub fn enable(name: &str) -> Result<()> {
    if !change_disabled(name, false)? {
        println!("hook `{name}` not disabled (no-op)");
        return Ok(());
    }
    println!("enabled hook `{name}` (removed from config.toml#hooks.disabled)");
    Ok(())
}

fn read_config(path: &std::path::Path) -> Result<(String, zhuye_config::ReflectConfig)> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    let config = zhuye_config::load_from_str(&text)
        .with_context(|| format!("invalid config: {}", path.display()))?;
    Ok((text, config))
}

/// 通过锁保护读取、修改与写入，避免多个 hook 命令互相覆盖。
fn change_disabled(name: &str, disabled: bool) -> Result<bool> {
    let path = default_config_path().context("cannot locate config.toml")?;
    let parent = path.parent().context("config.toml has no parent")?;
    std::fs::create_dir_all(parent)?;
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    fs2::FileExt::lock_exclusive(&lock).context("cannot lock config.toml")?;
    let (text, _) = read_config(&path)?;
    let mut document = text
        .parse::<toml_edit::DocumentMut>()
        .context("cannot edit config.toml")?;
    if document.get("hooks").is_none() {
        if !disabled {
            return Ok(false);
        }
        document["hooks"] = toml_edit::table();
    }
    let hooks = document["hooks"]
        .as_table_like_mut()
        .context("hooks must be a table")?;
    if hooks.get("disabled").is_none() {
        if !disabled {
            return Ok(false);
        }
        hooks.insert("disabled", toml_edit::value(toml_edit::Array::new()));
    }
    let list = hooks
        .get_mut("disabled")
        .and_then(toml_edit::Item::as_array_mut)
        .context("hooks.disabled must be an array")?;
    let present = list.iter().any(|value| value.as_str() == Some(name));
    if present == disabled {
        return Ok(false);
    }
    if disabled {
        list.push(name);
    } else {
        list.retain(|value| value.as_str() != Some(name));
    }
    zhuye_fs::write_atomic(&path, document.to_string().as_bytes())
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(true)
}
