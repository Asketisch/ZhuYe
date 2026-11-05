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

use base64::Engine as _;
use serde_json::{Value, json};
use zhuye_computer_use::{ComputerUseAction, MediaError};

const PROTOCOL_VERSION: &str = "2025-06-18";

fn tools_json() -> Value {
    json!({
        "tools": [
            {
                "name": "computer_screenshot",
                "description": "截取主显示器屏幕,返回 PNG 图片。需要屏幕录制权限。",
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
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !args.is_object() {
        return json!({"content": [{"type": "text", "text": "arguments 必须是对象"}], "isError": true});
    }
    // 统一 MediaError → String(错误文案给 LLM 看,含能力名与原因)。
    let result: Result<String, String> = match name {
        "computer_screenshot" => match backend.screenshot() {
            Ok(png) => {
                return json!({
                    "content": [{"type": "image", "data": base64::engine::general_purpose::STANDARD.encode(png), "mimeType": "image/png"}],
                    "isError": false,
                });
            }
            Err(error) => Err(error.to_string()),
        },
        "computer_click" => {
            parse_click(&args).and_then(|action| execute_action(backend, action, "clicked"))
        }
        "computer_type" => str_arg(&args, "text").and_then(|t| {
            backend
                .execute(&ComputerUseAction::KeyType { text: t })
                .map_err(|e| e.to_string())
                .map(|_| "typed".to_string())
        }),
        "computer_key" => str_arg(&args, "keys").and_then(|keys| {
            execute_action(backend, ComputerUseAction::KeyCombo { keys }, "pressed")
        }),
        "computer_scroll" => optional_i32(&args, "dx")
            .and_then(|dx| optional_i32(&args, "dy").map(|dy| ComputerUseAction::Scroll { dx, dy }))
            .and_then(|action| execute_action(backend, action, "scrolled")),
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

fn execute_action(
    backend: &dyn zhuye_computer_use::backend::ComputerBackend,
    action: ComputerUseAction,
    success: &str,
) -> Result<String, String> {
    action.validate().map_err(|error| error.to_string())?;
    backend
        .execute(&action)
        .map_err(|error| error.to_string())?;
    Ok(success.to_string())
}

fn required_i32(args: &Value, key: &str) -> Result<i32, String> {
    let value = args
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("{key} 必须是整数"))?;
    i32::try_from(value).map_err(|_| format!("{key} 超出支持范围"))
}

fn optional_i32(args: &Value, key: &str) -> Result<i32, String> {
    if args.get(key).is_none() {
        Ok(0)
    } else {
        required_i32(args, key)
    }
}

fn parse_click(args: &Value) -> Result<ComputerUseAction, String> {
    let x = required_i32(args, "x")?;
    let y = required_i32(args, "y")?;
    let button = match args.get("button") {
        None => "left".to_string(),
        Some(value) => value.as_str().ok_or("button 必须是字符串")?.to_string(),
    };
    Ok(ComputerUseAction::MouseClick { x, y, button })
}

// MediaError 用于 server 侧文案(编译期确认 Display 可用)。
#[allow(dead_code)]
fn _err_display(e: &MediaError) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use zhuye_computer_use::{BackendCapability, backend::ComputerBackend};

    #[derive(Default)]
    struct RecordingBackend(Mutex<Vec<ComputerUseAction>>);
    impl ComputerBackend for RecordingBackend {
        fn capability(&self) -> BackendCapability {
            BackendCapability::Full
        }
        fn screenshot(&self) -> Result<Vec<u8>, MediaError> {
            Ok(vec![0x89, b'P', b'N', b'G'])
        }
        fn execute(&self, action: &ComputerUseAction) -> Result<(), MediaError> {
            self.0.lock().unwrap().push(action.clone());
            Ok(())
        }
    }

    #[test]
    fn screenshot_returns_mcp_image_with_original_png_bytes() {
        let backend = RecordingBackend::default();
        let response = dispatch_tool(
            &json!({"name": "computer_screenshot", "arguments": {}}),
            &backend,
        );
        assert_eq!(response["isError"], false);
        assert_eq!(response["content"][0]["type"], "image");
        assert_eq!(response["content"][0]["mimeType"], "image/png");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(response["content"][0]["data"].as_str().unwrap())
                .unwrap(),
            [0x89, b'P', b'N', b'G']
        );
    }

    #[test]
    fn invalid_arguments_never_reach_backend() {
        let backend = RecordingBackend::default();
        for (name, args) in [
            ("computer_click", json!({"x": 4294967296i64, "y": 0})),
            ("computer_click", json!({"x": 1, "y": 2, "button": "rgiht"})),
            ("computer_click", json!({"x": 1, "y": 2, "button": null})),
            ("computer_scroll", json!({"dy": 2147483648i64})),
            ("computer_scroll", json!({"dx": "5"})),
            ("computer_key", json!({"keys": "ctrl+escapee"})),
            ("computer_key", json!({"keys": "ctrl++a"})),
            ("computer_click", json!(null)),
        ] {
            let response = dispatch_tool(&json!({"name": name, "arguments": args}), &backend);
            assert_eq!(response["isError"], true, "{name}: {response}");
        }
        assert!(backend.0.lock().unwrap().is_empty());
    }

    #[test]
    fn valid_arguments_reach_backend_without_conversion_loss() {
        let backend = RecordingBackend::default();
        for (name, args) in [
            (
                "computer_click",
                json!({"x": -42, "y": 99, "button": "right"}),
            ),
            ("computer_scroll", json!({"dx": i32::MIN, "dy": i32::MAX})),
            ("computer_key", json!({"keys": "ctrl+shift+p"})),
        ] {
            let response = dispatch_tool(&json!({"name": name, "arguments": args}), &backend);
            assert_eq!(response["isError"], false, "{response}");
        }
        let actions = backend.0.lock().unwrap();
        assert!(
            matches!(&actions[0], ComputerUseAction::MouseClick { x: -42, y: 99, button } if button == "right")
        );
        assert!(
            matches!(&actions[1], ComputerUseAction::Scroll { dx, dy } if *dx == i32::MIN && *dy == i32::MAX)
        );
        assert!(
            matches!(&actions[2], ComputerUseAction::KeyCombo { keys } if keys == "ctrl+shift+p")
        );
    }
}
