//! MCP stdio server 模式(`zhuye-import mcp`)。
//!
//! 与项目风格一致(`mock_mcp_server.rs`、`zhuye-lsp` transport):手写
//! JSON-RPC 2.0 over stdio(line-delimited JSON),不引入 rmcp server
//! 侧依赖。协议版本与 `zhuye-mcp` 客户端对齐(`2025-06-18`)。
//!
//! 暴露两个工具:
//! - `import_session`:导入一个文件(或目录下全部可识别 JSONL)到本地
//!   session 存储(`$HOME/.zhuye/sessions`);
//! - `list_importable`:扫描指定 Harness 的默认安装位置,列出可导入源。
//!
//! 日志一律走 stderr;stdout 只输出 JSON-RPC 响应(传输通道不可污染)。

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::error::ImportError;
use crate::model::{HarnessKind, ImportOptions, ImportSource};
use crate::{discover, import_all};

const PROTOCOL_VERSION: &str = "2025-06-18";

/// 工具清单(JSON Schema 形态,MCP `tools/list` 结果)。
fn tools_json() -> Value {
    json!({
        "tools": [
            {
                "name": "import_session",
                "description": "把外部 Harness(Claude Code / Codex / ZCode)历史会话导入为烛夜会话。path 可以是单个 JSONL 文件或目录(目录时递归收集全部 .jsonl 逐个嗅探导入);source_kind 为 auto 时自动识别格式。导入后可用 zhuye exec --resume <thread_id> 续跑。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "会话文件或目录路径"},
                        "source_kind": {"type": "string", "enum": ["auto", "claude-code", "codex", "zcode"], "description": "来源种类,默认 auto"},
                        "dry_run": {"type": "boolean", "description": "只解析不落盘"},
                        "include_thinking": {"type": "boolean", "description": "保留思考链(默认丢弃)"},
                        "keep_injected": {"type": "boolean", "description": "保留 Harness 注入的环境脚手架(默认过滤)"},
                        "force": {"type": "boolean", "description": "覆盖同 id 已存在的会话(默认跳过)"}
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "list_importable",
                "description": "扫描指定 Harness 的本机默认安装位置,列出全部可导入的会话来源(Claude Code ~ n/.claude/projects、Codex ~/.codex/sessions、ZCode ~/.zcode/cli/db/db.sqlite)。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source_kind": {"type": "string", "enum": ["claude-code", "codex", "zcode"]}
                    },
                    "required": ["source_kind"]
                }
            },
            {
                "name": "import_config",
                "description": "一键导入其他主流本地 agent(Claude Code / Codex / Gemini / opencode / crush)的模型 provider 与 MCP server 配置到烛夜 config.toml。只做增量合并:已有同名 MCP / 同 label 凭证跳过,不覆盖用户已有配置。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source_kind": {"type": "string", "enum": ["auto", "claude-code", "codex", "gemini", "opencode", "crush"], "description": "来源种类,默认 auto(全部已知来源)"},
                        "dry_run": {"type": "boolean", "description": "只报告不写入"},
                        "home": {"type": "string", "description": "来源 HOME 根目录(默认 $HOME)"},
                        "output": {"type": "string", "description": "目标 config.toml 路径(默认 ~/.zhuye/config.toml)"}
                    }
                }
            }
        ]
    })
}

/// 阻塞式主循环:逐行读 JSON-RPC 请求、写响应。stdin 关闭即退出。
pub fn run() {
    // 导入目标:默认 `$HOME/.zhuye/sessions`(显式传参让测试免碰 HOME env)。
    let base = zhuye_rollout::path::default_base();
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
            let err = json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {"code": -32700, "message": "parse error"},
            });
            writeln!(stdout, "{err}").ok();
            stdout.flush().ok();
            continue;
        };
        // 通知(无 id)不响应。
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result = handle_request(method, &params, &base);
        let resp = json!({"jsonrpc": "2.0", "id": id, "result": result});
        writeln!(stdout, "{resp}").ok();
        stdout.flush().ok();
    }
}

