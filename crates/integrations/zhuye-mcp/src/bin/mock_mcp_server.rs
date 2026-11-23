//! Mock MCP stdio server —— 仅用于 zhuye-mcp 自身测试。
//!
//! 协议版本 `2025-06-18`,与 `zhuye_mcp::PROTOCOL_VERSION` 对齐。
//! 支持 2 个 tool:`echo`(立即返回)和 `slow`(sleep 2s 用于超时测试)。
//!
//! ## 输入输出格式
//!
//! JSON-RPC 2.0 over stdio(line-delimited JSON,每行一条 message)。
//! 不实现 notifications 的异步 push(rmcp 1.7 client 不强制要求)。
//!
//! ## 启动方式
//!
//! ```toml
//! [mcp_servers.mock]
//! type = "stdio"
//! command = "/path/to/zhuye-mcp-mock_mcp_server"
//! ```
//!
//! ## env
//!
//! `MOCK_GREETING` —— 设置后 `echo` 工具返回 `"{greeting}: {text}"`,
//! 用于测试 env 变量变更后 reload 是否生效。
//!
//! `MOCK_LIST_CHANGED` —— 设置后 `tools/list` 先按旧清单(echo + slow)应答,
//! 紧接着发一条 `notifications/tools/list_changed`,之后的清单换成
//! (echo + extra) —— 用于验证工具列表动态刷新。
//!
//! `MOCK_ELICITATION` —— `elicitation/create` 的 params(JSON 串)。设置后
//! `echo` 工具在应答前先向客户端发起一次 server→client 请求,并把回执原样
//! 拼进返回文本(`"<text>|elicit:<回执 JSON>"`),用于 elicitation 端到端测试。
//!
//! `MOCK_RESOURCES` —— 设置后 `initialize` 声明 resources 能力,并实现
//! `resources/list` / `resources/read`(混合文本与 base64 二进制);`extra`
//! 工具返回嵌入资源与资源链接,供资源内容传递测试。
//!
//! `MOCK_DISCONNECT_ON_CALL` —— 设置后首次 `tools/call` 直接退出进程,
//! 用于验证调用中断开时宿主的错误回执。

use std::collections::HashMap;
use std::env;
use std::io::{self, BufRead, Write};
use std::thread::sleep;
use std::time::Duration;

use serde_json::{Value, json};

const PROTOCOL_VERSION: &str = "2025-06-18";

fn main() {
    if let Some(path) = env::var_os("MOCK_PID_MARKER") {
        write_marker(&path, &std::process::id().to_string());
    }
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut stdout = io::stdout();
    let mut handles = HashMap::new();
    // 用 read_line 而非 `lines()`:server→client 请求需要在处理一条客户端
    // 请求的中途再读一次 stdin(`MOCK_ELICITATION` 场景),`lines()` 的迭代器
    // 会把 borrow 钉在循环上,拿不到第二次读。
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let err = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")},
                });
                writeln!(stdout, "{err}").ok();
                stdout.flush().ok();
                continue;
            }
        };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        // 通知类型 (无 id 且无 result 期望): 直接吞掉,不响应。
        if msg.get("id").is_none() {
            // 只有在完全没有 "id" 字段时才视作通知。
            if method == "notifications/cancelled" {
                if let Some(path) = env::var_os("MOCK_CANCEL_MARKER") {
                    write_marker(&path, &msg.to_string());
                }
            }
            if method == "notifications/initialized"
                && env::var_os("MOCK_DISCONNECT_AFTER_INITIALIZED").is_some()
            {
                break;
            }
            continue;
        }
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        if env::var("MOCK_PENDING_METHOD").is_ok_and(|pending| pending == method) {
            continue;
        }
        if method == "resources/read" {
            if let Some(path) = env::var_os("MOCK_RESOURCE_PENDING_MARKER") {
                write_marker(&path, &id.to_string());
                continue;
            }
        }
        if method == "tools/call" && env::var_os("MOCK_DISCONNECT_ON_CALL").is_some() {
            break;
        }
        // 工具列表变化:先按旧清单应答,再发 `notifications/tools/list_changed`,
        // 之后的新清单少一个工具、多一个工具(供动态刷新测试用)。
        if method == "tools/list" && env::var_os("MOCK_LIST_CHANGED").is_some() {
            let changed = handles.contains_key("list_changed_sent");
            let tools = if changed {
                json!([
                    {"name": "echo", "description": "返回传入的 text 字段", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
                    {"name": "extra", "description": "刷新后新增的工具", "inputSchema": {"type": "object", "properties": {}}},
                ])
            } else {
                json!([
                    {"name": "echo", "description": "返回传入的 text 字段", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
                    {"name": "slow", "description": "sleep 2s 后返回 'slept'", "inputSchema": {"type": "object", "properties": {}}},
                ])
            };
            let resp = json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools}});
            writeln!(stdout, "{resp}").ok();
            stdout.flush().ok();
            if !changed {
                handles.insert("list_changed_sent".to_string(), json!(true));
                let note = json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"});
                writeln!(stdout, "{note}").ok();
                stdout.flush().ok();
            }
            continue;
        }
        let elicit = method == "tools/call"
            && params.get("name").and_then(Value::as_str) == Some("echo")
            && env::var("MOCK_ELICITATION").is_ok();
        let response = if elicit {
            // 空对象 / 缺字段时回退到默认 form:测试常写 `MOCK_ELICITATION="{}"`
            // 只表示「开启 elicitation」,并不想自定义 schema。
            let spec = env::var("MOCK_ELICITATION")
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .filter(|value| {
                    value.get("mode").is_some()
                        || value.get("requestedSchema").is_some()
                        || value.get("url").is_some()
                })
                .unwrap_or_else(default_elicitation_params);
            let text = params
                .get("arguments")
                .and_then(|a| a.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            match request_client(
                &mut reader,
                &mut stdout,
                "srv-1",
                "elicitation/create",
                &spec,
            ) {
                Some(answer) => json!({
                    "content": [{"type": "text", "text": format!("{text}|elicit:{answer}")}],
                    "isError": false,
                }),
                None => json!({
                    "content": [{"type": "text", "text": "elicitation 未获回执"}],
                    "isError": true,
                }),
            }
        } else {
            handle_request(method, &params, &mut handles)
        };
        let resp = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": response,
        });
        writeln!(stdout, "{resp}").ok();
        stdout.flush().ok();
    }
}

