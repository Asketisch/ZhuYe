//! `zhuye-browser-server` — 浏览器控制的 MCP stdio server
//! (computer-use 同范式:手写 JSON-RPC 2.0 over stdio;line-delimited;
//! stdout 只输出协议响应,日志一律 stderr)。
//!
//! 工具:`browser_navigate` / `browser_snapshot` / `browser_click` /
//! `browser_type` / `browser_press_key` / `browser_scroll` /
//! `browser_screenshot` / `browser_close`。构建时 `--features real` 启用
//! CDP 真实后端;feature 关闭时全部返回 Unavailable 文案(优雅降级)。
//!
//! 快照绑定协议:`browser_click` / `browser_type` 必须携带取到引用时的
//! `snapshotVersion` —— 页面变化后旧引用显式失效,防"点到另一个同名按钮"。

use std::io::{self, BufRead, Write};
use std::sync::Arc;

use base64::Engine as _;
use serde_json::{Value, json};
use zhuye_browser::BrowserBackend;

const PROTOCOL_VERSION: &str = "2025-06-18";

fn tools_json() -> Value {
    json!({
        "tools": [
            {
                "name": "browser_navigate",
                "description": "让受控浏览器导航到 URL(等页面 load 完成返回;之后必须重新 snapshot)。",
                "inputSchema": {
                    "type": "object",
                    "properties": {"url": {"type": "string"}},
                    "required": ["url"]
                }
            },
            {
                "name": "browser_snapshot",
                "description": "拍页面可及性快照,返回带 @eN 元素引用的树形文本;首行 [版本] 标记当前快照版本,后续 click/type 必须携带该版本。",
                "inputSchema": {"type": "object", "properties": {}}
            },
            {
                "name": "browser_click",
                "description": "点击快照中的元素(@eN 引用 + 取到引用时的快照版本)。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "reference": {"type": "string", "description": "@eN 元素引用"},
                        "snapshotVersion": {"type": "integer", "description": "取到该引用时的快照版本"}
                    },
                    "required": ["reference", "snapshotVersion"]
                }
            },
            {
                "name": "browser_type",
                "description": "向快照中的输入框填写文本(聚焦 → 全选 → 覆盖输入)。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "reference": {"type": "string"},
                        "text": {"type": "string"},
                        "snapshotVersion": {"type": "integer"}
                    },
                    "required": ["reference", "text", "snapshotVersion"]
                }
            },
            {
                "name": "browser_press_key",
                "description": "按组合键,如 \"enter\" / \"ctrl+a\" / \"tab\"。",
                "inputSchema": {
                    "type": "object",
                    "properties": {"keys": {"type": "string", "description": "+ 分隔的组合键"}},
                    "required": ["keys"]
                }
            },
            {
                "name": "browser_scroll",
                "description": "滚轮滚动(正 dy = 下,正 dx = 右)。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "dx": {"type": "integer", "description": "水平滚动量(默认 0)"},
                        "dy": {"type": "integer", "description": "垂直滚动量(默认 0)"}
                    }
                }
            },
            {
                "name": "browser_screenshot",
                "description": "截取当前页面,返回 JPEG 图片。",
                "inputSchema": {"type": "object", "properties": {}}
            },
            {
                "name": "browser_close",
                "description": "关闭当前受控标签页(headless 下浏览器随之退出)。",
                "inputSchema": {"type": "object", "properties": {}}
            }
        ]
    })
}

fn main() {
    // 工具体是 async(CDP over ws)—— 自建 runtime,顺序 block_on。
    // server 主循环保持与 computer-use 相同的同步 stdio 形态。
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("构建 tokio runtime");
    let backend: Arc<dyn BrowserBackend> = zhuye_browser::browser_backend();
    eprintln!(
        "[browser-server] capability={:?}",
        runtime.block_on(async { backend.capability() })
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
        let result = handle_request(method, &params, backend.as_ref(), &runtime);
        let resp = json!({"jsonrpc": "2.0", "id": id, "result": result});
        writeln!(stdout, "{resp}").ok();
        stdout.flush().ok();
    }
}

fn handle_request(
    method: &str,
    params: &Value,
    backend: &dyn BrowserBackend,
    runtime: &tokio::runtime::Runtime,
) -> Value {
    match method {
        "initialize" => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "serverInfo": {
                "name": "zhuye-browser",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {"tools": {"listChanged": false}},
        }),
        "tools/list" => tools_json(),
        "tools/call" => dispatch_tool(params, backend, runtime),
        "ping" => json!({}),
        other => json!({
            "content": [{"type": "text", "text": format!("method not implemented: {other}")}],
            "isError": true,
        }),
    }
}

