//! `zhuye-kms-server` — KMS 知识库的 MCP stdio server(plugins/kms 的
//! 能力本体,harness-import 同范式:手写 JSON-RPC 2.0 over stdio,
//! line-delimited;stdout 只输出协议响应,日志一律 stderr)。
//!
//! 工具面:`kms_list_wikis` / `kms_get_page` / `kms_list_pages` /
//! `kms_search` / `kms_create_wiki` / `kms_save_page` / `kms_delete_wiki` /
//! `kms_dream`(读 Auto 语义由宿主权限层决定;MCP 工具天然 Prompt 下限)。
//! 存储走 `zhuye_kms::KnowledgeManager`(`~/.zhuye/kms/<wiki>/`),unix
//! 下 openat 语义防 TOCTOU / 目录替换攻击。

use std::io::{self, BufRead, Write};

use serde_json::{Value, json};
use zhuye_kms::KnowledgeManager;

const PROTOCOL_VERSION: &str = "2025-06-18";

/// 工具清单(MCP `tools/list` 结果)。
fn tools_json() -> Value {
    json!({
        "tools": [
            {
                "name": "kms_list_wikis",
                "description": "列出全部知识库(名称 / 描述 / 页数 / 最近修改时间)。",
                "inputSchema": {"type": "object", "properties": {}}
            },
            {
                "name": "kms_list_pages",
                "description": "列出某知识库的全部页面(名称 / 标题 / 标签)。",
                "inputSchema": {
                    "type": "object",
                    "properties": {"wiki": {"type": "string", "description": "知识库名"}},
                    "required": ["wiki"]
                }
            },
            {
                "name": "kms_get_page",
                "description": "读取某知识库的指定页面(含 frontmatter 解析出的 title / tags)。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "wiki": {"type": "string"},
                        "page": {"type": "string"}
                    },
                    "required": ["wiki", "page"]
                }
            },
            {
                "name": "kms_search",
                "description": "跨全部知识库按子串检索(逐行命中,返回 wiki/page/命中行)。",
                "inputSchema": {
                    "type": "object",
                    "properties": {"query": {"type": "string", "description": "检索词"}},
                    "required": ["query"]
                }
            },
            {
                "name": "kms_create_wiki",
                "description": "创建新知识库(可选描述,写入 index.md frontmatter)。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "知识库名([a-z0-9_-])"},
                        "description": {"type": "string"}
                    },
                    "required": ["name"]
                }
            },
            {
                "name": "kms_save_page",
                "description": "创建 / 覆盖页面(Markdown 正文;可选 title / tags 落 frontmatter)。create_only=true 时同名存在即报错。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "wiki": {"type": "string"},
                        "page": {"type": "string"},
                        "content": {"type": "string"},
                        "title": {"type": "string"},
                        "tags": {"type": "array", "items": {"type": "string"}},
                        "create_only": {"type": "boolean"}
                    },
                    "required": ["wiki", "page", "content"]
                }
            },
            {
                "name": "kms_delete_wiki",
                "description": "删除整个知识库(目录级,不可恢复)。",
                "inputSchema": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                }
            },
            {
                "name": "kms_dream",
                "description": "把洞见(insights)固化成 dream-insights 知识库页面(从会话学到的经验沉淀)。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "insights": {"type": "array", "items": {"type": "string"}, "description": "洞见列表"},
                        "sessions_analyzed": {"type": "integer", "description": "分析的会话数(可选元信息)"}
                    },
                    "required": ["insights"]
                }
            }
        ]
    })
}

/// 阻塞式主循环:逐行读 JSON-RPC 请求、写响应;stdin 关闭即退出。
fn main() {
    let manager = KnowledgeManager::with_default_home();
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
        eprintln!("[kms-server] method={method}");
        let result = handle_request(method, &params, &manager);
        let resp = json!({"jsonrpc": "2.0", "id": id, "result": result});
        writeln!(stdout, "{resp}").ok();
        stdout.flush().ok();
    }
}

fn handle_request(method: &str, params: &Value, manager: &KnowledgeManager) -> Value {
    match method {
        "initialize" => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "serverInfo": {
                "name": "zhuye-kms",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {"tools": {"listChanged": false}},
        }),
        "tools/list" => tools_json(),
        "tools/call" => dispatch_tool(params, manager),
        "ping" => json!({}),
        other => json!({
            "content": [{"type": "text", "text": format!("method not implemented: {other}")}],
            "isError": true,
        }),
    }
}

fn dispatch_tool(params: &Value, manager: &KnowledgeManager) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let result = match name {
        "kms_list_wikis" => tool_list_wikis(manager),
        "kms_list_pages" => tool_list_pages(&args, manager),
        "kms_get_page" => tool_get_page(&args, manager),
        "kms_search" => tool_search(&args, manager),
        "kms_create_wiki" => tool_create_wiki(&args, manager),
        "kms_save_page" => tool_save_page(&args, manager),
        "kms_delete_wiki" => tool_delete_wiki(&args, manager),
        "kms_dream" => tool_dream(&args, manager),
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

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("缺少 {key} 参数"))
}

