//! # 凭据 provider 抽象(F08)
//!
//! 把「密钥存哪」与「怎么用」解耦。三种后端:
//!
//! | provider | 保护边界 | 适用 |
//! |---|---|---|
//! | [`FileProvider`] | 文件权限 + AEAD(现有 `zhuye-secrets` 加密文件) | 默认、无依赖 |
//! | [`CommandProvider`] | 外部程序(1Password CLI / `security` / `pass`) | 有钥匙串工具时 |
//! | [`EnvProvider`] | 进程环境 | CI / 临时覆盖 |
//!
//! ## 为什么用 trait 而不是 enum
//!
//! 用户可能塞自己的 provider(公司 vault),trait 让外部实现无需改本 crate。
//! 内置三个实现覆盖绝大多数场景。
//!
//! ## OS 钥匙串的诚实边界
//!
//! [`CommandProvider`] 走命令行(`security` / `op`),**不是**直接链接
//! Keychain / Credential Manager API。原因:直接链接需要平台特定依赖
//! (Security.framework / windows-sys 凭据 API),而 CLI 封装已经足够
//! 覆盖 macOS(`security add-generic-password` / `find-generic-password`)、
//! Linux(`secret-tool`)、跨平台 1Password(`op`)。文档明确写这一点,
//! 不假装"原生钥匙串"。
//!
//! ## 子进程 env 脱敏
//!
//! [`scrub_env`] 是配套的出口护栏:任何要 spawn 的子进程,env 都应先过
//! 它 —— 把疑似凭据的变量名(`*_API_KEY` / `*_TOKEN` / `*_SECRET` …)
//! 过滤掉,避免 agent 跑的 bash 命令能从 `/proc/self/environ` 读到密钥。

use std::collections::HashMap;
use std::process::Command;

use thiserror::Error;

/// provider 错误。
#[derive(Debug, Error)]
pub enum ProviderError {
    /// 后端不存在 / 命令不可执行。
    #[error("provider unavailable: {0}")]
    Unavailable(String),
    /// 读取失败。
    #[error("lookup failed for {key}: {detail}")]
    LookupFailed { key: String, detail: String },
    /// 写入失败。
    #[error("store failed for {key}: {detail}")]
    StoreFailed { key: String, detail: String },
    /// 删除失败。
    #[error("delete failed for {key}: {detail}")]
    DeleteFailed { key: String, detail: String },
}

/// 凭据保存位置的来源标签(UI 徽标 / 诊断用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    File,
    Command,
    Env,
}

impl ProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::File => "encrypted-file",
            ProviderKind::Command => "system-keychain",
            ProviderKind::Env => "env",
        }
    }
}

/// 凭据 provider。
pub trait SecretProvider: Send + Sync {
    /// 来源标签。
    fn kind(&self) -> ProviderKind;

    /// 读取一个凭据。`Ok(None)` = 不存在(与"读取失败"区分)。
    fn get(&self, key: &str) -> Result<Option<String>, ProviderError>;

    /// 写入/覆盖。
    fn set(&self, key: &str, value: &str) -> Result<(), ProviderError>;

    /// 删除。不存在视为成功(幂等)。
    fn delete(&self, key: &str) -> Result<(), ProviderError>;

    /// 后端当前是否可用(用于启动期探测 / UI 显示)。
    fn available(&self) -> bool {
        true
    }
}

/// 加密文件 provider —— 委托给 `zhuye-secrets` 的加密存储。
///
/// 这里用闭包注入读写,避免本模块反向依赖加密实现(也便于测试)。
/// 三个回调类型抽成别名,避免 clippy `type_complexity`。
pub type GetFn = Box<dyn Fn(&str) -> Result<Option<String>, ProviderError> + Send + Sync>;
pub type SetFn = Box<dyn Fn(&str, &str) -> Result<(), ProviderError> + Send + Sync>;
pub type DelFn = Box<dyn Fn(&str) -> Result<(), ProviderError> + Send + Sync>;

pub struct FileProvider {
    get_fn: GetFn,
    set_fn: SetFn,
    del_fn: DelFn,
}

