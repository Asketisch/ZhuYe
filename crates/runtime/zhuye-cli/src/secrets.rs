//! `zhuye secrets` —— 加密密钥存储的 CLI 入口(v1.x 运行时接线)。
//!
//! 底层是 [`zhuye_secrets::SecretStore`](AES-256-GCM + machine key,详见
//! crate 文档的"诚实说明")。存储目录:`<data_dir>/secrets/`
//! (`$ZHUYE_HOME/secrets`,与 config.toml 同根)。
//!
//! 安全约定:
//! - `set` 只从 **stdin** 读明文(argv 里的密钥会进 shell history /
//!   process listing);结尾换行剥掉,内部换行保留。
//! - `list` 只输出条目名,**永不**输出值。
//! - `delete` 默认要确认(`--yes` 跳过),与 `session rm` 同模式。

use anyhow::{Context, bail};
use std::io::Read;
use zhuye_secrets::SecretStore;

/// 解析存储目录并打开 store(`$ZHUYE_HOME` > `$HOME/.zhuye` 口径与
/// config 一致;HOME 缺失时报可读错误)。
fn open_store() -> anyhow::Result<SecretStore> {
    let dir = zhuye_config::data_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve data dir (HOME unset?)"))?
        .join("secrets");
    SecretStore::open(&dir).with_context(|| format!("open secret store at {}", dir.display()))
}

/// `secrets set <name>`:从 stdin 读取明文(整段读到 EOF,剥掉结尾
/// 换行)并加密落盘。空输入报错(防御手滑回车)。
pub fn set(name: &str) -> anyhow::Result<()> {
    let mut plaintext = String::new();
    std::io::stdin()
        .read_to_string(&mut plaintext)
        .context("read secret from stdin")?;
    let plaintext = match plaintext.strip_suffix('\n') {
        Some(s) => s.strip_suffix('\r').unwrap_or(s),
        None => plaintext.as_str(),
    };
    if plaintext.is_empty() {
        bail!("stdin is empty: nothing to store (echo the value into stdin)");
    }
    open_store()?.set(name, plaintext)?;
    println!("secret '{name}' stored");
    Ok(())
}

/// `secrets get <name>`:解密并打到 stdout(末尾带换行)。
/// 条目不存在 → 报错(rc != 0),与 `session show` 缺失路径同语义。
pub fn get(name: &str) -> anyhow::Result<()> {
    match open_store()?.get(name)? {
        Some(v) => {
            println!("{v}");
            Ok(())
        }
        None => bail!("secret not found: {name}"),
    }
}

/// `secrets list`:只列条目名(按字母序),不输出任何值。
pub fn list() -> anyhow::Result<()> {
    let names = open_store()?.list()?;
    if names.is_empty() {
        println!("(no secrets)");
        return Ok(());
    }
    for n in names {
        println!("{n}");
    }
    Ok(())
}

/// `secrets delete <name> [--yes]`:默认确认;拒绝 → "aborted" 且条目
/// 保留;`--yes` 直接删。条目不存在 → 报错。
pub fn delete(name: &str, yes: bool) -> anyhow::Result<()> {
    let store = open_store()?;
    if store.get(name)?.is_none() {
        bail!("secret not found: {name}");
    }
    if !yes {
        eprint!("Delete secret '{name}'? [y/N] ");
        std::io::Write::flush(&mut std::io::stderr()).ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
        if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("aborted");
            return Ok(());
        }
    }
    let removed = store.delete(name)?;
    if removed {
        println!("secret '{name}' deleted");
    }
    Ok(())
}