fn tool_list_wikis(manager: &KnowledgeManager) -> Result<String, String> {
    let wikis = manager.list_wikis().map_err(e2s)?;
    if wikis.is_empty() {
        return Ok("(无知识库;先用 kms_create_wiki 创建)".into());
    }
    let mut out = format!("{} 个知识库:\n", wikis.len());
    for w in wikis {
        out.push_str(&format!(
            "- {}({} 页,{}): {}\n",
            w.name,
            w.page_count,
            w.modified_at_ms,
            w.description.as_deref().unwrap_or("-")
        ));
    }
    Ok(out)
}

fn tool_list_pages(args: &Value, manager: &KnowledgeManager) -> Result<String, String> {
    let wiki = str_arg(args, "wiki")?;
    let pages = manager.list_pages(wiki).map_err(e2s)?;
    if pages.is_empty() {
        return Ok(format!("(知识库 {wiki} 无页面)"));
    }
    let mut out = format!("{} 的 {} 个页面:\n", wiki, pages.len());
    for p in pages {
        out.push_str(&format!(
            "- {}(tags: {})\n",
            p.name,
            if p.tags.is_empty() {
                "-".to_string()
            } else {
                p.tags.join(",")
            }
        ));
    }
    Ok(out)
}

fn tool_get_page(args: &Value, manager: &KnowledgeManager) -> Result<String, String> {
    let wiki = str_arg(args, "wiki")?;
    let page = str_arg(args, "page")?;
    let p = manager.get_page(wiki, page).map_err(e2s)?;
    let mut out = format!("# {}/{}\n\n{}", wiki, p.name, p.content);
    if !p.tags.is_empty() {
        out.push_str(&format!("\n\n(tags: {})", p.tags.join(", ")));
    }
    Ok(out)
}

fn tool_search(args: &Value, manager: &KnowledgeManager) -> Result<String, String> {
    let query = str_arg(args, "query")?;
    let hits = manager.search(query).map_err(e2s)?;
    if hits.is_empty() {
        return Ok(format!("(无命中:{query})"));
    }
    let mut out = format!("{} 处命中:\n", hits.len());
    for h in hits {
        out.push_str(&format!("== {}/{} ==\n", h.wiki, h.page));
        for line in h.lines {
            out.push_str(&format!("  {line}\n"));
        }
    }
    Ok(out)
}

fn tool_create_wiki(args: &Value, manager: &KnowledgeManager) -> Result<String, String> {
    let name = str_arg(args, "name")?;
    let description = args
        .get("description")
        .and_then(Value::as_str)
        .map(String::from);
    let info = manager.create_wiki(name, description).map_err(e2s)?;
    Ok(format!(
        "知识库 {} 已创建({} 路径)",
        info.name,
        info.path.display()
    ))
}

fn tool_save_page(args: &Value, manager: &KnowledgeManager) -> Result<String, String> {
    let wiki = str_arg(args, "wiki")?;
    let page = str_arg(args, "page")?;
    let content = str_arg(args, "content")?;
    let title = args.get("title").and_then(Value::as_str).map(String::from);
    let tags: Vec<String> = args
        .get("tags")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let create_only = args
        .get("create_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let saved = if create_only {
        manager.create_page(wiki, page, content, title, tags)
    } else {
        manager.save_page(wiki, page, content, title, tags)
    }
    .map_err(e2s)?;
    Ok(format!(
        "页面 {}/{} 已保存({} 字节,tags: {})",
        wiki,
        saved.name,
        saved.content.len(),
        if saved.tags.is_empty() {
            "-".to_string()
        } else {
            saved.tags.join(",")
        }
    ))
}

fn tool_delete_wiki(args: &Value, manager: &KnowledgeManager) -> Result<String, String> {
    let name = str_arg(args, "name")?;
    manager.delete_wiki(name).map_err(e2s)?;
    Ok(format!("知识库 {name} 已删除"))
}

fn tool_dream(args: &Value, manager: &KnowledgeManager) -> Result<String, String> {
    // dream 固定写入 `dream-insights` 知识库(实现内置),wiki 参数不收。
    let insights: Vec<String> = args
        .get("insights")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .ok_or("缺少 insights 参数(字符串数组)")?;
    let sessions = args
        .get("sessions_analyzed")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let result = manager.dream(insights, sessions).map_err(e2s)?;
    Ok(format!(
        "洞见已固化为 {}/{}(共 {} 条)",
        result.wiki,
        result.page,
        result.insights.len()
    ))
}

fn e2s(e: zhuye_kms::KmsError) -> String {
    e.to_string()
}
