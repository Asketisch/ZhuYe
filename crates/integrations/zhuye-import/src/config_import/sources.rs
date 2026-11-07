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
            // 只回 message,不带 TOML 源码行 —— 出错行可能正是
            // `api_key = "sk-…"`,随 warning 进 stderr / MCP 结果会泄露密钥。
            cfg.push_warning(format!(
                "{} 解析失败({}),已跳过",
                path.display(),
                e.message()
            ));
            Ok(None)
        }
    }
}

/// toml::Value → serde_json::Value(浅递归;日期等特殊类型转字符串)。
fn toml_to_json(v: &toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::Number((*i).into()),
        toml::Value::Float(f) => {
            serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number)
        }
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

/// `s_of` 的 `&str` 版(env 变量等已是字符串的字段)。
fn s_of_val(v: Option<&str>) -> Option<String> {
    v.map(str::to_string).filter(|s| !s.trim().is_empty())
}

/// 归一化 MCP server 定义。各家形状大同小异,但字段来源有差异:
/// - `command`:Claude Code / Codex / crush 为**字符串**;opencode 为
///   **数组**(首元素是可执行文件,其余是 args);
/// - env:`env`(Claude Code / Codex)或 `environment`(opencode);
/// - 远程形态:`url` / `httpUrl`(Gemini:`url` 是 SSE,`httpUrl` 是
///   streamable HTTP)或 `type: "sse"`(opencode)显式声明;
/// - `headers`:远程 MCP 的鉴权头。
///
/// `url_means_sse`:该来源的裸 `url` 字段默认按 SSE 解析(Gemini 如此;
/// 其余来源按 streamable HTTP)。显式 `type: "sse"` 总是优先生效。
fn mcp_entry(
    name: &str,
    v: &Value,
    cfg: &mut ImportedAgentConfig,
    url_means_sse: bool,
) -> Option<ImportedMcpServer> {
    use std::collections::HashMap;

    let obj = v.as_object()?;
    // enabled=false(opencode)显式跳过。
    if obj.get("enabled").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let type_str = s_of(obj.get("type"));
    // env 取 `env` 或 `environment`(opencode 别名)。
    let env_map = |obj: &serde_json::Map<String, Value>| -> HashMap<String, String> {
        obj.get("env")
            .or_else(|| obj.get("environment"))
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default()
    };
    let headers = |obj: &serde_json::Map<String, Value>| -> HashMap<String, String> {
        obj.get("headers")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default()
    };
    let transport = if let Some(url) = s_of(obj.get("httpUrl")).or_else(|| s_of(obj.get("url"))) {
        let is_sse =
            type_str.as_deref() == Some("sse") || (url_means_sse && !obj.contains_key("httpUrl"));
        ImportedMcpTransport::Http {
            url,
            headers: headers(obj),
            sse: is_sse,
        }
    } else if let Some((command, cmd_args)) = command_of(obj) {
        let args = obj
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        ImportedMcpTransport::Stdio {
            command,
            args: cmd_args.unwrap_or(args),
            env: env_map(obj),
        }
    } else {
        cfg.push_warning(format!("mcp server `{name}` 既无 command 也无 url,跳过"));
        return None;
    };
    Some(ImportedMcpServer {
        name: name.to_string(),
        transport,
    })
}

/// 解析 `command` 字段:字符串形态 → `(command, None)`;数组形态
/// (opencode)→ `(首元素, Some(其余为 args))`。
fn command_of(obj: &serde_json::Map<String, Value>) -> Option<(String, Option<Vec<String>>)> {
    match obj.get("command")? {
        Value::String(s) if !s.trim().is_empty() => Some((s.clone(), None)),
        Value::Array(a) => {
            let parts: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
            let (cmd, rest) = parts.split_first()?;
            if cmd.trim().is_empty() {
                return None;
            }
            Some((
                cmd.to_string(),
                Some(rest.iter().map(|s| s.to_string()).collect()),
            ))
        }
        _ => None,
    }
}

