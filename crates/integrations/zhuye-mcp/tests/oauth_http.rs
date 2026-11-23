//! MCP 客户端侧 OAuth 端到端:本地 HTTP 服务要求 Bearer token,
//! 客户端按「受保护资源元数据 → 授权服务器元数据 → 客户端凭据换 token」
//! 完成认证并握手成功;无凭据时必须明确报「需要授权」而不是含糊失败。
//!
//! 本地服务用裸 TCP 手写 HTTP(不引第三方 web 框架):只实现本用例需要的
//! 四个端点。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use zhuye_mcp::{McpConnectionManager, McpOAuthConfig, McpServerConfig, McpTransport};

const TOKEN: &str = "t0k";

/// 记录到的一次请求:(路径, 请求体, Authorization 头)。
type Recorded = (String, String, Option<String>);

/// 手写 HTTP 服务:返回请求路径、请求体、Authorization 头。
struct MiniServer {
    port: u16,
    requests: Arc<std::sync::Mutex<Vec<Recorded>>>,
}

impl MiniServer {
    async fn start(require_token: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定端口");
        let port = listener.local_addr().expect("本地地址").port();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 4096];
                    // 读到请求头结束,再按 Content-Length 读 body。
                    let head_end = loop {
                        if let Some(position) = find_double_crlf(&buffer) {
                            break position;
                        }
                        let read = stream.read(&mut chunk).await.unwrap_or(0);
                        if read == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                    };
                    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
                    let mut lines = head.lines();
                    let request_line = lines.next().unwrap_or_default().to_string();
                    let path = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string();
                    let mut headers: HashMap<String, String> = HashMap::new();
                    for line in lines {
                        if let Some((key, value)) = line.split_once(':') {
                            headers
                                .insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
                        }
                    }
                    let length: usize = headers
                        .get("content-length")
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(0);
                    let mut body = buffer[head_end + 4..].to_vec();
                    while body.len() < length {
                        let read = stream.read(&mut chunk).await.unwrap_or(0);
                        if read == 0 {
                            break;
                        }
                        body.extend_from_slice(&chunk[..read]);
                    }
                    let body = String::from_utf8_lossy(&body).to_string();
                    let auth = headers.get("authorization").cloned();
                    recorded.lock().expect("请求记录锁").push((
                        path.clone(),
                        body.clone(),
                        auth.clone(),
                    ));
                    let response = respond(&path, &body, auth.as_deref(), require_token, port);
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        Self { port, requests }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn recorded(&self) -> Vec<Recorded> {
        self.requests.lock().expect("请求记录锁").clone()
    }
}

fn find_double_crlf(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// 按路径生成响应。
fn respond(path: &str, body: &str, auth: Option<&str>, require_token: bool, port: u16) -> String {
    let authorized = auth == Some(&format!("Bearer {TOKEN}"));
    if path.starts_with("/.well-known/oauth-protected-resource") {
        return json_response(
            200,
            &format!(
                r#"{{"resource":"http://127.0.0.1:{port}/mcp","authorization_servers":["http://127.0.0.1:{port}"],"scopes_supported":["mcp:read"]}}"#
            ),
        );
    }
    if path == "/.well-known/oauth-authorization-server" {
        return json_response(
            200,
            &format!(
                r#"{{"issuer":"http://127.0.0.1:{port}","authorization_endpoint":"http://127.0.0.1:{port}/authorize","token_endpoint":"http://127.0.0.1:{port}/token","scopes_supported":["mcp:read"]}}"#
            ),
        );
    }
    if path == "/token" {
        return json_response(
            200,
            r#"{"access_token":"t0k","token_type":"Bearer","expires_in":3600}"#,
        );
    }
    if path == "/mcp" {
        if require_token && !authorized {
            // 未授权:按 MCP 规范回 401 + WWW-Authenticate(指向元数据)。
            return format!(
                "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer resource_metadata=\"http://127.0.0.1:{port}/.well-known/oauth-protected-resource/mcp\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
        }
        let request: serde_json::Value =
            serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
        let id = request
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        if id.is_null() {
            // 通知(initialized 等):202。
            return "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string();
        }
        let method = request
            .get("method")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let result = match method {
            "initialize" => serde_json::json!({
                "protocolVersion": "2025-06-18",
                "serverInfo": {"name": "oauth-mock", "version": "1"},
                "capabilities": {"tools": {}},
            }),
            "tools/list" => serde_json::json!({
                "tools": [{"name": "ping", "inputSchema": {"type": "object", "properties": {}}}],
            }),
            "tools/call" => serde_json::json!({
                "content": [{"type": "text", "text": "pong"}],
                "isError": false,
            }),
            _ => serde_json::json!({}),
        };
        return json_response(
            200,
            &serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
        );
    }
    json_response(404, "{}")
}

fn json_response(status: u16, body: &str) -> String {
    let reason = if status == 200 { "OK" } else { "Error" };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn http_config(url: String, oauth: Option<McpOAuthConfig>) -> McpServerConfig {
    McpServerConfig {
        name: "oauth-mock".to_string(),
        transport: McpTransport::Http,
        command: None,
        args: vec![],
        env: HashMap::new(),
        url: Some(url),
        headers: HashMap::new(),
        timeout: Duration::from_secs(10),
        oauth,
    }
}

/// 客户端凭据换 token → 带 token 握手 → 列出工具。
#[tokio::test]
async fn client_credentials_token_is_discovered_and_used() {
    let server = MiniServer::start(true).await;
    let url = server.url("/mcp");
    let oauth = McpOAuthConfig {
        client_id: Some("cid".to_string()),
        client_secret: Some("secret".to_string()),
        scope: Some("mcp:read".to_string()),
        ..McpOAuthConfig::default()
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let manager = McpConnectionManager::new(tx);
    let handle = manager
        .start_server(http_config(url, Some(oauth)))
        .await
        .expect("凭据齐备时应握手成功");
    assert_eq!(handle.tools.len(), 1, "应列出 1 个工具");
    assert_eq!(handle.tools[0].original_name, "ping");

    // 端到端证据:换过 token、且 MCP 请求带上了 Bearer。
    let recorded = server.recorded();
    assert!(
        recorded
            .iter()
            .any(|(path, _, _)| path.starts_with("/.well-known/oauth-protected-resource")),
        "应发现受保护资源元数据:{recorded:?}"
    );
    assert!(
        recorded.iter().any(|(path, _, _)| path == "/token"),
        "应调用 token 端点:{recorded:?}"
    );
    let mcp_auth = recorded
        .iter()
        .find(|(path, _, _)| path == "/mcp")
        .and_then(|(_, _, auth)| auth.clone());
    assert_eq!(
        mcp_auth.as_deref(),
        Some("Bearer t0k"),
        "MCP 请求应带上换到的 token"
    );
    manager.shutdown().await;
    let _ = rx.try_recv();
}

/// 没有凭据时:必须明确报「需要授权」,而不是含糊的握手失败。
#[tokio::test]
async fn missing_credentials_report_auth_required() {
    let server = MiniServer::start(true).await;
    let url = server.url("/mcp");
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let manager = McpConnectionManager::new(tx);
    let outcome = manager.start_server(http_config(url, None)).await;
    let error = match outcome {
        Ok(_) => panic!("无凭据时不应握手成功"),
        Err(error) => error,
    };
    let message = format!("{error}");
    assert!(
        message.contains("auth required") || message.contains("401"),
        "错误应明确指出需要授权:{message}"
    );
    manager.shutdown().await;
    let _ = rx.try_recv();
}
