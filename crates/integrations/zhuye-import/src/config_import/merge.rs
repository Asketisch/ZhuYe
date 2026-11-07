//! IR → `ReflectConfig` 的增量合并与写回。
//!
//! 原则:**只增不改** —— 已有 provider 凭证(label 去重)、已有 MCP 同名
//! 条目一律跳过;不动 `[active]`。
//!
//! 关于用户手写的段级单值 `api_key` / `base_url`:运行时 builder 的规则是
//! 「`credentials` 数组非空 → **只用数组**,否则才 wrap 段级单值」,因此
//! 往一个只有单值的段追加 `[[credentials]]` 会让用户原有的端点与 key 在
//! 运行时**静默失效**。合并前先把这类单值 wrap 成 `label = "default"` 的
//! 显式条目(文本层新增一条,语义层等价),再追加导入项。

use std::path::Path;

use zhuye_config::{
    AnthropicSection, CredentialConfig, McpServerEntry, McpTransport, OpenAISection, ReflectConfig,
};

use super::{ImportedAgentConfig, ImportedMcpTransport};

/// 一次合并的结果报告(dry-run 与实际执行共用)。
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize)]
pub struct MergeReport {
    /// 新增的 provider credential label。
    pub providers_added: Vec<String>,
    /// 跳过的 provider label(已存在,幂等)。
    pub providers_skipped: Vec<String>,
    /// 新增的 MCP server 名。
    pub mcp_added: Vec<String>,
    /// 跳过的 MCP server 名(已存在)。
    pub mcp_skipped: Vec<String>,
    /// 合并过程 / 源解析的警告。
    pub warnings: Vec<String>,
}

impl MergeReport {
    /// 没有任何新增(provider / MCP)—— 调用方据此跳过写回,避免一次
    /// 「什么也没导入」的运行把用户配置文件重排 / 丢注释。
    pub fn is_empty(&self) -> bool {
        self.providers_added.is_empty() && self.mcp_added.is_empty()
    }
}

/// label 在两个 provider 段的 credentials 里是否已存在。
fn label_exists(cfg: &ReflectConfig, label: &str) -> bool {
    let in_anthropic = cfg
        .anthropic
        .as_ref()
        .is_some_and(|s| s.credentials.iter().any(|c| c.label == label));
    let in_openai = cfg
        .openai
        .as_ref()
        .is_some_and(|s| s.credentials.iter().any(|c| c.label == label));
    in_anthropic || in_openai
}

/// 往某 provider 段追加导入条目前,对该段 legacy 单值形态的处置决策。
enum SectionPlan {
    /// 直接追加(数组已非空,或段完全未配置)。
    Append,
    /// 先把单值 wrap 成 `default` 条目再追加。
    WrapThenAppend,
    /// 不能安全 wrap(单值 key 为空但 base_url 非空:key 可能来自 env,
    /// wrap 成空 key 条目会让 builder 失去 env 回落)→ 跳过该段。
    SkipUnsafe,
}

/// 决策依据:运行时 builder 「数组非空 → 只用数组,否则 wrap 单值(含 env
/// 回落)」。详见模块头注释。
fn plan_for_section(
    credentials: &[CredentialConfig],
    api_key: Option<&str>,
    base_url: Option<&str>,
) -> SectionPlan {
    if !credentials.is_empty() {
        return SectionPlan::Append;
    }
    let has_key = api_key.is_some_and(|k| !k.trim().is_empty());
    let has_url = base_url.is_some_and(|u| !u.trim().is_empty());
    match (has_key, has_url) {
        (true, _) => SectionPlan::WrapThenAppend,
        (false, true) => SectionPlan::SkipUnsafe,
        (false, false) => SectionPlan::Append,
    }
}

/// 段级单值 wrap 成显式 `default` 条目(与 `zhuye_config::builder` 的
/// wrap 口径一致:label `default`、weight 1)。
fn wrapped_default(
    api_key: Option<&str>,
    base_url: Option<&str>,
    model: Option<&str>,
) -> CredentialConfig {
    CredentialConfig {
        label: "default".into(),
        api_key: api_key.unwrap_or_default().to_string(),
        base_url: base_url.map(str::to_string),
        model: model.map(str::to_string),
        weight: 1,
        cooldown_override_secs: None,
        quota: None,
    }
}

