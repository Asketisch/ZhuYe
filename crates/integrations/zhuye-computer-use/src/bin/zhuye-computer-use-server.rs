//! `zhuye-computer-use-server` — Computer Use 的 MCP stdio server
//! (harness-import 同范式:手写 JSON-RPC 2.0 over stdio;line-delimited;
//! stdout 只输出协议响应,日志一律 stderr)。
//!
//! 工具:`computer_screenshot` / `computer_click` / `computer_type` /
//! `computer_key` / `computer_scroll`。构建时 `--features real` 启用
//! xcap/enigo 真实后端;feature 关闭时全部返回 Unavailable 文案(优雅
//! 降级,不 panic)。

use std::io::{self, BufRead, Write};
use std::sync::Arc;

use serde_json::{Value, json};
use zhuye_computer_use::{ComputerUseAction, MediaError};

const PROTOCOL_VERSION: &str = "2025-06-18";

fn tools_json() -> Value {
    json!({
        "tools": [
            {
                "name": "computer_screenshot",
                "description": "截取主显示器屏幕,返回 PNG(含 data URL 提示)。需要屏幕录制权限。",
                "inputSchema": {"type": "object", "properties": {}}
            },
            {
                "name": "computer_click",
                "description": "在屏幕绝对坐标 (x, y) 点击(默认左键;button 可选 left/right/middle)。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "x": {"type": "integer"},
                        "y": {"type": "integer"},
                        "button": {"type": "string", "enum": ["left", "right", "middle"]}
                    },
                    "required": ["x", "y"]
                }
            },
            {
                "name": "computer_type",
                "description": "键入文本(经输入法语义;不要用它发组合键,用 computer_key)。",
                "inputSchema": {
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"]
                }
            },
            {
                "name": "computer_key",
                "description": "按组合键,如 \"ctrl+c\" / \"cmd+shift+p\" / \"enter\"。",
                "inputSchema": {
                    "type": "object",
                    "properties": {"keys": {"type": "string", "description": "+ 分隔的组合键"}},
                    "required": ["keys"]
                }
            },
            {
                "name": "computer_scroll",
                "description": "滚轮滚动(正 dy = 上,正 dx = 右)。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "dx": {"type": "integer", "description": "水平滚动量(默认 0)"},
                        "dy": {"type": "integer", "description": "垂直滚动量(默认 0)"}
                    }
                }
            }
        ]
    })
}

fn main() {
    let backend: Arc<dyn zhuye_computer_use::backend::ComputerBackend> =
        zhuye_computer_use::computer_backend();
    eprintln!(
        "[computer-use-server] capability={:?}",
        backend.capability()
    );
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
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result = handle_request(method, &params, backend.as_ref());
        let resp = json!({"jsonrpc": "2.0", "id": id, "result": result});
        writeln!(stdout, "{resp}").ok();
        stdout.flush().ok();
    }
}

fn handle_request(
    method: &str,
    params: &Value,
    backend: &dyn zhuye_computer_use::backend::ComputerBackend,
) -> Value {
    match method {
        "initialize" => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "serverInfo": {
                "name": "zhuye-computer-use",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {"tools": {"listChanged": false}},
        }),
        "tools/list" => tools_json(),
        "tools/call" => dispatch_tool(params, backend),
        "ping" => json!({}),
        other => json!({
            "content": [{"type": "text", "text": format!("method not implemented: {other}")}],
            "isError": true,
        }),
    }
}

fn dispatch_tool(
    params: &Value,
    backend: &dyn zhuye_computer_use::backend::ComputerBackend,
) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    // 统一 MediaError → String(错误文案给 LLM 看,含能力名与原因)。
    let result: Result<String, String> = match name {
        "computer_screenshot" => backend.screenshot().map(|png| {
            // 不内嵌 bytes——字节量对 LLM 上下文不友好;给元信息,宿主可
            // 落盘后经 image_view 查看。
            format!(
                "screenshot captured: {} bytes (PNG);可让宿主保存为文件后经 image_view 查看",
                png.len()
            )
        }).map_err(|e| e.to_string()),
        "computer_click" => parse_click(&args)
            .and_then(|a| backend.execute(&a).map_err(|e| e.to_string()).map(|_| "clicked".to_string())),
        "computer_type" => {
            str_arg(&args, "text").and_then(|t| {
                backend
                    .execute(&ComputerUseAction::KeyType { text: t })
                    .map_err(|e| e.to_string())
                    .map(|_| "typed".to_string())
            })
        }
        "computer_key" => {
            str_arg(&args, "keys").and_then(|k| {
                backend
                    .execute(&ComputerUseAction::KeyCombo { keys: k })
                    .map_err(|e| e.to_string())
                    .map(|_| "pressed".to_string())
            })
        }
        "computer_scroll" => {
            let dx = args.get("dx").and_then(Value::as_i64).unwrap_or(0) as i32;
            let dy = args.get("dy").and_then(Value::as_i64).unwrap_or(0) as i32;
            backend
                .execute(&ComputerUseAction::Scroll { dx, dy })
                .map_err(|e| e.to_string())
                .map(|_| "scrolled".to_string())
        }
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

fn str_arg(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| format!("缺少 {key} 参数"))
}

fn parse_click(args: &Value) -> Result<ComputerUseAction, String> {
    let x = args.get("x").and_then(Value::as_i64).ok_or("缺少 x 参数")? as i32;
    let y = args.get("y").and_then(Value::as_i64).ok_or("缺少 y 参数")? as i32;
    let button = args
        .get("button")
        .and_then(Value::as_str)
        .unwrap_or("left")
        .to_string();
    Ok(ComputerUseAction::MouseClick { x, y, button })
}

// MediaError 用于 server 侧文案(编译期确认 Display 可用)。
#[allow(dead_code)]
fn _err_display(e: &MediaError) -> String {
    e.to_string()
}