/// 遍历 `{name: {...}}` 形状的 MCP 表。
fn collect_mcp_map(map: Option<&Value>, cfg: &mut ImportedAgentConfig, url_means_sse: bool) {
    let Some(obj) = map.and_then(Value::as_object) else {
        return;
    };
    for (name, v) in obj {
        match mcp_entry(name, v, cfg, url_means_sse) {
            Some(server) => cfg.mcp_servers.push(server),
            None => continue,
        }
    }
}

/// 判定是否为 Anthropic 兼容端点(落 `[[anthropic.credentials]]`)。
///
/// 仅用于 opencode / crush 这类**用户自定义** provider(显式协议字段
/// 优先):opencode 用 `npm` 包名(`@ai-sdk/anthropic`),crush 用显式
/// `type`(`"anthropic"` / `"messages"`)。名字 / 模型前缀启发式只作
/// 兜底并记 warning —— 按名字硬判曾把 OpenAI 兼容网关错误落进
/// anthropic 段,请求必然失败。
fn is_anthropic_compatible(
    key: &str,
    base_url: Option<&str>,
    model: Option<&str>,
    explicit: Option<&str>,
    cfg: &mut ImportedAgentConfig,
) -> bool {
    // 显式协议声明。
    if let Some(t) = explicit {
        let t = t.to_ascii_lowercase();
        if t.contains("anthropic") || t == "messages" {
            return true;
        }
        if t.contains("openai") || t == "completions" || t == "responses" || t == "chat" {
            return false;
        }
    }
    // 启发式兜底(带警告)。
    let guessed = key.contains("anthropic")
        || key.contains("claude")
        || base_url.is_some_and(|u| u.contains("anthropic"))
        || model.is_some_and(|m| m.starts_with("claude-"));
    if guessed {
        cfg.push_warning(format!(
            "provider `{key}` 无显式协议声明,按名字推断为 anthropic 兼容;若实际是 OpenAI 兼容端点请手动改到 [[openai.credentials]]"
        ));
    }
    guessed
}

// ── Claude Code ──────────────────────────────────────────────────────
//
// 两个文件:
// - ~/.claude/settings.json:env.ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN /
//   ANTHROPIC_BASE_URL / ANTHROPIC_MODEL、model
// - ~/.claude.json:顶层 mcpServers、primaryApiKey(legacy)

