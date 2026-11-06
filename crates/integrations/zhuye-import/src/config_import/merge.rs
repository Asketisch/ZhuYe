//! IR → `ReflectConfig` 的增量合并与写回。
//!
//! 原则:**只增不改** —— 已有 provider 凭证(label 去重)、已有 MCP 同名
//! 条目一律跳过;不动 `[active]`、不动用户手写的单值 `api_key` / `base_url`。

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
        if p.anthropic_compatible {
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
        report.providers_added.push(p.label.clone());
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
        ImportedMcpTransport::Http { url } => McpServerEntry {
            transport: McpTransport::Http,
            command: None,
            args: None,
            env: None,
            url: Some(url.clone()),
            headers: None,
            timeout_ms: None,
            always_load: None,
        },
    }
}

/// 从目标路径加载既有配置(缺失 / 坏文件 → Default + warning,「一键导入」
/// 不该被一个坏的目标文件挡住;坏文件会被写回覆盖,先如实记进报告)。
pub fn load_target(path: &Path) -> (ReflectConfig, Vec<String>) {
    match std::fs::read_to_string(path) {
        Ok(s) => match toml::from_str(&s) {
            Ok(cfg) => (cfg, Vec::new()),
            Err(e) => (
                ReflectConfig::default(),
                vec![format!(
                    "目标配置 {} 解析失败,将覆盖为全新配置:{e}",
                    path.display()
                )],
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            (ReflectConfig::default(), Vec::new())
        }
        Err(e) => (
            ReflectConfig::default(),
            vec![format!(
                "目标配置 {} 读取失败({e}),将覆盖为全新配置",
                path.display()
            )],
        ),
    }
}

/// 写回目标路径(建父目录;pretty TOML)。与 `zhuye-cli::login::write_config_to`
/// 同构 —— 本 crate 刻意不依赖 zhuye-cli(不进 SDK facade 的约束)。
pub fn write_target(path: &Path, cfg: &ReflectConfig) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败:{e}"))?;
    }
    let body = toml::to_string_pretty(cfg).map_err(|e| format!("序列化失败:{e}"))?;
    std::fs::write(path, body).map_err(|e| format!("写入 {} 失败:{e}", path.display()))
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
        let (loaded, warnings) = load_target(&path);
        assert!(warnings.is_empty());
        assert_eq!(
            loaded.openai.as_ref().unwrap().credentials[0].label,
            "imported:opencode:deepseek"
        );
        assert!(loaded.mcp_servers.servers.contains_key("fetch"));
        // 坏文件 → Default + warning。
        std::fs::write(&path, "not = [valid").unwrap();
        let (cfg2, w2) = load_target(&path);
        assert!(cfg2.openai.is_none());
        assert!(!w2.is_empty());
    }
}
