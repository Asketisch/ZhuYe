//! 各家来源的配置读取与解析。
//!
//! 只读不改;字段缺失 / 格式异常记 warning 跳过(`ImportError` 仅用于
//! IO 层面的失败)。每家一个 `parse_*` 函数,`parse_source` 按 kind 分发。

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{
    ConfigSourceKind, ImportedAgentConfig, ImportedMcpServer, ImportedMcpTransport,
    ImportedProvider,
};

/// 按来源种类解析 `home` 下的默认配置位置。
pub fn parse_source(kind: ConfigSourceKind, home: &Path) -> Result<ImportedAgentConfig, String> {
    let mut cfg = ImportedAgentConfig {
        kind: Some(kind),
        ..Default::default()
    };
    match kind {
        ConfigSourceKind::ClaudeCode => parse_claude_code(home, &mut cfg)?,
        ConfigSourceKind::Codex => parse_codex(home, &mut cfg)?,
        ConfigSourceKind::Gemini => parse_gemini(home, &mut cfg)?,
        ConfigSourceKind::Opencode => parse_opencode(home, &mut cfg)?,
        ConfigSourceKind::Crush => parse_crush(home, &mut cfg)?,
    }
    Ok(cfg)
}

// ── 通用小工具 ───────────────────────────────────────────────────────

/// 读 JSON 文件;缺失 → Ok(None)(未安装是常态),坏 JSON → warning。
fn read_json(path: &Path, cfg: &mut ImportedAgentConfig) -> Result<Option<Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(None),
        Ok(s) => match serde_json::from_str(&s) {
            Ok(v) => Ok(Some(v)),
            Err(e) => {
                cfg.push_warning(format!("{} 解析失败,已跳过:{e}", path.display()));
                Ok(None)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{} 读取失败:{e}", path.display())),
    }
}

/// 读 TOML 文件(serde_json::Value 形态,统一后续取值路径)。
fn read_toml(path: &Path, cfg: &mut ImportedAgentConfig) -> Result<Option<Value>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{} 读取失败:{e}", path.display())),
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    match toml::from_str::<toml::Value>(&raw) {
        Ok(v) => Ok(Some(toml_to_json(&v))),
        Err(e) => {
            cfg.push_warning(format!("{} 解析失败,已跳过:{e}", path.display()));
            Ok(None)
        }
    }
}