/// 把一份来源 IR 合并进 `cfg`(内存,不落盘)。
pub fn merge_into_config(cfg: &mut ReflectConfig, imported: &ImportedAgentConfig) -> MergeReport {
    let mut report = MergeReport::default();
    let agent = imported
        .kind
        .map_or_else(|| "unknown".into(), |k| k.as_str().to_string());

    // ── providers:按兼容面落到 anthropic / openai 的 credentials 数组 ──
    for p in &imported.providers {
        // 幂等:任一段已有同 label → 跳过(即使兼容面判断变了也不重复插)。
        let exists = label_exists(cfg, &p.label);
        if exists {
            report.providers_skipped.push(p.label.clone());
            continue;
        }
        let cred = CredentialConfig {
            label: p.label.clone(),
            api_key: p.api_key.clone(),
            base_url: p.base_url.clone(),
            model: p.model.clone(),
            weight: 1,
            cooldown_override_secs: None,
            quota: None,
        };
        let section_name = if p.anthropic_compatible {
            "anthropic"
        } else {
            "openai"
        };
        // 两段结构不同,这里取出共同需要的三元组做决策,再分别写回。
        let (plan, wrapped) = if p.anthropic_compatible {
            let s = cfg.anthropic.get_or_insert_with(AnthropicSection::default);
            let plan =
                plan_for_section(&s.credentials, s.api_key.as_deref(), s.base_url.as_deref());
            let wrapped = wrapped_default(
                s.api_key.as_deref(),
                s.base_url.as_deref(),
                s.model.as_deref(),
            );
            (plan, wrapped)
        } else {
            let s = cfg.openai.get_or_insert_with(OpenAISection::default);
            let plan =
                plan_for_section(&s.credentials, s.api_key.as_deref(), s.base_url.as_deref());
            let wrapped = wrapped_default(
                s.api_key.as_deref(),
                s.base_url.as_deref(),
                s.model.as_deref(),
            );
            (plan, wrapped)
        };
        match plan {
            SectionPlan::SkipUnsafe => {
                report.warnings.push(format!(
                    "{agent}: 跳过 {}(目标 [{section_name}] 段只有 base_url、api_key 为空——\
                     key 可能来自环境变量,追加 [[{section_name}.credentials]] 会让其失效;\
                     请先手动改成 credentials 数组形态再导入)",
                    p.label
                ));
                report.providers_skipped.push(p.label.clone());
                continue;
            }
            SectionPlan::WrapThenAppend => {
                report.warnings.push(format!(
                    "[{section_name}] 原有单值 api_key/base_url 已 wrap 为 \
                     [[{section_name}.credentials]] label=\"default\",避免被导入条目遮蔽"
                ));
                push_credential(cfg, p.anthropic_compatible, wrapped);
            }
            SectionPlan::Append => {}
        }
        push_credential(cfg, p.anthropic_compatible, cred);
        report.providers_added.push(p.label.clone());
    }
    // 导入条目与既有条目同池加权轮询:未钉 `[active].credential` 时请求会
    // 在各端点间轮转,提示用户按需钉住。
    if !report.providers_added.is_empty() && cfg.active.credential.is_none() {
        report.warnings.push(
            "导入的凭证与既有凭证同池轮询;若只想用其中一个,请在 [active] 设 credential = \"<label>\""
                .into(),
        );
    }

    // ── MCP servers:同名跳过 ─────────────────────────────────────────
    for server in &imported.mcp_servers {
        if cfg.mcp_servers.servers.contains_key(&server.name) {
            report.mcp_skipped.push(server.name.clone());
            continue;
        }
        cfg.mcp_servers
            .servers
            .insert(server.name.clone(), to_mcp_entry(&server.transport));
        report.mcp_added.push(server.name.clone());
    }

    // ── 源警告上浮 ────────────────────────────────────────────────────
    for w in &imported.warnings {
        report.warnings.push(format!("{agent}: {w}"));
    }
    report
}