fn dispatch_tool(
    params: &Value,
    backend: &dyn BrowserBackend,
    runtime: &tokio::runtime::Runtime,
) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !args.is_object() {
        return json!({"content": [{"type": "text", "text": "arguments 必须是对象"}], "isError": true});
    }
    // 统一 BrowserError → String(错误文案给 LLM 看,含能力名与原因)。
    let result: Result<String, String> = match name {
        "browser_navigate" => str_arg(&args, "url").and_then(|url| {
            runtime
                .block_on(backend.navigate(&url))
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "navigated to {url};请重新 browser_snapshot 获取元素引用"
            ))
        }),
        "browser_snapshot" => runtime
            .block_on(backend.snapshot())
            .map(|s| s.render())
            .map_err(|e| e.to_string()),
        "browser_click" => str_arg(&args, "reference").and_then(|reference| {
            u64_arg(&args, "snapshotVersion").and_then(|version| {
                runtime
                    .block_on(backend.click(&reference, version))
                    .map(|_| "clicked".to_string())
                    .map_err(|e| e.to_string())
            })
        }),
        "browser_type" => str_arg(&args, "reference").and_then(|reference| {
            str_arg(&args, "text").and_then(|text| {
                u64_arg(&args, "snapshotVersion").and_then(|version| {
                    runtime
                        .block_on(backend.fill(&reference, &text, version))
                        .map(|_| "typed".to_string())
                        .map_err(|e| e.to_string())
                })
            })
        }),
        "browser_press_key" => str_arg(&args, "keys").and_then(|keys| {
            runtime
                .block_on(backend.press_key(&keys))
                .map(|_| "pressed".to_string())
                .map_err(|e| e.to_string())
        }),
        "browser_scroll" => optional_i32(&args, "dx").and_then(|dx| {
            optional_i32(&args, "dy").and_then(|dy| {
                runtime
                    .block_on(backend.scroll(dx, dy))
                    .map(|_| "scrolled".to_string())
                    .map_err(|e| e.to_string())
            })
        }),
        "browser_screenshot" => match runtime.block_on(backend.screenshot()) {
            Ok(jpeg) => {
                return json!({
                    "content": [{"type": "image", "data": base64::engine::general_purpose::STANDARD.encode(jpeg), "mimeType": "image/jpeg"}],
                    "isError": false,
                });
            }
            Err(error) => Err(error.to_string()),
        },
        "browser_close" => runtime
            .block_on(backend.close())
            .map(|_| "closed".to_string())
            .map_err(|e| e.to_string()),
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

fn u64_arg(args: &Value, key: &str) -> Result<u64, String> {
    let value = args
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("{key} 必须是整数"))?;
    u64::try_from(value).map_err(|_| format!("{key} 不能为负"))
}

fn optional_i32(args: &Value, key: &str) -> Result<i32, String> {
    if args.get(key).is_none() {
        return Ok(0);
    }
    let value = args
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("{key} 必须是整数"))?;
    i32::try_from(value).map_err(|_| format!("{key} 超出支持范围"))
}