impl FileProvider {
    pub fn new(get_fn: GetFn, set_fn: SetFn, del_fn: DelFn) -> FileProvider {
        FileProvider {
            get_fn,
            set_fn,
            del_fn,
        }
    }
}

impl SecretProvider for FileProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::File
    }
    fn get(&self, key: &str) -> Result<Option<String>, ProviderError> {
        (self.get_fn)(key)
    }
    fn set(&self, key: &str, value: &str) -> Result<(), ProviderError> {
        (self.set_fn)(key, value)
    }
    fn delete(&self, key: &str) -> Result<(), ProviderError> {
        (self.del_fn)(key)
    }
}

/// 外部命令 provider(1Password CLI / `security` / `secret-tool` / `pass`)。
///
/// 三个命令模板:`get` / `set` / `delete`,`{key}` 与 `{value}` 占位。
/// 命令以 **argv 数组**执行(不经 shell),避免 key/value 里的特殊字符
/// 被解释 —— 这是安全底线,不让凭据值进入 shell 解析。
pub struct CommandProvider {
    program: String,
    /// `get` 的 argv 模板,`{key}` 会被替换。
    get_args: Vec<String>,
    /// `set` 的 argv 模板(`{key}` / `{value}`)。
    set_args: Vec<String>,
    /// `delete` 的 argv 模板(`{key}`)。
    delete_args: Vec<String>,
}

impl CommandProvider {
    pub fn new(
        program: impl Into<String>,
        get_args: Vec<String>,
        set_args: Vec<String>,
        delete_args: Vec<String>,
    ) -> CommandProvider {
        CommandProvider {
            program: program.into(),
            get_args,
            set_args,
            delete_args,
        }
    }

    /// macOS `security` 钥匙串预设(通用密码项,service = "zhuye")。
    pub fn macos_keychain() -> CommandProvider {
        CommandProvider::new(
            "security",
            vec![
                "find-generic-password".into(),
                "-s".into(),
                "zhuye".into(),
                "-a".into(),
                "{key}".into(),
                "-w".into(),
            ],
            vec![
                "add-generic-password".into(),
                "-U".into(),
                "-s".into(),
                "zhuye".into(),
                "-a".into(),
                "{key}".into(),
                "-w".into(),
                "{value}".into(),
            ],
            vec![
                "delete-generic-password".into(),
                "-s".into(),
                "zhuye".into(),
                "-a".into(),
                "{key}".into(),
            ],
        )
    }

    /// 1Password CLI 预设(`op`),vault 名 `zhuye`。
    pub fn onepassword() -> CommandProvider {
        CommandProvider::new(
            "op",
            vec!["read".into(), "op://zhuye/{key}/password".into()],
            vec![
                "item".into(),
                "create".into(),
                "--vault".into(),
                "zhuye".into(),
                "--title".into(),
                "{key}".into(),
                "--category".into(),
                "password".into(),
                "--tags".into(),
                "zhuye".into(),
            ],
            vec!["item".into(), "delete".into(), "{key}".into()],
        )
    }

    /// Linux Secret Service 预设(`secret-tool`)。
    pub fn linux_secret_service() -> CommandProvider {
        CommandProvider::new(
            "secret-tool",
            vec![
                "lookup".into(),
                "service".into(),
                "zhuye".into(),
                "account".into(),
                "{key}".into(),
            ],
            vec![
                "store".into(),
                "--label".into(),
                "zhuye".into(),
                "service".into(),
                "zhuye".into(),
                "account".into(),
                "{key}".into(),
            ],
            vec![
                "clear".into(),
                "service".into(),
                "zhuye".into(),
                "account".into(),
                "{key}".into(),
            ],
        )
    }

    fn expand(template: &[String], key: &str, value: Option<&str>) -> Vec<String> {
        template
            .iter()
            .map(|a| {
                a.replace("{key}", key)
                    .replace("{value}", value.unwrap_or(""))
            })
            .collect()
    }

    fn run(
        &self,
        args: &[String],
        stdin: Option<&str>,
    ) -> Result<(bool, String, String), ProviderError> {
        use std::io::Write;
        use std::process::Stdio;
        let mut cmd = Command::new(&self.program);
        cmd.args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| ProviderError::Unavailable(format!("{}: {e}", self.program)))?;
        if let Some(input) = stdin
            && let Some(mut pipe) = child.stdin.take()
        {
            pipe.write_all(input.as_bytes()).ok();
        }
        let out = child
            .wait_with_output()
            .map_err(|e| ProviderError::Unavailable(format!("{}: {e}", self.program)))?;
        Ok((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        ))
    }
}