/// 按兼容面把一条凭证追加到对应段的 `credentials` 数组。
fn push_credential(cfg: &mut ReflectConfig, anthropic_compatible: bool, cred: CredentialConfig) {
    if anthropic_compatible {
        cfg.anthropic
            .get_or_insert_with(AnthropicSection::default)
            .credentials
            .push(cred);
    } else {
        cfg.openai
            .get_or_insert_with(OpenAISection::default)
            .credentials
            .push(cred);
    }
}

/// IR transport → config 表项。
fn to_mcp_entry(t: &ImportedMcpTransport) -> McpServerEntry {
    match t {
        ImportedMcpTransport::Stdio { command, args, env } => McpServerEntry {
            transport: McpTransport::Stdio,
            command: Some(command.clone()),
            args: if args.is_empty() {
                None
            } else {
                Some(args.clone())
            },
            env: if env.is_empty() {
                None
            } else {
                Some(env.clone())
            },
            url: None,
            headers: None,
            timeout_ms: None,
            always_load: None,
        },
        ImportedMcpTransport::Http { url, headers, sse } => McpServerEntry {
            transport: if *sse {
                McpTransport::Sse
            } else {
                McpTransport::Http
            },
            command: None,
            args: None,
            env: None,
            url: Some(url.clone()),
            headers: if headers.is_empty() {
                None
            } else {
                Some(headers.clone())
            },
            timeout_ms: None,
            always_load: None,
        },
    }
}

/// 从目标路径加载既有配置。
///
/// - 文件缺失 → `Default`(首次使用);
/// - 文件存在但读取 / 解析失败 → **硬失败**。此前这里降级为 `Default` +
///   warning,随后调用方无条件写回,等于一个 typo 就把用户全部既有
///   API key / permissions / hooks 清空只剩导入内容(数据丢失)。
///   解析错误只回传 `message()` + 行号,不带 TOML 源码行(那一行可能正是
///   `api_key = "sk-…"`,会随错误文本进 stderr / MCP 工具结果 / rollout)。
pub fn load_target(path: &Path) -> Result<ReflectConfig, String> {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).map_err(|e: toml::de::Error| {
            let line = e
                .span()
                .map(|sp| s[..sp.start.min(s.len())].matches('\n').count() + 1);
            format!(
                "目标配置 {} 解析失败(第 {} 行):{};为避免覆盖丢失既有配置,已中止。\
                 请修复该文件后重试",
                path.display(),
                line.map_or_else(|| "?".to_string(), |l| l.to_string()),
                e.message()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ReflectConfig::default()),
        Err(e) => Err(format!(
            "目标配置 {} 读取失败({e});为避免覆盖丢失既有配置,已中止",
            path.display()
        )),
    }
}