// BrowserError 用于 server 侧文案(编译期确认 Display 可用)。
#[allow(dead_code)]
fn _err_display(e: &zhuye_browser::BrowserError) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zhuye_browser::{BrowserCapability, BrowserStatus};
    use zhuye_computer_use::A11ySnapshot;

    /// 记录调用的假后端(不依赖真浏览器)。
    struct FakeBackend {
        calls: std::sync::Mutex<Vec<String>>,
    }
    impl FakeBackend {
        fn new() -> Self {
            FakeBackend {
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }
    }
    #[async_trait::async_trait]
    impl BrowserBackend for FakeBackend {
        fn capability(&self) -> BrowserCapability {
            BrowserCapability::Full
        }
        async fn status(&self) -> Result<BrowserStatus, zhuye_browser::BrowserError> {
            Ok(BrowserStatus {
                url: None,
                title: None,
                running: false,
            })
        }
        async fn navigate(&self, url: &str) -> Result<(), zhuye_browser::BrowserError> {
            self.calls.lock().unwrap().push(format!("navigate:{url}"));
            Ok(())
        }
        async fn snapshot(&self) -> Result<A11ySnapshot, zhuye_browser::BrowserError> {
            self.calls.lock().unwrap().push("snapshot".into());
            Ok(A11ySnapshot::new(
                7,
                "测试页",
                vec![zhuye_computer_use::A11yElement {
                    reference: "@e1".into(),
                    role: zhuye_computer_use::ElementRole::Button,
                    name: "提交".into(),
                    disabled: false,
                    visible: true,
                    value: None,
                    depth: 1,
                }],
            ))
        }
        async fn click(
            &self,
            reference: &str,
            version: u64,
        ) -> Result<(), zhuye_browser::BrowserError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("click:{reference}@{version}"));
            Ok(())
        }
        async fn fill(
            &self,
            reference: &str,
            text: &str,
            version: u64,
        ) -> Result<(), zhuye_browser::BrowserError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("fill:{reference}:{text}@{version}"));
            Ok(())
        }
        async fn press_key(&self, keys: &str) -> Result<(), zhuye_browser::BrowserError> {
            self.calls.lock().unwrap().push(format!("press:{keys}"));
            Ok(())
        }
        async fn scroll(&self, dx: i32, dy: i32) -> Result<(), zhuye_browser::BrowserError> {
            self.calls.lock().unwrap().push(format!("scroll:{dx}:{dy}"));
            Ok(())
        }
        async fn screenshot(&self) -> Result<Vec<u8>, zhuye_browser::BrowserError> {
            self.calls.lock().unwrap().push("screenshot".into());
            Ok(vec![0xFF, 0xD8]) // JPEG 魔数
        }
        async fn close(&self) -> Result<(), zhuye_browser::BrowserError> {
            self.calls.lock().unwrap().push("close".into());
            Ok(())
        }
    }

    #[test]
    fn tools_list_has_all_eight_tools() {
        let tools_doc = tools_json();
        let tools = tools_doc["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 8);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in [
            "browser_navigate",
            "browser_snapshot",
            "browser_click",
            "browser_type",
            "browser_press_key",
            "browser_scroll",
            "browser_screenshot",
            "browser_close",
        ] {
            assert!(names.contains(&expected), "缺 {expected}");
        }
    }

    #[test]
    fn click_requires_reference_and_snapshot_version() {
        let backend = FakeBackend::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        for args in [
            json!({}),
            json!({"reference": "@e1"}),
            json!({"reference": "@e1", "snapshotVersion": -1}),
            json!({"reference": "@e1", "snapshotVersion": "7"}),
        ] {
            let response = dispatch_tool(
                &json!({"name": "browser_click", "arguments": args}),
                &backend,
                &runtime,
            );
            assert_eq!(response["isError"], true, "{response}");
        }
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn valid_click_reaches_backend_with_ref_and_version() {
        let backend = FakeBackend::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let response = dispatch_tool(
            &json!({"name": "browser_click", "arguments": {"reference": "@e1", "snapshotVersion": 7}}),
            &backend,
            &runtime,
        );
        assert_eq!(response["isError"], false, "{response}");
        assert_eq!(backend.calls.lock().unwrap()[0], "click:@e1@7");
    }

    #[test]
    fn snapshot_renders_tree_text() {
        let backend = FakeBackend::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let response = dispatch_tool(
            &json!({"name": "browser_snapshot", "arguments": {}}),
            &backend,
            &runtime,
        );
        assert_eq!(response["isError"], false);
        let text = response["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("[7]"), "快照版本应在首行: {text}");
        assert!(text.contains("@e1"), "元素引用应渲染: {text}");
    }

    #[test]
    fn screenshot_returns_mcp_image_with_jpeg_bytes() {
        let backend = FakeBackend::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let response = dispatch_tool(
            &json!({"name": "browser_screenshot", "arguments": {}}),
            &backend,
            &runtime,
        );
        assert_eq!(response["isError"], false);
        assert_eq!(response["content"][0]["type"], "image");
        assert_eq!(response["content"][0]["mimeType"], "image/jpeg");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(response["content"][0]["data"].as_str().unwrap())
                .unwrap(),
            [0xFF, 0xD8]
        );
    }

    #[test]
    fn unknown_tool_is_error() {
        let backend = FakeBackend::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let response = dispatch_tool(
            &json!({"name": "browser_hack", "arguments": {}}),
            &backend,
            &runtime,
        );
        assert_eq!(response["isError"], true);
    }
}