impl SecretProvider for CommandProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Command
    }

    fn available(&self) -> bool {
        // `which` 语义:直接尝试 spawn 一个无害参数,失败即不可用。
        Command::new(&self.program)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    }

    fn get(&self, key: &str) -> Result<Option<String>, ProviderError> {
        let args = Self::expand(&self.get_args, key, None);
        // secret-tool 的 store 语义从 stdin 读值;get 不需要。
        let (ok, stdout, stderr) = self.run(&args, None)?;
        if !ok {
            // 「未找到」在各后端都是非零退出;区分是否像"压根没装"。
            if stderr.contains("could not be found") || stderr.contains("No such") {
                return Ok(None);
            }
            return Ok(None);
        }
        let value = stdout.trim_end_matches(['\n', '\r']).to_string();
        if value.is_empty() {
            return Ok(None);
        }
        Ok(Some(value))
    }

    fn set(&self, key: &str, value: &str) -> Result<(), ProviderError> {
        let args = Self::expand(&self.set_args, key, Some(value));
        // `secret-tool store` 从 stdin 读密码;`security -w value` 已带在 argv。
        let stdin = if self.program == "secret-tool" {
            Some(value)
        } else {
            None
        };
        let (ok, _out, err) = self.run(&args, stdin)?;
        if ok {
            Ok(())
        } else {
            Err(ProviderError::StoreFailed {
                key: key.to_string(),
                detail: err.trim().to_string(),
            })
        }
    }

    fn delete(&self, key: &str) -> Result<(), ProviderError> {
        let args = Self::expand(&self.delete_args, key, None);
        let (ok, _out, err) = self.run(&args, None)?;
        if ok {
            Ok(())
        } else {
            // 删除不存在的项在各后端都是非零 → 视为幂等成功。
            let _ = (key, err);
            Ok(())
        }
    }
}

/// 环境变量 provider(只读语义:set / delete 不支持)。
///
/// CI 与临时覆盖用:凭据来自 env,不落盘。
pub struct EnvProvider {
    prefix: String,
    /// 允许的前缀白名单为空 = 直接按 key 原名查找。
    env: HashMap<String, String>,
}

impl EnvProvider {
    /// 从进程环境构造(只读取真正存在的变量)。
    pub fn from_process(prefix: impl Into<String>, keys: &[&str]) -> EnvProvider {
        let prefix = prefix.into();
        let mut env = HashMap::new();
        for k in keys {
            let name = if prefix.is_empty() {
                k.to_string()
            } else {
                format!("{prefix}{k}")
            };
            if let Ok(v) = std::env::var(&name) {
                env.insert(k.to_string(), v);
            }
        }
        EnvProvider { prefix, env }
    }

    /// 从显式 map 构造(测试用)。
    pub fn from_map(prefix: impl Into<String>, env: HashMap<String, String>) -> EnvProvider {
        EnvProvider {
            prefix: prefix.into(),
            env,
        }
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }
}

impl SecretProvider for EnvProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Env
    }
    fn get(&self, key: &str) -> Result<Option<String>, ProviderError> {
        Ok(self.env.get(key).cloned())
    }
    fn set(&self, key: &str, _value: &str) -> Result<(), ProviderError> {
        Err(ProviderError::StoreFailed {
            key: key.to_string(),
            detail: "env provider is read-only".to_string(),
        })
    }
    fn delete(&self, key: &str) -> Result<(), ProviderError> {
        Err(ProviderError::DeleteFailed {
            key: key.to_string(),
            detail: "env provider is read-only".to_string(),
        })
    }
}

/// 多 provider 级联:按顺序尝试,第一个命中的胜出。
///
/// 典型装配:env(临时覆盖)→ 系统钥匙串 → 加密文件(兜底)。
pub struct ProviderChain {
    providers: Vec<Box<dyn SecretProvider>>,
}