fn handle_request(method: &str, params: &Value, base: &std::path::Path) -> Value {
    match method {
        "initialize" => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "serverInfo": {
                "name": "zhuye-import",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {"tools": {"listChanged": false}},
        }),
        "tools/list" => tools_json(),
        "tools/call" => dispatch_tool(params, base),
        "ping" => json!({}),
        other => json!({
            "content": [{"type": "text", "text": format!("method not implemented: {other}")}],
            "isError": true,
        }),
    }
}

fn dispatch_tool(params: &Value, base: &std::path::Path) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let result = match name {
        "import_session" => tool_import_session(&args, base),
        "list_importable" => tool_list_importable(&args),
        "import_config" => tool_import_config(&args),
        other => Err(format!("unknown tool: {other}")),
    };
    match result {
        Ok(text) => json!({
            "content": [{"type": "text", "text": text}],
            "isError": false,
        }),
        Err(err) => json!({
            "content": [{"type": "text", "text": err}],
            "isError": true,
        }),
    }
}

fn parse_kind(args: &Value, allow_auto: bool) -> Result<Option<HarnessKind>, String> {
    let raw = args
        .get("source_kind")
        .and_then(Value::as_str)
        .unwrap_or(if allow_auto { "auto" } else { "" });
    if raw.is_empty() || raw == "auto" {
        if allow_auto {
            return Ok(None);
        }
        return Err("source_kind 必填:claude-code / codex / zcode".into());
    }
    HarnessKind::parse(raw)
        .map(Some)
        .ok_or_else(|| format!("未知 source_kind: {raw}"))
}

fn parse_opts(args: &Value) -> ImportOptions {
    ImportOptions {
        include_thinking: args
            .get("include_thinking")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        keep_injected: args
            .get("keep_injected")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        include_sidechain: false,
    }
}

fn tool_import_session(args: &Value, base: &std::path::Path) -> Result<String, String> {
    let path_raw = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or("缺少 path 参数")?;
    let path = PathBuf::from(path_raw);
    let kind = parse_kind(args, true)?;
    let opts = parse_opts(args);
    let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
    let dry_run = args
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // 文件 → 单源;目录 → 递归收集 .jsonl(zcode 的 sqlite 不走此入口,
    // 用 list_importable + 显式 key 场景处理;此处保持文件/目录语义)。
    let sources: Vec<ImportSource> = if path.is_file() {
        vec![ImportSource::JsonlFile(path)]
    } else if path.is_dir() {
        let mut out = Vec::new();
        collect_jsonl(&path, &mut out, 0);
        if out.is_empty() {
            return Err(format!("目录中没有 .jsonl 文件: {}", path.display()));
        }
        out
    } else {
        return Err(format!("路径不存在: {}", path.display()));
    };

    let (ok, failed) = import_all(base, &sources, kind, &opts, force, dry_run);
    // 幂等冲突单列:skip 是预期行为。
    let (skipped, errors): (Vec<_>, Vec<_>) = failed
        .into_iter()
        .partition(|(_, e)| matches!(e, ImportError::AlreadyExists(_)));

    // 仅剩真实错误时暴露为 isError(幂等 skip 以行呈现让 LLM 可续做)。
    if ok.is_empty() && errors.len() == 1 {
        return Err(format!("导入失败: {}", errors[0].1));
    }

    let mut lines = Vec::new();
    for o in &ok {
        lines.push(o.summary_line());
    }
    for (src, _) in &skipped {
        lines.push(format!("skipped {src}: 目标会话已存在(加 force 覆盖)"));
    }
    for (src, e) in &errors {
        lines.push(format!("failed {src}: {e}"));
    }
    let skip_note = if !skipped.is_empty() || !errors.is_empty() {
        format!("(其中 {} 个跳过、{} 个失败)", skipped.len(), errors.len())
    } else {
        String::new()
    };
    lines.push(format!(
        "完成:{} 个导入{};续跑:zhuye exec --resume <thread_id>",
        ok.len(),
        skip_note
    ));
    Ok(lines.join("\n"))
}