fn parse_claude_code(home: &Path, cfg: &mut ImportedAgentConfig) -> Result<(), String> {
    let mut found_any = false;

    let settings_path = home.join(".claude").join("settings.json");
    if let Some(settings) = read_json(&settings_path, cfg)? {
        found_any = true;
        let env = settings.get("env").and_then(Value::as_object);
        let get_env = |k: &str| env.and_then(|m| m.get(k)).and_then(Value::as_str);
        // key 取 API_KEY,回落 AUTH_TOKEN —— 国内网关(GLM / Kimi / MiniMax
        // 等)官方接入指引普遍用 `ANTHROPIC_AUTH_TOKEN` +
        // `ANTHROPIC_BASE_URL`;AnthropicClient 两种 token 都会发认证头。
        let api_key = get_env("ANTHROPIC_API_KEY")
            .filter(|s| !s.trim().is_empty())
            .or_else(|| get_env("ANTHROPIC_AUTH_TOKEN"))
            .unwrap_or_default()
            .to_string();
        let base_url = get_env("ANTHROPIC_BASE_URL").map(str::to_string);
        // model:env ANTHROPIC_MODEL 优先(Claude Code 自身也这么解析),
        // 回落 settings 顶层 `model` 字段。
        let env_model = s_of_val(get_env("ANTHROPIC_MODEL"));
        let model = env_model.or_else(|| s_of(settings.get("model")));
        if !api_key.is_empty() || base_url.is_some() || model.is_some() {
            cfg.providers.push(ImportedProvider {
                label: "imported:claude-code:anthropic".into(),
                api_key,
                base_url,
                model,
                anthropic_compatible: true,
            });
        }
        collect_mcp_map(settings.get("mcpServers"), cfg, false);
    }

    let claude_json_path = home.join(".claude.json");
    if let Some(cj) = read_json(&claude_json_path, cfg)? {
        found_any = true;
        collect_mcp_map(cj.get("mcpServers"), cfg, false);
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
            // 落成免 key 形态,由用户后续补 —— 但要明说,静默空 key 只会在
            // 运行时表现为 401)。
            let api_key = env_key
                .as_deref()
                .and_then(|k| std::env::var(k).ok())
                .filter(|s| !s.is_empty())
                .unwrap_or_default();
            if api_key.is_empty() {
                if let Some(k) = &env_key {
                    cfg.push_warning(format!(
                        "codex: provider `{key}` 的 env_key `{k}` 在当前环境不存在,导入为空 key"
                    ));
                }
            }
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
            // Codex 只以 OpenAI wire(chat/completions / responses)访问
            // model_providers.*;名字 / 模型带 "anthropic"/"claude" 不代表
            // 端点讲 Anthropic Messages 协议 —— 按名字启发式落 anthropic 段
            // 会用错协议必然失败(曾经如此)。
            cfg.providers.push(ImportedProvider {
                label: format!("imported:codex:{key}"),
                api_key,
                base_url,
                model,
                anthropic_compatible: false,
            });
        }
        // 激活 provider 是内建 `openai`(不在 model_providers 表里)且表
        // 非空时,上面的循环不会为它生成条目,顶层 model 会丢 —— 补一条
        // 无 base_url 的 openai 条目让模型选择生效。
        if active_provider.as_deref() == Some("openai") && top_model.is_some() {
            let exists = cfg
                .providers
                .iter()
                .any(|p| p.label == "imported:codex:openai");
            if !exists {
                cfg.providers.push(ImportedProvider {
                    label: "imported:codex:openai".into(),
                    api_key: std::env::var("OPENAI_API_KEY").unwrap_or_default(),
                    base_url: None,
                    // 本分支是 top_model 的最后使用,move 即可。
                    model: top_model,
                    anthropic_compatible: false,
                });
            }
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
    collect_mcp_map(v.get("mcp_servers"), cfg, false);
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
    collect_mcp_map(v.get("mcpServers"), cfg, true);
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
        // 非 NotFound 的读错误(权限 / EISDIR 等)降级为 warning 后继续
        // 尝试下一候选,不让一个坏候选挡住另一个。
        match read_json(&path, cfg) {
            Ok(Some(v)) => {
                parsed = true;
                parse_opencode_like(&v, cfg, "opencode");
                break;
            }
            Ok(None) => continue,
            Err(e) => {
                cfg.push_warning(e);
                continue;
            }
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
    // crush 全局配置的规范路径是 `~/.config/crush/crush.json`
    // (XDG 配置目录 + `<app>/<app>.json`);`config.json` 是本导入器早期
    // 版本写错的路径,保留作兼容回落。
    let canonical = home.join(".config").join("crush").join("crush.json");
    let legacy = home.join(".config").join("crush").join("config.json");
    let path = if canonical.exists() {
        &canonical
    } else {
        &legacy
    };
    let Some(v) = read_json(path, cfg)? else {
        cfg.push_warning("crush: 未找到 ~/.config/crush/crush.json");
        return Ok(());
    };
    parse_crush_config(&v, cfg);
    Ok(())
}

/// opencode 风格配置。
///
/// provider 表的键在 V1 是 `provider`、V2 是 `providers`;baseURL / apiKey
/// 的真实位置在 `options.*`(V1)/ `settings.*`(V2),而不是 def 顶层
/// —— 只读顶层的话一个自定义 provider 都导不出来(此前如此)。顶层
/// 旧位置保留作回落。
fn parse_opencode_like(v: &Value, cfg: &mut ImportedAgentConfig, agent: &str) {
    let selected = s_of(v.get("model"));
    let table = v
        .get("provider")
        .or_else(|| v.get("providers"))
        .and_then(Value::as_object);
    if let Some(providers) = table {
        for (key, def) in providers {
            let opt = def.get("options").or_else(|| def.get("settings"));
            let base_url = opt
                .and_then(|o| s_of(o.get("baseURL")))
                .or_else(|| opt.and_then(|o| s_of(o.get("base_url"))))
                .or_else(|| s_of(def.get("baseURL")))
                .or_else(|| s_of(def.get("base_url")))
                .or_else(|| s_of(def.get("baseUrl")));
            let api_key = opt
                .and_then(|o| s_of(o.get("apiKey")))
                .or_else(|| opt.and_then(|o| s_of(o.get("api_key"))))
                .or_else(|| s_of(def.get("apiKey")))
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
            // opencode 的显式协议声明在 `npm` 包名(如 `@ai-sdk/anthropic`)。
            let npm = s_of(def.get("npm"));
            let anthropic = is_anthropic_compatible(
                key,
                base_url.as_deref(),
                model.as_deref(),
                npm.as_deref(),
                cfg,
            );
            cfg.providers.push(ImportedProvider {
                label: format!("imported:{agent}:{key}"),
                api_key,
                base_url,
                model,
                anthropic_compatible: anthropic,
            });
        }
    }
    // MCP 表:opencode V1 在 `mcp` 顶层,V2 下沉到 `mcp.servers`。
    let mcp_table = v.get("mcp").and_then(|m| m.get("servers").or(Some(m)));
    collect_mcp_map(mcp_table, cfg, false);
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
            // crush 的 provider 带显式 `type`("anthropic"/"messages" vs
            // "openai"/"completions"/"responses")。
            let explicit = s_of(def.get("type"));
            let anthropic = is_anthropic_compatible(
                key,
                base_url.as_deref(),
                model.as_deref(),
                explicit.as_deref(),
                cfg,
            );
            cfg.providers.push(ImportedProvider {
                label: format!("imported:crush:{key}"),
                api_key,
                base_url,
                model,
                anthropic_compatible: anthropic,
            });
        }
    }
    collect_mcp_map(v.get("mcp"), cfg, false);
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

    /// 真实 opencode 形状:baseURL / apiKey 在 `options.*`、`npm` 声明协议、
    /// MCP `command` 为数组且 env 键叫 `environment`、`type: "sse"` 显式。
    #[test]
    fn 解析opencode真实形状_options与数组command() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join(".config").join("opencode")).unwrap();
        std::fs::write(
            home.join(".config").join("opencode").join("opencode.json"),
            r#"{
              "model": "gw/claude-sonnet-4",
              "provider": {
                "gw": {
                  "npm": "@ai-sdk/anthropic",
                  "options": {"baseURL": "https://gw.example.com", "apiKey": "sk-gw"}
                },
                "oai": {
                  "npm": "@ai-sdk/openai-compatible",
                  "options": {"baseURL": "https://oai.example.com/v1", "apiKey": "sk-oai"}
                },
                "claude-named-but-openai": {
                  "npm": "@ai-sdk/openai-compatible",
                  "options": {"baseURL": "https://router.example.com/v1", "apiKey": "sk-r"}
                }
              },
              "mcp": {
                "local-one": {
                  "type": "local",
                  "command": ["bun", "run", "server.ts"],
                  "environment": {"TOKEN": "t"}
                },
                "remote-sse": {"type": "sse", "url": "https://mcp.example/sse", "headers": {"Authorization": "Bearer x"}}
              }
            }"#,
        )
        .unwrap();
        let cfg = parse_source(ConfigSourceKind::Opencode, home).unwrap();
        let by_label: std::collections::HashMap<_, _> = cfg
            .providers
            .iter()
            .map(|p| (p.label.as_str(), p))
            .collect();
        let gw = by_label["imported:opencode:gw"];
        assert_eq!(gw.api_key, "sk-gw", "options.apiKey 应被读取");
        assert_eq!(gw.base_url.as_deref(), Some("https://gw.example.com"));
        assert_eq!(gw.model.as_deref(), Some("claude-sonnet-4"));
        assert!(
            gw.anthropic_compatible,
            "npm @ai-sdk/anthropic → anthropic 段"
        );
        let oai = by_label["imported:opencode:oai"];
        assert!(!oai.anthropic_compatible);
        // 名字含 claude 但 npm 显式 openai-compatible → 不落 anthropic。
        let named = by_label["imported:opencode:claude-named-but-openai"];
        assert!(
            !named.anthropic_compatible,
            "显式协议声明必须覆盖名字启发式"
        );

        let mcp: std::collections::HashMap<_, _> = cfg
            .mcp_servers
            .iter()
            .map(|s| (s.name.as_str(), s))
            .collect();
        match &mcp["local-one"].transport {
            ImportedMcpTransport::Stdio { command, args, env } => {
                assert_eq!(command, "bun", "数组 command 首元素是可执行文件");
                assert_eq!(args, &vec!["run".to_string(), "server.ts".to_string()]);
                assert_eq!(env["TOKEN"], "t", "environment 应作为 env 别名");
            }
            other => panic!("expected stdio, got {other:?}"),
        }
        match &mcp["remote-sse"].transport {
            ImportedMcpTransport::Http { url, headers, sse } => {
                assert_eq!(url, "https://mcp.example/sse");
                assert!(*sse, "type: sse 应映射为 SSE 传输");
                assert_eq!(headers["Authorization"], "Bearer x", "鉴权头不得丢弃");
            }
            other => panic!("expected http, got {other:?}"),
        }
    }

    /// opencode V2 形状:`providers` 键 + `settings.*`、MCP 下沉到 `mcp.servers`。
    #[test]
    fn 解析opencode_v2形状() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join(".config").join("opencode")).unwrap();
        std::fs::write(
            home.join(".config").join("opencode").join("opencode.json"),
            r#"{
              "providers": {"p": {"settings": {"baseURL": "https://p.example/v1", "apiKey": "k"}}},
              "mcp": {"servers": {"s": {"command": ["srv"]}}}
            }"#,
        )
        .unwrap();
        let cfg = parse_source(ConfigSourceKind::Opencode, home).unwrap();
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(
            cfg.providers[0].base_url.as_deref(),
            Some("https://p.example/v1")
        );
        assert_eq!(cfg.mcp_servers.len(), 1);
        assert_eq!(cfg.mcp_servers[0].name, "s");
    }

    /// Claude Code 网关主流配法:`ANTHROPIC_AUTH_TOKEN` + `ANTHROPIC_BASE_URL`,
    /// `ANTHROPIC_MODEL` 优先于顶层 `model`。
    #[test]
    fn 解析claude_code_auth_token回落与env_model优先() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(
            home.join(".claude").join("settings.json"),
            r#"{
              "env": {
                "ANTHROPIC_AUTH_TOKEN": "tok-glm",
                "ANTHROPIC_BASE_URL": "https://open.bigmodel.cn/api/anthropic",
                "ANTHROPIC_MODEL": "glm-4.6"
              },
              "model": "sonnet"
            }"#,
        )
        .unwrap();
        let cfg = parse_source(ConfigSourceKind::ClaudeCode, home).unwrap();
        let p = &cfg.providers[0];
        assert_eq!(p.api_key, "tok-glm", "API_KEY 缺失时回落 AUTH_TOKEN");
        assert_eq!(
            p.model.as_deref(),
            Some("glm-4.6"),
            "env ANTHROPIC_MODEL 优先"
        );
    }

    /// Codex 一律 OpenAI wire:名字 / 模型带 claude 也不落 anthropic 段;
    /// env_key 缺失要有 warning;内建 openai 为激活项时顶层 model 不丢。
    #[test]
    fn 解析codex_不按名字落anthropic_且env_key缺失有警告() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex").join("config.toml"),
            r#"
            model = "gpt-5"
            model_provider = "openai"

            [model_providers.anthropic-router]
            base_url = "https://router.example.com/v1"
            env_key = "ZZ_DEFINITELY_UNSET_KEY_42"
            "#,
        )
        .unwrap();
        let cfg = parse_source(ConfigSourceKind::Codex, home).unwrap();
        let router = cfg
            .providers
            .iter()
            .find(|p| p.label == "imported:codex:anthropic-router")
            .expect("router provider");
        assert!(
            !router.anthropic_compatible,
            "Codex provider 永远是 OpenAI wire"
        );
        assert_eq!(router.api_key, "");
        assert!(
            cfg.warnings
                .iter()
                .any(|w| w.contains("ZZ_DEFINITELY_UNSET_KEY_42")),
            "env_key 缺失应告知:{:?}",
            cfg.warnings
        );
        // 内建 openai 激活 + 表非空 → 仍导入顶层 model。
        let builtin = cfg
            .providers
            .iter()
            .find(|p| p.label == "imported:codex:openai")
            .expect("builtin openai provider");
        assert_eq!(builtin.model.as_deref(), Some("gpt-5"));
    }

    /// crush 规范文件名 crush.json;显式 `type` 决定协议。
    #[test]
    fn 解析crush_json规范路径与显式type() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join(".config").join("crush")).unwrap();
        std::fs::write(
            home.join(".config").join("crush").join("crush.json"),
            r#"{
              "providers": {
                "claude-ish": {"type": "openai", "base_url": "https://oai.example/v1", "api_key": "k1"},
                "plain": {"type": "anthropic", "base_url": "https://a.example", "api_key": "k2"}
              }
            }"#,
        )
        .unwrap();
        let cfg = parse_source(ConfigSourceKind::Crush, home).unwrap();
        let by_label: std::collections::HashMap<_, _> = cfg
            .providers
            .iter()
            .map(|p| (p.label.as_str(), p))
            .collect();
        assert!(
            !by_label["imported:crush:claude-ish"].anthropic_compatible,
            "type: openai 覆盖名字启发式"
        );
        assert!(by_label["imported:crush:plain"].anthropic_compatible);
    }

    /// Gemini:`url` 是 SSE、`httpUrl` 是 streamable HTTP。
    #[test]
    fn 解析gemini的http_url与url语义() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join(".gemini")).unwrap();
        std::fs::write(
            home.join(".gemini").join("settings.json"),
            r#"{"mcpServers": {
              "streamable": {"httpUrl": "https://s.example/mcp"},
              "legacy-sse": {"url": "https://e.example/sse"}
            }}"#,
        )
        .unwrap();
        let cfg = parse_source(ConfigSourceKind::Gemini, home).unwrap();
        let mcp: std::collections::HashMap<_, _> = cfg
            .mcp_servers
            .iter()
            .map(|s| (s.name.as_str(), s))
            .collect();
        assert!(matches!(
            &mcp["streamable"].transport,
            ImportedMcpTransport::Http { sse: false, .. }
        ));
        assert!(matches!(
            &mcp["legacy-sse"].transport,
            ImportedMcpTransport::Http { sse: true, .. }
        ));
    }

    /// TOML 解析失败的 warning 不得携带源码行(可能含密钥)。
    #[test]
    fn toml解析失败_警告不泄露源码行() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex").join("config.toml"),
            "[model_providers.x]\nenv_key = \"K\"\napi_key_leak = \"sk-LEAKME\n",
        )
        .unwrap();
        let cfg = parse_source(ConfigSourceKind::Codex, home).unwrap();
        let w = cfg
            .warnings
            .iter()
            .find(|w| w.contains("解析失败"))
            .expect("应有解析失败 warning");
        assert!(!w.contains("sk-LEAKME"), "警告泄露了源码行:{w}");
    }
}