/// toml::Value → serde_json::Value(浅递归;日期等特殊类型转字符串)。
fn toml_to_json(v: &toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::Number((*i).into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Array(a) => Value::Array(a.iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => {
            let mut m = serde_json::Map::new();
            for (k, v) in t {
                m.insert(k.clone(), toml_to_json(v));
            }
            Value::Object(m)
        }
        other => Value::String(other.to_string()),
    }
}

fn s_of(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
}

/// 归一化 MCP server 定义(Claude Code / Gemini / opencode / crush 的
/// `{command?, args?, env?, url?, type?}` 形状一致)。
fn mcp_entry(name: &str, v: &Value, cfg: &mut ImportedAgentConfig) -> Option<ImportedMcpServer> {
    let obj = v.as_object()?;
    // enabled=false(opencode)显式跳过。
    if obj.get("enabled").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let transport = if let Some(url) = s_of(obj.get("url")) {
        ImportedMcpTransport::Http { url }
    } else if let Some(command) = s_of(obj.get("command")) {
        let args = obj
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let env = obj
            .get("env")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        ImportedMcpTransport::Stdio { command, args, env }
    } else {
        cfg.push_warning(format!("mcp server `{name}` 既无 command 也无 url,跳过"));
        return None;
    };
    Some(ImportedMcpServer {
        name: name.to_string(),
        transport,
    })
}

/// 遍历 `{name: {...}}` 形状的 MCP 表。
fn collect_mcp_map(map: Option<&Value>, cfg: &mut ImportedAgentConfig) {
    let Some(obj) = map.and_then(Value::as_object) else {
        return;
    };
    for (name, v) in obj {
        match mcp_entry(name, v, cfg) {
            Some(server) => cfg.mcp_servers.push(server),
            None => continue,
        }
    }
}

/// 判定是否为 Anthropic 兼容端点(落 `[[anthropic.credentials]]`)。
fn is_anthropic_compatible(key: &str, base_url: Option<&str>, model: Option<&str>) -> bool {
    if key.contains("anthropic") || key.contains("claude") {
        return true;
    }
    if base_url.is_some_and(|u| u.contains("anthropic")) {
        return true;
    }
    model.is_some_and(|m| m.starts_with("claude-"))
}

// ── Claude Code ──────────────────────────────────────────────────────
//
// 两个文件:
// - ~/.claude/settings.json:env.ANTHROPIC_API_KEY / ANTHROPIC_BASE_URL、model
// - ~/.claude.json:顶层 mcpServers、primaryApiKey(legacy)

fn parse_claude_code(home: &Path, cfg: &mut ImportedAgentConfig) -> Result<(), String> {
    let mut found_any = false;

    let settings_path = home.join(".claude").join("settings.json");
    if let Some(settings) = read_json(&settings_path, cfg)? {
        found_any = true;
        let env = settings.get("env").and_then(Value::as_object);
        let get_env = |k: &str| env.and_then(|m| m.get(k)).and_then(Value::as_str);
        let api_key = get_env("ANTHROPIC_API_KEY").unwrap_or_default().to_string();
        let base_url = get_env("ANTHROPIC_BASE_URL").map(str::to_string);
        let model = s_of(settings.get("model"));
        if !api_key.is_empty() || base_url.is_some() || model.is_some() {
            cfg.providers.push(ImportedProvider {
                label: "imported:claude-code:anthropic".into(),
                api_key,
                base_url,
                model,
                anthropic_compatible: true,
            });
        }
        collect_mcp_map(settings.get("mcpServers"), cfg);
    }

    let claude_json_path = home.join(".claude.json");
    if let Some(cj) = read_json(&claude_json_path, cfg)? {
        found_any = true;
        collect_mcp_map(cj.get("mcpServers"), cfg);
        // primaryApiKey 是 legacy 形态;settings.json 已导入同 label 时跳过。
        if let Some(key) = s_of(cj.get("primaryApiKey")) {
            let already = cfg
                .providers
                .iter()
                .any(|p| p.label == "imported:claude-code:anthropic");
            if !already {
                cfg.providers.push(ImportedProvider {
                    label: "imported:claude-code:anthropic".into(),
                    api_key: key,
                    base_url: None,
                    model: None,
                    anthropic_compatible: true,
                });
            }
        }
    }

    if !found_any {
        cfg.push_warning("claude-code: 未找到 ~/.claude/settings.json 或 ~/.claude.json");
    }
    Ok(())
}

// ── Codex ────────────────────────────────────────────────────────────
//
// ~/.codex/config.toml:
//   model = "gpt-5.1"
//   model_provider = "oss"
//   [model_providers.oss] base_url = "..." env_key = "OPENAI_API_KEY"
//   [mcp_servers.<name>] command / args / env

fn parse_codex(home: &Path, cfg: &mut ImportedAgentConfig) -> Result<(), String> {
    let path = home.join(".codex").join("config.toml");
    let Some(v) = read_toml(&path, cfg)? else {
        cfg.push_warning("codex: 未找到 ~/.codex/config.toml");
        return Ok(());
    };
    let top_model = s_of(v.get("model"));
    let active_provider = s_of(v.get("model_provider"));
    let providers = v.get("model_providers").and_then(Value::as_object);
    if let Some(providers) = providers {
        for (key, def) in providers {
            let base_url = s_of(def.get("base_url"));
            let env_key = s_of(def.get("env_key"));
            // env_key 引用的密钥在进程 env 里;导入时尽力读取(读不到 = 空,
            // 落成免 key 形态,由用户后续补)。
            let api_key = env_key
                .and_then(|k| std::env::var(k).ok())
                .filter(|s| !s.is_empty())
                .unwrap_or_default();
            let is_active = active_provider.as_deref() == Some(key.as_str());
            let model = if is_active { top_model.clone() } else { None };
            // 无 base_url 且无 key 的 provider(如官方 openai 默认项)只在其
            // 为激活项时导入(让模型选择生效)。
            if base_url.is_none() && api_key.is_empty() && !is_active {
                continue;
            }
            if base_url.is_none() && api_key.is_empty() && is_active && model.is_none() {
                continue;
            }
            let anthropic = is_anthropic_compatible(key, base_url.as_deref(), model.as_deref());
            cfg.providers.push(ImportedProvider {
                label: format!("imported:codex:{key}"),
                api_key,
                base_url,
                model,
                anthropic_compatible: anthropic,
            });
        }
    } else if top_model.is_some() {
        // 无 model_providers 段但有 model(官方 openai 形态)→ 导入为无
        // base_url 的 openai provider。
        cfg.providers.push(ImportedProvider {
            label: "imported:codex:openai".into(),
            api_key: std::env::var("OPENAI_API_KEY").unwrap_or_default(),
            base_url: None,
            model: top_model,
            anthropic_compatible: false,
        });
    }
    collect_mcp_map(v.get("mcp_servers"), cfg);
    Ok(())
}

// ── Gemini CLI ───────────────────────────────────────────────────────
//
// ~/.gemini/settings.json:mcpServers、selectedModel。
// Gemini 模型暂无对应烛夜 provider → 只导 MCP,模型记 warning。

fn parse_gemini(home: &Path, cfg: &mut ImportedAgentConfig) -> Result<(), String> {
    let path = home.join(".gemini").join("settings.json");
    let Some(v) = read_json(&path, cfg)? else {
        cfg.push_warning("gemini: 未找到 ~/.gemini/settings.json");
        return Ok(());
    };
    if let Some(m) = s_of(v.get("selectedModel")) {
        cfg.push_warning(format!(
            "gemini: selectedModel `{m}` 暂无对应烛夜 provider(仅导入 MCP servers)"
        ));
    }
    collect_mcp_map(v.get("mcpServers"), cfg);
    Ok(())
}

// ── opencode ─────────────────────────────────────────────────────────
//
// ~/.config/opencode/opencode.json(或 ~/.opencode.json):
//   { "provider": { "<key>": { "baseURL", "apiKey", "models": {...} } },
//     "model": "deepseek/deepseek-chat",
//     "mcp": { "<name>": {...} } }

fn parse_opencode(home: &Path, cfg: &mut ImportedAgentConfig) -> Result<(), String> {
    let candidates: Vec<PathBuf> = vec![
        home.join(".config").join("opencode").join("opencode.json"),
        home.join(".opencode.json"),
    ];
    let mut parsed = false;
    for path in candidates {
        if let Some(v) = read_json(&path, cfg)? {
            parsed = true;
            parse_opencode_like(&v, cfg, "opencode");
            break;
        }
    }
    if !parsed {
        cfg.push_warning("opencode: 未找到 ~/.config/opencode/opencode.json");
    }
    Ok(())
}

// ── crush ────────────────────────────────────────────────────────────
//
// ~/.config/crush/config.json:
//   { "providers": { "<key>": { "baseUrl"/"base_url", "apiKey", "models": [...] } },
//     "mcp": { "<name>": {...} } }

fn parse_crush(home: &Path, cfg: &mut ImportedAgentConfig) -> Result<(), String> {
    let path = home.join(".config").join("crush").join("config.json");
    let Some(v) = read_json(&path, cfg)? else {
        cfg.push_warning("crush: 未找到 ~/.config/crush/config.json");
        return Ok(());
    };
    parse_crush_config(&v, cfg);
    Ok(())
}

/// opencode 风格配置(`provider` 键 + baseURL/apiKey camelCase)。
fn parse_opencode_like(v: &Value, cfg: &mut ImportedAgentConfig, agent: &str) {
    let selected = s_of(v.get("model"));
    if let Some(providers) = v.get("provider").and_then(Value::as_object) {
        for (key, def) in providers {
            let base_url = s_of(def.get("baseURL"))
                .or_else(|| s_of(def.get("base_url")))
                .or_else(|| s_of(def.get("baseUrl")));
            let api_key = s_of(def.get("apiKey"))
                .or_else(|| s_of(def.get("api_key")))
                .unwrap_or_default();
            if base_url.is_none() && api_key.is_empty() {
                continue;
            }
            // 选中模型形如 "<key>/<model>" 时挂到对应 provider。
            let model = selected.as_deref().and_then(|sel| {
                sel.split_once('/')
                    .and_then(|(k, m)| (k == key).then(|| m.to_string()))
            });
            let anthropic = is_anthropic_compatible(key, base_url.as_deref(), model.as_deref());
            cfg.providers.push(ImportedProvider {
                label: format!("imported:{agent}:{key}"),
                api_key,
                base_url,
                model,
                anthropic_compatible: anthropic,
            });
        }
    }
    collect_mcp_map(v.get("mcp"), cfg);
}

/// crush 风格配置(`providers` 键 + baseUrl/apiKey)。
fn parse_crush_config(v: &Value, cfg: &mut ImportedAgentConfig) {
    let selected = s_of(v.get("model"));
    if let Some(providers) = v.get("providers").and_then(Value::as_object) {
        for (key, def) in providers {
            let base_url = s_of(def.get("baseUrl"))
                .or_else(|| s_of(def.get("base_url")))
                .or_else(|| s_of(def.get("api_base")))
                .or_else(|| s_of(def.get("baseURL")));
            let api_key = s_of(def.get("apiKey"))
                .or_else(|| s_of(def.get("api_key")))
                .unwrap_or_default();
            if base_url.is_none() && api_key.is_empty() {
                continue;
            }
            let model = selected.as_deref().and_then(|sel| {
                sel.split_once('/')
                    .and_then(|(k, m)| (k == key).then(|| m.to_string()))
            });
            let anthropic = is_anthropic_compatible(key, base_url.as_deref(), model.as_deref());
            cfg.providers.push(ImportedProvider {
                label: format!("imported:crush:{key}"),
                api_key,
                base_url,
                model,
                anthropic_compatible: anthropic,
            });
        }
    }
    collect_mcp_map(v.get("mcp"), cfg);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_import::ImportedMcpTransport;

    #[test]
    fn 解析claude_code_settings与claude_json() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(
            home.join(".claude").join("settings.json"),
            r#"{
              "env": {
                "ANTHROPIC_API_KEY": "sk-ant-1",
                "ANTHROPIC_BASE_URL": "https://gw.example.com/v1"
              },
              "model": "claude-sonnet-4-latest"
            }"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".claude.json"),
            r#"{"mcpServers": {"fetch": {"command": "uvx", "args": ["mcp-server-fetch"], "env": {"K": "V"}}}}"#,
        )
        .unwrap();

        let cfg = parse_source(ConfigSourceKind::ClaudeCode, home).unwrap();
        assert_eq!(cfg.providers.len(), 1);
        let p = &cfg.providers[0];
        assert_eq!(p.label, "imported:claude-code:anthropic");
        assert_eq!(p.api_key, "sk-ant-1");
        assert_eq!(p.base_url.as_deref(), Some("https://gw.example.com/v1"));
        assert_eq!(p.model.as_deref(), Some("claude-sonnet-4-latest"));
        assert!(p.anthropic_compatible);

        assert_eq!(cfg.mcp_servers.len(), 1);
        match &cfg.mcp_servers[0].transport {
            ImportedMcpTransport::Stdio { command, args, env } => {
                assert_eq!(command, "uvx");
                assert_eq!(args, &vec!["mcp-server-fetch".to_string()]);
                assert_eq!(env["K"], "V");
            }
            other => panic!("expected stdio, got {other:?}"),
        }
    }

    #[test]
    fn 解析codex_toml含env_key与mcp() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex").join("config.toml"),
            r#"
            model = "gpt-5.1"
            model_provider = "oss"

            [model_providers.oss]
            name = "OSS gateway"
            base_url = "https://oss.example.com/v1"
            env_key = "ZZ_TEST_CODEX_KEY"

            [mcp_servers.linter]
            command = "npx"
            args = ["-y", "mcp-linter"]
            "#,
        )
        .unwrap();
        // SAFETY:测试内短临界区设置 env(进程内串行执行该模块)。
        unsafe { std::env::set_var("ZZ_TEST_CODEX_KEY", "sk-codex-1") };

        let cfg = parse_source(ConfigSourceKind::Codex, home).unwrap();
        assert_eq!(cfg.providers.len(), 1);
        let p = &cfg.providers[0];
        assert_eq!(p.label, "imported:codex:oss");
        assert_eq!(p.api_key, "sk-codex-1");
        assert_eq!(p.base_url.as_deref(), Some("https://oss.example.com/v1"));
        assert_eq!(
            p.model.as_deref(),
            Some("gpt-5.1"),
            "激活 provider 挂顶层 model"
        );
        assert!(!p.anthropic_compatible);
        assert_eq!(cfg.mcp_servers.len(), 1);
        assert_eq!(cfg.mcp_servers[0].name, "linter");
    }

    #[test]
    fn 解析opencode_provider与mcp与选中模型() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join(".config").join("opencode")).unwrap();
        std::fs::write(
            home.join(".config").join("opencode").join("opencode.json"),
            r#"{
              "model": "deepseek/deepseek-chat",
              "provider": {
                "deepseek": {
                  "baseURL": "https://api.deepseek.com/v1",
                  "apiKey": "sk-ds-1"
                },
                "local": {
                  "baseURL": "http://127.0.0.1:8080/v1",
                  "apiKey": ""
                },
                "empty-entry": {}
              },
              "mcp": {
                "busy": {"command": "x", "enabled": false},
                "http-one": {"url": "https://mcp.example/mcp"}
              }
            }"#,
        )
        .unwrap();

        let cfg = parse_source(ConfigSourceKind::Opencode, home).unwrap();
        // empty-entry(无 baseURL / apiKey)被跳过;local 空 key 也导入。
        assert_eq!(cfg.providers.len(), 2);
        let ds = cfg
            .providers
            .iter()
            .find(|p| p.label == "imported:opencode:deepseek")
            .unwrap();
        assert_eq!(ds.api_key, "sk-ds-1");
        assert_eq!(ds.base_url.as_deref(), Some("https://api.deepseek.com/v1"));
        assert_eq!(
            ds.model.as_deref(),
            Some("deepseek-chat"),
            "选中模型挂到对应 provider"
        );
        let local = cfg
            .providers
            .iter()
            .find(|p| p.label == "imported:opencode:local")
            .unwrap();
        assert_eq!(local.api_key, "", "空 key(免 key 端点)照常导入");
        assert!(local.model.is_none(), "未选中的 provider 不挂 model");

        // enabled=false 跳过;url 形态 → Http。
        assert_eq!(cfg.mcp_servers.len(), 1);
        assert_eq!(cfg.mcp_servers[0].name, "http-one");
        assert!(matches!(
            cfg.mcp_servers[0].transport,
            ImportedMcpTransport::Http { .. }
        ));
    }

    #[test]
    fn 解析crush与gemini_缺失文件记警告() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // crush 存在,gemini 缺失。
        std::fs::create_dir_all(home.join(".config").join("crush")).unwrap();
        std::fs::write(
            home.join(".config").join("crush").join("config.json"),
            r#"{
              "providers": {
                "anthropic-gw": {"baseUrl": "https://gw.example.com", "apiKey": "sk-c-1"}
              },
              "mcp": {"t": {"command": "tserver"}}
            }"#,
        )
        .unwrap();
        let cfg = parse_source(ConfigSourceKind::Crush, home).unwrap();
        assert_eq!(cfg.providers.len(), 1);
        let p = &cfg.providers[0];
        assert_eq!(p.label, "imported:crush:anthropic-gw");
        assert_eq!(p.base_url.as_deref(), Some("https://gw.example.com"));
        assert!(
            p.anthropic_compatible,
            "key 含 anthropic → anthropic 兼容段"
        );
        assert_eq!(cfg.mcp_servers.len(), 1);

        let gem = parse_source(ConfigSourceKind::Gemini, home).unwrap();
        assert!(gem.providers.is_empty() && gem.mcp_servers.is_empty());
        assert!(gem.warnings.iter().any(|w| w.contains("未找到")));
    }

    #[test]
    fn 坏json文件记警告不整体失败() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join(".gemini")).unwrap();
        std::fs::write(home.join(".gemini").join("settings.json"), "{invalid").unwrap();
        let cfg = parse_source(ConfigSourceKind::Gemini, home).unwrap();
        assert!(cfg.warnings.iter().any(|w| w.contains("解析失败")));
    }
}