fn tool_list_importable(args: &Value) -> Result<String, String> {
    let kind = parse_kind(args, false)?;
    let kind = kind.expect("parse_kind(false) 已确保非 auto");
    let sources = discover(kind).map_err(|e| e.to_string())?;
    if sources.is_empty() {
        return Ok(format!("{kind}:未发现可导入会话(可能未安装或历史为空)"));
    }
    let mut lines = vec![format!("{kind}:发现 {} 个来源", sources.len())];
    for (i, s) in sources.iter().take(500).enumerate() {
        lines.push(format!("{:>4}. {}", i + 1, s.display()));
    }
    if sources.len() > 500 {
        lines.push(format!("…(其余 {} 个省略)", sources.len() - 500));
    }
    Ok(lines.join("\n"))
}

/// v1.10 功能1:配置导入 MCP 工具(与 CLI `zhuye-import config` 同一管线)。
fn tool_import_config(args: &Value) -> Result<String, String> {
    use crate::config_import::{self, merge};

    let raw_kind = args
        .get("source_kind")
        .and_then(Value::as_str)
        .unwrap_or("auto");
    let kinds: Vec<config_import::ConfigSourceKind> = if raw_kind == "auto" {
        config_import::ConfigSourceKind::all().to_vec()
    } else {
        let k = config_import::ConfigSourceKind::parse(raw_kind)
            .ok_or_else(|| format!("未知 source_kind: {raw_kind}"))?;
        vec![k]
    };
    let home = args
        .get("home")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .ok_or("无法确定来源 HOME")?;
    let target = args
        .get("output")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(zhuye_config::default_config_path)
        .ok_or("无法确定目标 config.toml 路径")?;
    let dry_run = args
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let (mut cfg, load_warnings) = merge::load_target(&target);
    let (mut imported_list, mut warnings) = (Vec::new(), load_warnings);
    for kind in kinds {
        match config_import::sources::parse_source(kind, &home) {
            Ok(c) => imported_list.push(c),
            Err(e) => warnings.push(format!("{}: {e}", kind.as_str())),
        }
    }
    let mut report = merge::MergeReport::default();
    for imported in &imported_list {
        let r = merge::merge_into_config(&mut cfg, imported);
        report.providers_added.extend(r.providers_added);
        report.providers_skipped.extend(r.providers_skipped);
        report.mcp_added.extend(r.mcp_added);
        report.mcp_skipped.extend(r.mcp_skipped);
        report.warnings.extend(r.warnings);
    }
    report.warnings.extend(warnings);

    let mut lines = vec![format!("目标配置:{}", target.display())];
    for l in &report.providers_added {
        lines.push(format!("  + provider 凭证:{l}"));
    }
    for l in &report.providers_skipped {
        lines.push(format!("  = provider 凭证(已存在,跳过):{l}"));
    }
    for n in &report.mcp_added {
        lines.push(format!("  + mcp server:{n}"));
    }
    for n in &report.mcp_skipped {
        lines.push(format!("  = mcp server(已存在,跳过):{n}"));
    }
    if report.is_empty() && report.warnings.is_empty() {
        lines.push("未发现可导入的配置(未安装这些 agent 或配置为空)".into());
    }
    for w in &report.warnings {
        lines.push(format!("  ! {w}"));
    }
    if dry_run {
        lines.push("(dry-run:以上内容未写入)".into());
    } else {
        merge::write_target(&target, &cfg)?;
        lines.push(format!("已写入:{}", target.display()));
    }
    Ok(lines.join("\n"))
}