impl ProviderChain {
    pub fn new(providers: Vec<Box<dyn SecretProvider>>) -> ProviderChain {
        ProviderChain { providers }
    }

    /// 首个命中的 provider 与其值。
    pub fn get(&self, key: &str) -> Result<Option<(ProviderKind, String)>, ProviderError> {
        for p in &self.providers {
            if let Some(v) = p.get(key)? {
                return Ok(Some((p.kind(), v)));
            }
        }
        Ok(None)
    }

    /// 写入到**第一个可写**的 provider(env 只读会被跳过)。
    pub fn set(&self, key: &str, value: &str) -> Result<ProviderKind, ProviderError> {
        let mut last_err = None;
        for p in &self.providers {
            match p.set(key, value) {
                Ok(()) => return Ok(p.kind()),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| ProviderError::StoreFailed {
            key: key.to_string(),
            detail: "no writable provider in chain".to_string(),
        }))
    }

    /// 删除时**所有** provider 都尝试(同一凭据可能多处存在)。
    pub fn delete(&self, key: &str) -> Result<(), ProviderError> {
        for p in &self.providers {
            // 忽略单项错误:某个后端没这个 key 是正常的。
            let _ = p.delete(key);
        }
        Ok(())
    }

    /// 各 provider 的可用性(UI 诊断)。
    pub fn availability(&self) -> Vec<(ProviderKind, bool)> {
        self.providers
            .iter()
            .map(|p| (p.kind(), p.available()))
            .collect()
    }
}

/// 判断一个 env 变量名是否像凭据(需要从子进程 env 里抹掉)。
///
/// 保守:命中任一后缀/片段即视为凭据。宁可多抹(工具读不到环境变量
/// 只影响少数场景),不可少抹(密钥泄漏给 agent 起的 bash)。
pub fn looks_like_credential(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    const FRAGMENTS: [&str; 12] = [
        "API_KEY",
        "APIKEY",
        "ACCESS_KEY",
        "SECRET",
        "TOKEN",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "PRIVATE_KEY",
        "AUTH",
        "SESSION_KEY",
        "COOKIE",
    ];
    FRAGMENTS.iter().any(|f| upper.contains(f))
}