/// 写回目标路径(建父目录;pretty TOML)。
///
/// **原子写 + 0600**:先写同目录临时文件、fsync、再 rename 覆盖 ——
/// 中途被 kill / 磁盘满不会留下空 / 半截的 config.toml(该文件持有全部
/// provider 密钥,截断写的后果是下次启动所有凭证丢失)。Unix 下临时文件
/// 以 0600 创建,rename 后权限随之,避免密钥文件按 umask 落成 0644。
pub fn write_target(path: &Path, cfg: &ReflectConfig) -> Result<(), String> {
    use std::io::Write as _;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败:{e}"))?;
    let body = toml::to_string_pretty(cfg).map_err(|e| format!("序列化失败:{e}"))?;

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.toml");
    let tmp = parent.join(format!(".{file_name}.tmp-{}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let write_result = (|| -> std::io::Result<()> {
        let mut f = opts.open(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = write_result {
        // 失败保全:清掉临时文件,原文件保持不变。
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("写入 {} 失败:{e}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_import::{ConfigSourceKind, ImportedProvider};
    use std::collections::HashMap;

    fn imported() -> ImportedAgentConfig {
        let mut m = ImportedAgentConfig {
            kind: Some(ConfigSourceKind::Opencode),
            ..Default::default()
        };
        m.providers.push(ImportedProvider {
            label: "imported:opencode:deepseek".into(),
            api_key: "sk-1".into(),
            base_url: Some("https://api.deepseek.com/v1".into()),
            model: Some("deepseek-chat".into()),
            anthropic_compatible: false,
        });
        m.providers.push(ImportedProvider {
            label: "imported:opencode:anthropic".into(),
            api_key: String::new(), // 免 key 端点
            base_url: Some("http://127.0.0.1:1234/v1".into()),
            model: Some("claude-sonnet-4-latest".into()),
            anthropic_compatible: true,
        });
        let mut env = HashMap::new();
        env.insert("KEY".to_string(), "V".to_string());
        m.mcp_servers.push(super::super::ImportedMcpServer {
            name: "fetch".into(),
            transport: ImportedMcpTransport::Stdio {
                command: "uvx".into(),
                args: vec!["mcp-server-fetch".into()],
                env,
            },
        });
        m
    }

    #[test]
    fn 合并_新增凭证与mcp_空key照常导入() {
        let mut cfg = ReflectConfig::default();
        let report = merge_into_config(&mut cfg, &imported());
        assert_eq!(report.providers_added.len(), 2);
        assert_eq!(report.mcp_added, vec!["fetch".to_string()]);

        let openai = cfg.openai.as_ref().unwrap();
        assert_eq!(openai.credentials.len(), 1);
        assert_eq!(openai.credentials[0].label, "imported:opencode:deepseek");
        assert_eq!(
            openai.credentials[0].base_url.as_deref(),
            Some("https://api.deepseek.com/v1")
        );
        assert_eq!(
            openai.credentials[0].model.as_deref(),
            Some("deepseek-chat")
        );

        let anthropic = cfg.anthropic.as_ref().unwrap();
        assert_eq!(anthropic.credentials.len(), 1);
        assert_eq!(anthropic.credentials[0].api_key, "", "免 key 端点空串保留");
        assert_eq!(
            anthropic.credentials[0].base_url.as_deref(),
            Some("http://127.0.0.1:1234/v1")
        );

        let mcp = &cfg.mcp_servers.servers["fetch"];
        assert!(matches!(mcp.transport, McpTransport::Stdio));
        assert_eq!(mcp.command.as_deref(), Some("uvx"));
        assert_eq!(
            mcp.args.as_deref(),
            Some(["mcp-server-fetch".to_string()].as_slice())
        );
        assert_eq!(mcp.env.as_ref().unwrap()["KEY"], "V");
    }

    #[test]
    fn 合并_幂等_重复导入全部跳过() {
        let mut cfg = ReflectConfig::default();
        merge_into_config(&mut cfg, &imported());
        let report2 = merge_into_config(&mut cfg, &imported());
        assert!(report2.providers_added.is_empty());
        assert_eq!(report2.providers_skipped.len(), 2);
        assert_eq!(report2.mcp_skipped, vec!["fetch".to_string()]);
        // 不重复插入。
        assert_eq!(cfg.openai.as_ref().unwrap().credentials.len(), 1);
        assert_eq!(cfg.mcp_servers.servers.len(), 1);
    }

    #[test]
    fn 合并_不动用户已有同名mcp() {
        let mut cfg = ReflectConfig::default();
        cfg.mcp_servers.servers.insert(
            "fetch".into(),
            McpServerEntry {
                transport: McpTransport::Http,
                url: Some("https://mine.example/mcp".into()),
                ..Default::default()
            },
        );
        let report = merge_into_config(&mut cfg, &imported());
        assert_eq!(report.mcp_added, Vec::<String>::new());
        assert_eq!(
            cfg.mcp_servers.servers["fetch"].url.as_deref(),
            Some("https://mine.example/mcp")
        );
    }

    #[test]
    fn 写回与加载_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let mut cfg = ReflectConfig::default();
        let r = merge_into_config(&mut cfg, &imported());
        assert!(!r.is_empty());
        write_target(&path, &cfg).unwrap();
        let loaded = load_target(&path).unwrap();
        assert_eq!(
            loaded.openai.as_ref().unwrap().credentials[0].label,
            "imported:opencode:deepseek"
        );
        assert!(loaded.mcp_servers.servers.contains_key("fetch"));
        // 原子写不留临时文件;Unix 下密钥文件 0600。
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时文件:{leftovers:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "持密钥的配置应为 0600,实际 {mode:o}");
        }
        // 缺失文件 → Default(首次使用)。
        let fresh = load_target(&dir.path().join("none.toml")).unwrap();
        assert!(fresh.openai.is_none());
    }

    /// P0 回归:目标文件存在但解析失败必须硬失败,而不是降级为 Default 后
    /// 被写回覆盖(等于清空用户全部既有配置)。错误文本不得带 TOML 源码行
    /// (可能含 api_key)。
    #[test]
    fn 坏目标文件_硬失败且不泄露源码行() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[openai]\napi_key = \"sk-SECRET-should-not-leak\"\nbroken = [valid\n",
        )
        .unwrap();
        let err = load_target(&path).unwrap_err();
        assert!(err.contains("解析失败"), "{err}");
        assert!(err.contains("第 3 行"), "应定位到出错行:{err}");
        assert!(
            !err.contains("sk-SECRET"),
            "错误文本不得泄露密钥所在源码行:{err}"
        );
        // 原文件原样保留(没被覆盖)。
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("sk-SECRET")
        );
    }

    /// P1 回归:目标段只有 legacy 单值 api_key 时,追加 credentials 数组会让
    /// builder 只用数组、单值静默失效 —— 合并前必须先把单值 wrap 成
    /// `default` 条目。
    #[test]
    fn 合并_legacy单值key先wrap再追加() {
        let mut cfg = ReflectConfig {
            openai: Some(OpenAISection {
                api_key: Some("sk-user-own".into()),
                base_url: Some("https://my.gateway/v1".into()),
                model: Some("gpt-x".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let report = merge_into_config(&mut cfg, &imported());
        let creds = &cfg.openai.as_ref().unwrap().credentials;
        assert_eq!(creds.len(), 2, "wrap 的 default + 导入条目");
        assert_eq!(creds[0].label, "default");
        assert_eq!(creds[0].api_key, "sk-user-own");
        assert_eq!(creds[0].base_url.as_deref(), Some("https://my.gateway/v1"));
        assert_eq!(creds[0].model.as_deref(), Some("gpt-x"));
        assert_eq!(creds[1].label, "imported:opencode:deepseek");
        assert!(
            report.warnings.iter().any(|w| w.contains("wrap")),
            "应提示 wrap:{:?}",
            report.warnings
        );
        // 同池轮询提示(未钉 active.credential)。
        assert!(
            report.warnings.iter().any(|w| w.contains("同池轮询")),
            "{:?}",
            report.warnings
        );
    }

    /// 目标段只有 base_url、api_key 为空(key 来自 env)时不能安全 wrap ——
    /// 跳过该段并提示,不让用户既有端点静默失效。
    #[test]
    fn 合并_仅base_url的legacy段跳过不破坏() {
        let mut cfg = ReflectConfig {
            openai: Some(OpenAISection {
                api_key: None,
                base_url: Some("https://env-key.gateway/v1".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let report = merge_into_config(&mut cfg, &imported());
        // openai 侧导入项被跳过;anthropic 侧照常。
        assert!(cfg.openai.as_ref().unwrap().credentials.is_empty());
        assert!(
            report
                .providers_skipped
                .contains(&"imported:opencode:deepseek".to_string())
        );
        assert_eq!(report.providers_added, vec!["imported:opencode:anthropic"]);
        assert!(
            report.warnings.iter().any(|w| w.contains("环境变量")),
            "{:?}",
            report.warnings
        );
    }
}