/// 递归收集目录下的 .jsonl(深度上限 8,防御符号循环)。
fn collect_jsonl(dir: &std::path::Path, out: &mut Vec<ImportSource>, depth: usize) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        if p.is_dir() {
            collect_jsonl(&p, out, depth + 1);
        } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
            out.push(ImportSource::JsonlFile(p));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// initialize / tools/list / ping / 未知方法的基本响应面。
    #[test]
    fn request_surface() {
        let init = handle_request(
            "initialize",
            &json!({"protocolVersion": "2025-06-18", "capabilities": {}}),
            std::path::Path::new("/tmp/unused"),
        );
        assert_eq!(init["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(init["serverInfo"]["name"], "zhuye-import");

        let tools = handle_request(
            "tools/list",
            &Value::Null,
            std::path::Path::new("/tmp/unused"),
        );
        let names: Vec<&str> = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["import_session", "list_importable", "import_config"]
        );

        assert!(
            handle_request("ping", &Value::Null, std::path::Path::new("/tmp/unused")).is_object()
        );
        let unknown = handle_request(
            "resources/list",
            &Value::Null,
            std::path::Path::new("/tmp/unused"),
        );
        assert_eq!(unknown["isError"], true);
    }

    /// import_session:单文件导入 + 幂等;不经 HOME env(显式传 base)。
    #[test]
    fn import_tool_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("sessions");
        let src = dir.path().join("s.jsonl");
        std::fs::write(
            &src,
            r#"{"type":"user","message":{"role":"user","content":"问"},"sessionId":"ef9ee90f-0000-4000-8000-000000000005","uuid":"u","timestamp":"2026-06-01T10:00:00Z","cwd":"/w"}"#,
        )
        .unwrap();

        let out = dispatch_tool(
            &json!({
                "name": "import_session",
                "arguments": {"path": src.display().to_string(), "source_kind": "auto"}
            }),
            &base,
        );
        assert_eq!(out["isError"], false, "{out}");
        let text = out["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("imported"), "{text}");

        // 重复导入:failed 行出现在结果里(幂等跳过),仍不是 isError。
        let out2 = dispatch_tool(
            &json!({
                "name": "import_session",
                "arguments": {"path": src.display().to_string(), "source_kind": "auto"}
            }),
            &base,
        );
        let text2 = out2["content"][0]["text"].as_str().unwrap();
        assert!(text2.contains("skipped"), "{text2}");
        assert!(text2.contains("已存在"));
    }

    /// v1.10 功能1:import_config 端到端(临时 HOME + 显式 output)。
    #[test]
    fn import_config_tool_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let output = dir.path().join("out").join("config.toml");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(
            home.join(".claude").join("settings.json"),
            r#"{"env": {"ANTHROPIC_API_KEY": "sk-ant-x"}, "model": "claude-sonnet-4-latest"}"#,
        )
        .unwrap();

        let out = dispatch_tool(
            &json!({
                "name": "import_config",
                "arguments": {
                    "source_kind": "claude-code",
                    "home": home.display().to_string(),
                    "output": output.display().to_string()
                }
            }),
            std::path::Path::new("/tmp/unused"),
        );
        assert_eq!(out["isError"], false, "{out}");
        let text = out["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("imported:claude-code:anthropic"), "{text}");

        // 落盘字段级校验。
        let body = std::fs::read_to_string(&output).unwrap();
        let cfg: zhuye_config::ReflectConfig = toml::from_str(&body).unwrap();
        let anth = cfg.anthropic.as_ref().unwrap();
        assert_eq!(anth.credentials.len(), 1);
        assert_eq!(anth.credentials[0].label, "imported:claude-code:anthropic");
        assert_eq!(anth.credentials[0].api_key, "sk-ant-x");
        assert_eq!(
            anth.credentials[0].model.as_deref(),
            Some("claude-sonnet-4-latest")
        );

        // 再跑一次:幂等跳过。
        let out2 = dispatch_tool(
            &json!({
                "name": "import_config",
                "arguments": {
                    "source_kind": "claude-code",
                    "home": home.display().to_string(),
                    "output": output.display().to_string()
                }
            }),
            std::path::Path::new("/tmp/unused"),
        );
        let text2 = out2["content"][0]["text"].as_str().unwrap();
        assert!(text2.contains("已存在,跳过"), "{text2}");
    }

    /// list_importable 缺 kind 报错文案。
    #[test]
    fn list_tool_requires_kind() {
        let out = dispatch_tool(
            &json!({
                "name": "list_importable",
                "arguments": {}
            }),
            std::path::Path::new("/tmp/unused"),
        );
        assert_eq!(out["isError"], true);
        assert!(
            out["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("source_kind 必填")
        );
    }
}