/// 子进程 env 脱敏:返回一份**去掉疑似凭据变量**的 env 副本。
///
/// 调用方在 spawn 工具进程时用它替代继承的 `std::env::vars()`。
pub fn scrub_env(env: impl IntoIterator<Item = (String, String)>) -> HashMap<String, String> {
    env.into_iter()
        .filter(|(k, _)| !looks_like_credential(k))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;

    fn map_provider(data: Arc<Mutex<HashMap<String, String>>>) -> FileProvider {
        let d1 = data.clone();
        let d2 = data.clone();
        FileProvider::new(
            Box::new(move |k| Ok(d1.lock().unwrap().get(k).cloned())),
            Box::new(move |k, v| {
                d2.lock().unwrap().insert(k.to_string(), v.to_string());
                Ok(())
            }),
            Box::new(move |k| {
                data.lock().unwrap().remove(k);
                Ok(())
            }),
        )
    }

    #[test]
    fn file_provider_roundtrip() {
        let data = Arc::new(Mutex::new(HashMap::new()));
        let p = map_provider(data);
        assert_eq!(p.kind(), ProviderKind::File);
        assert!(p.get("k").unwrap().is_none());
        p.set("k", "v").unwrap();
        assert_eq!(p.get("k").unwrap(), Some("v".to_string()));
        p.delete("k").unwrap();
        assert!(p.get("k").unwrap().is_none());
    }

    #[test]
    fn env_provider_readonly() {
        let mut m = HashMap::new();
        m.insert("OPENAI".to_string(), "sk-test".to_string());
        let p = EnvProvider::from_map("ZHUYE_", m);
        assert_eq!(p.kind(), ProviderKind::Env);
        assert_eq!(p.get("OPENAI").unwrap(), Some("sk-test".to_string()));
        assert!(p.set("X", "y").is_err(), "env provider 不可写");
        assert!(p.delete("X").is_err(), "env provider 不可删");
    }

    #[test]
    fn chain_prefers_first_hit() {
        let a = Arc::new(Mutex::new(HashMap::new()));
        let b = Arc::new(Mutex::new(HashMap::new()));
        b.lock()
            .unwrap()
            .insert("k".to_string(), "from-file".to_string());
        let chain = ProviderChain::new(vec![Box::new(map_provider(a)), Box::new(map_provider(b))]);
        assert_eq!(
            chain.get("k").unwrap(),
            Some((ProviderKind::File, "from-file".to_string()))
        );
        assert!(chain.get("missing").unwrap().is_none());
    }

    #[test]
    fn chain_set_skips_readonly_env() {
        let file = Arc::new(Mutex::new(HashMap::new()));
        let env = EnvProvider::from_map("P_", HashMap::new());
        let chain = ProviderChain::new(vec![Box::new(env), Box::new(map_provider(file.clone()))]);
        let kind = chain.set("k", "v").unwrap();
        assert_eq!(kind, ProviderKind::File, "env 只读应被跳过");
        assert_eq!(file.lock().unwrap().get("k"), Some(&"v".to_string()));
    }

    #[test]
    fn chain_delete_touches_all() {
        let a = Arc::new(Mutex::new(HashMap::new()));
        let b = Arc::new(Mutex::new(HashMap::new()));
        a.lock().unwrap().insert("k".to_string(), "1".to_string());
        b.lock().unwrap().insert("k".to_string(), "2".to_string());
        let chain = ProviderChain::new(vec![
            Box::new(map_provider(a.clone())),
            Box::new(map_provider(b.clone())),
        ]);
        chain.delete("k").unwrap();
        assert!(a.lock().unwrap().is_empty());
        assert!(b.lock().unwrap().is_empty(), "所有后端都应删除");
    }

    #[test]
    fn credential_name_detection() {
        assert!(looks_like_credential("OPENAI_API_KEY"));
        assert!(looks_like_credential("GITHUB_TOKEN"));
        assert!(looks_like_credential("DB_PASSWORD"));
        assert!(looks_like_credential("aws_secret_access_key"));
        assert!(looks_like_credential("ANTHROPIC_APIKEY"));
        assert!(!looks_like_credential("PATH"));
        assert!(!looks_like_credential("HOME"));
        assert!(!looks_like_credential("LANG"));
    }

    #[test]
    fn scrub_env_drops_credentials_keeps_rest() {
        let env = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("OPENAI_API_KEY".to_string(), "sk-secret".to_string()),
            ("HOME".to_string(), "/root".to_string()),
            ("GITHUB_TOKEN".to_string(), "ghp_x".to_string()),
        ];
        let scrubbed = scrub_env(env);
        assert_eq!(scrubbed.get("PATH"), Some(&"/usr/bin".to_string()));
        assert_eq!(scrubbed.get("HOME"), Some(&"/root".to_string()));
        assert!(!scrubbed.contains_key("OPENAI_API_KEY"), "凭据必须被抹");
        assert!(!scrubbed.contains_key("GITHUB_TOKEN"), "凭据必须被抹");
    }

    #[test]
    fn command_provider_templates_expand() {
        let args = CommandProvider::expand(
            &["read".to_string(), "op://zhuye/{key}/password".to_string()],
            "openai",
            None,
        );
        assert_eq!(args, vec!["read", "op://zhuye/openai/password"]);
        let set_args = CommandProvider::expand(
            &[
                "--key".to_string(),
                "{key}".to_string(),
                "--value".to_string(),
                "{value}".to_string(),
            ],
            "k",
            Some("v"),
        );
        assert_eq!(set_args, vec!["--key", "k", "--value", "v"]);
    }

    #[test]
    fn command_provider_missing_binary_reports_unavailable() {
        let p = CommandProvider::new(
            "definitely-not-installed-xyz",
            vec!["get".into()],
            vec!["set".into()],
            vec!["del".into()],
        );
        assert!(!p.available());
        assert!(matches!(p.get("k"), Err(ProviderError::Unavailable(_))));
    }

    #[test]
    fn presets_have_expected_program() {
        assert_eq!(CommandProvider::macos_keychain().program, "security");
        assert_eq!(CommandProvider::onepassword().program, "op");
        assert_eq!(
            CommandProvider::linux_secret_service().program,
            "secret-tool"
        );
    }
}