/// 默认 elicitation 参数:一个必填字符串字段(MCP form 形态)。
fn default_elicitation_params() -> Value {
    json!({
        "mode": "form",
        "message": "请补充信息",
        "requestedSchema": {
            "type": "object",
            "properties": {
                "answer": {
                    "type": "string",
                    "title": "回答",
                    "description": "测试用输入",
                },
            },
            "required": ["answer"],
        },
    })
}

/// 发出一次 server→client 请求并阻塞等回执。
///
/// 测试专用:期间到达的其它消息只处理取消通知(写 marker),其余忽略 ——
/// 真实服务端会处理,这里不需要。
fn request_client(
    reader: &mut impl BufRead,
    stdout: &mut impl Write,
    id: &str,
    method: &str,
    params: &Value,
) -> Option<Value> {
    let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    writeln!(stdout, "{request}").ok()?;
    stdout.flush().ok()?;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return None,
            Ok(_) => {}
        }
        let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if msg.get("id").and_then(Value::as_str) == Some(id) {
            return msg.get("result").cloned();
        }
        if msg.get("method").and_then(Value::as_str) == Some("notifications/cancelled") {
            if let Some(path) = env::var_os("MOCK_CANCEL_MARKER") {
                write_marker(&path, &msg.to_string());
            }
        }
    }
}

fn write_marker(path: &std::ffi::OsStr, contents: &str) {
    let path = std::path::Path::new(path);
    let temporary = path.with_extension("tmp");
    if std::fs::write(&temporary, contents).is_ok() {
        std::fs::rename(temporary, path).ok();
    }
}

fn handle_request(method: &str, params: &Value, _handles: &mut HashMap<String, Value>) -> Value {
    match method {
        "initialize" => {
            let mut capabilities = json!({
                "tools": {"listChanged": false},
            });
            if env::var_os("MOCK_RESOURCES").is_some() {
                capabilities["resources"] = json!({});
            }
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "serverInfo": {
                    "name": "zhuye-mock",
                    "version": "0.3.0",
                },
                "capabilities": capabilities,
            })
        }
        "tools/list" => json!({
            "tools": [
                {
                    "name": "echo",
                    "description": "返回传入的 text 字段,可在 env MOCK_GREETING 启用时加前缀",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "text": {"type": "string"},
                        },
                        "required": ["text"],
                    },
                },
                {
                    "name": "slow",
                    "description": "sleep 2s 后返回 'slept',用于 timeout 测试",
                    "inputSchema": {
                        "type": "object",
                        "properties": {},
                    },
                },
            ],
        }),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            match name {
                "echo" => {
                    let text = args
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let greeting = env::var("MOCK_GREETING").unwrap_or_default();
                    let out = if greeting.is_empty() {
                        text
                    } else {
                        format!("{greeting}: {text}")
                    };
                    json!({
                        "content": [{"type": "text", "text": out}],
                        "isError": false,
                    })
                }
                "slow" => {
                    sleep(Duration::from_secs(2));
                    json!({
                        "content": [{"type": "text", "text": "slept"}],
                        "isError": false,
                    })
                }
                // 刷新后新增的工具;开启 MOCK_RESOURCES 时附带嵌入资源。
                "extra" => {
                    if env::var_os("MOCK_RESOURCES").is_some() {
                        json!({
                            "content": [
                                {
                                    "type": "resource",
                                    "resource": {
                                        "uri": "file:///embedded",
                                        "mimeType": "text/plain",
                                        "text": "嵌入正文",
                                    }
                                },
                                {
                                    "type": "resource_link",
                                    "uri": "file:///linked",
                                    "name": "关联资源",
                                    "description": "资源说明",
                                },
                            ],
                            "isError": false,
                        })
                    } else {
                        json!({
                            "content": [{"type": "text", "text": "extra called"}],
                            "isError": false,
                        })
                    }
                }
                other => json!({
                    "content": [{"type": "text", "text": format!("unknown tool: {other}")}],
                    "isError": true,
                }),
            }
        }
        "ping" => json!({}),
        "resources/list" if env::var_os("MOCK_RESOURCES").is_some() => json!({
            "resources": [{
                "name": "mixed",
                "uri": "file:///mixed",
                "mimeType": "text/plain",
            }],
        }),
        "resources/read" if env::var_os("MOCK_RESOURCES").is_some() => json!({
            "contents": [
                {
                    "uri": "file:///text",
                    "mimeType": "text/plain",
                    "text": "资源正文",
                },
                {
                    "uri": "file:///blob",
                    "mimeType": "application/octet-stream",
                    "blob": "AAEC/w==",
                },
            ],
        }),
        // 其他方法 (prompts/list, ...) 留空。
        _ => json!({
            "content": [{"type": "text", "text": format!("method not implemented: {method}")}],
            "isError": true,
        }),
    }
}
