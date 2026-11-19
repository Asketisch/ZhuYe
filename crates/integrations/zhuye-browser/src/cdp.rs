//! 极简 CDP(Chrome DevTools Protocol)客户端:WebSocket + JSON-RPC 2.0,
//! id 匹配读回响应、忽略事件流。单连接顺序调用(server 的工具分发是
//! 串行的),无需 pending map。
//!
//! 目标模型:连接后 `Target.getTargets` 取 page targets(**单 target**
//! 约束在这里强制),`Target.attachToTarget(flatten=true)` 拿 sessionId,
//! 之后所有命令带 `sessionId` 字段发往该 target。

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::error::BrowserError;

/// tungstenite 传输错误 → BrowserError(`?` 直通)。
impl From<tokio_tungstenite::tungstenite::Error> for BrowserError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        BrowserError::other(format!("CDP 传输错误: {e}"))
    }
}

/// CDP 命令默认超时(导航/截图可能慢;整轮工具调用另有 MCP 层超时)。
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

/// 一条 CDP WebSocket 连接。
pub struct CdpConnection {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next_id: u64,
}

impl CdpConnection {
    /// 连接 CDP WebSocket(`ws://127.0.0.1:<port><path>`)。
    pub async fn connect(ws_url: &str) -> Result<CdpConnection, BrowserError> {
        let (ws, _resp) = connect_async(ws_url)
            .await
            .map_err(|e| BrowserError::other(format!("CDP 连接失败: {e}")))?;
        Ok(CdpConnection { ws, next_id: 1 })
    }

    /// 发一条浏览器级命令(无 target session)并等响应 `result`。
    pub async fn command(&mut self, method: &str, params: Value) -> Result<Value, BrowserError> {
        self.roundtrip(None, method, params).await
    }

    /// 发一条 flatten session 命令并等响应 `result`。
    pub async fn command_on_target(
        &mut self,
        session_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, BrowserError> {
        self.roundtrip(Some(session_id), method, params).await
    }

    async fn roundtrip(
        &mut self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, BrowserError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut req = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        if let Some(session) = session_id {
            req["sessionId"] = json!(session);
        }
        let frame = req.to_string();
        self.ws
            .send(Message::Text(frame.into()))
            .await
            .map_err(|e| BrowserError::other(format!("CDP 发送失败: {e}")))?;

        // 读到匹配 id 的响应为止;事件（Page.frameNavigated 等）忽略。
        let deadline = tokio::time::Instant::now() + COMMAND_TIMEOUT;
        loop {
            let msg = tokio::select! {
                m = self.ws.next() => m,
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(BrowserError::other(format!(
                        "CDP 响应超时({method},{}s)",
                        COMMAND_TIMEOUT.as_secs()
                    )));
                }
            };
            let Some(msg) = msg else {
                return Err(BrowserError::other("CDP 连接已关闭"));
            };
            let Message::Text(text) = msg? else {
                // Ping/Pong/Binary:交给 tungstenite 自动处理,继续读。
                continue;
            };
            let value: Value = serde_json::from_str(&text)
                .map_err(|e| BrowserError::other(format!("CDP 帧解析失败: {e}")))?;
            // 响应带 id;事件带 method 无 id。
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = value.get("error") {
                    return Err(BrowserError::other(format!("CDP {method} 失败: {err}")));
                }
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
            tracing::trace!(target: "zhuye_browser", event = %value, "CDP event ignored");
        }
    }
}

/// 从 `ws://host:port/path` 提取 host(BrowserSecurity 校验用)。
///
/// IPv6 形如 `ws://[::1]:9222/x` → 去括号取 `::1`。
pub fn extract_host(ws_url: &str) -> Result<String, BrowserError> {
    let rest = ws_url
        .strip_prefix("ws://")
        .or_else(|| ws_url.strip_prefix("wss://"))
        .ok_or_else(|| BrowserError::other(format!("非 ws(s) URL: {ws_url}")))?;
    let authority = rest.split(['/', '?']).next().unwrap_or("");
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped
            .split(']')
            .next()
            .ok_or_else(|| BrowserError::other(format!("IPv6 括号未闭合: {ws_url}")))?
            .to_string()
    } else {
        // host:port → 冒号前半;无端口 → 整段。
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host)
            .to_string()
    };
    if host.is_empty() {
        return Err(BrowserError::other(format!("URL 缺 host: {ws_url}")));
    }
    Ok(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_host_parses_loopback_urls() {
        assert_eq!(
            extract_host("ws://127.0.0.1:9222/devtools/browser/abc").unwrap(),
            "127.0.0.1"
        );
        assert_eq!(extract_host("ws://localhost:9222").unwrap(), "localhost");
        assert_eq!(extract_host("ws://[::1]:9222/x").unwrap(), "::1");
    }

    #[test]
    fn extract_host_rejects_non_ws_schemes() {
        assert!(extract_host("http://127.0.0.1:9222").is_err());
        assert!(extract_host("ws:///no-host").is_err());
    }
}
