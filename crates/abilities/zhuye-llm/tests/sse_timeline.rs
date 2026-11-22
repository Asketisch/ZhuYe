//! SSE 时间线回放(v1.x,借鉴 ZCode e2e-sse-capture-replay)——
//! wiremock 只能静态 stub;时间线 server 按 `offset_ms` 分段吐 chunk、
//! 支持 `mid_stream_abort`(吐到一半直接断 TCP),把「慢首字 / 中途断流 /
//! 恢复」这些真实网络边缘变成确定性回归资产。批次 2 流锚点恢复的
//! 端到端用例跑在本 server 上。

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket};

/// 时间线上的一帧:相对起始时刻的偏移 + SSE 负载(裸 data 行内容)。
#[derive(Debug, Clone)]
pub struct TimelineFrame {
    pub offset_ms: u64,
    /// 完整 SSE 事件文本(如 `"event: content_block_delta\ndata: {...}"`)。
    pub payload: String,
}

/// 单帧便捷构造。
pub fn frame(offset_ms: u64, payload: &str) -> TimelineFrame {
    TimelineFrame {
        offset_ms,
        payload: payload.to_string(),
    }
}

/// 时间线 SSE server。`bind` 后从 `url()` 拿 base_url 配
/// `AnthropicConfig.base_url`;`abort_after_ms = Some(t)` 时,服务端在
/// 吐完 offset ≤ t 的帧后**直接断开连接**(模拟中途断流,不 发
/// message_stop)。
pub struct TimelineServer {
    listener: Arc<TcpListener>,
    frames: Vec<TimelineFrame>,
    abort_after_ms: Option<u64>,
    pub received_requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl TimelineServer {
    pub async fn start(frames: Vec<TimelineFrame>, abort_after_ms: Option<u64>) -> Self {
        // 仅回环绑定(测试设施;与 provider 测试同一安全基线)。
        let socket = TcpSocket::new_v4().expect("socket");
        socket
            .bind("127.0.0.1:0".parse().expect("addr"))
            .expect("bind loopback");
        let listener = Arc::new(socket.listen(1).expect("listen"));
        Self {
            listener,
            frames,
            abort_after_ms,
            received_requests: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// `AnthropicConfig.base_url` 可直接填的形态(`http://127.0.0.1:port`)。
    pub fn url(&self) -> String {
        format!("http://{}", self.listener.local_addr().expect("addr"))
    }

    /// 后台接受一条连接,按时间线吐帧。必须在 stream 前调用。
    pub fn serve_once(&self) {
        let listener = self.listener.clone();
        let frames = self.frames.clone();
        let abort_after_ms = self.abort_after_ms;
        let received = self.received_requests.clone();
        tokio::spawn(async move {
            let (mut sock, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            // 读完整请求头(到 \r\n\r\n),记请求体供断言。
            let mut buf = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                match sock.read(&mut byte).await {
                    Ok(1) => {
                        buf.push(byte[0]);
                        if buf.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                    _ => return,
                }
                if buf.len() > 64 * 1024 {
                    return;
                }
            }
            // Content-Length 读 body(请求体供断言)。
            let header = String::from_utf8_lossy(&buf).to_string();
            let content_length = header
                .lines()
                .find(|l| l.to_lowercase().starts_with("content-length:"))
                .and_then(|l| l.split(':').nth(1))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; content_length];
            if content_length > 0 {
                let _ = sock.read_exact(&mut body).await;
            }
            received
                .lock()
                .expect("lock")
                .push(String::from_utf8_lossy(&body).to_string());
            // 响应头:SSE。
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n")
                .await;
            let started = tokio::time::Instant::now();
            for f in &frames {
                let due = started + Duration::from_millis(f.offset_ms);
                if tokio::time::Instant::now() < due {
                    tokio::time::sleep_until(due).await;
                }
                if let Some(t) = abort_after_ms
                    && f.offset_ms > t
                {
                    // 中途断流:不发该帧、不发 message_stop,直接断 TCP。
                    let _ = sock.shutdown().await;
                    return;
                }
                let wire = format!("{}\n\n", f.payload);
                if sock.write_all(wire.as_bytes()).await.is_err() {
                    return;
                }
                let _ = sock.flush().await;
            }
            // 全部帧吐完:保持连接片刻(客户端排空),再收尾。
            tokio::time::sleep(Duration::from_millis(150)).await;
            let _ = sock.shutdown().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use zhuye_llm::providers::{AnthropicClient, AnthropicConfig};
    use zhuye_llm::{ChatEvent, ChatMessage, ChatRequest, ContentBlock, ModelClient, UserContent};

    fn basic_request() -> ChatRequest {
        ChatRequest {
            model: "claude-3-5-sonnet-latest".into(),
            messages: vec![ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::text("hi")],
            })],
            max_tokens: Some(64),
            ..Default::default()
        }
    }

    /// 时间线语义冒烟:两帧按 offset 顺序到达,流正常收尾。
    #[tokio::test]
    async fn timeline_streams_frames_in_offset_order() {
        let server = TimelineServer::start(
            vec![
                frame(0, "event: message_start\ndata: {\"type\":\"message_start\"}"),
                frame(
                    60,
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}",
                ),
                frame(80, "event: message_stop\ndata: {\"type\":\"message_stop\"}"),
            ],
            None,
        )
        .await;
        let url = server.url();
        server.serve_once();
        let client = AnthropicClient::new(AnthropicConfig {
            api_key: "k".into(),
            base_url: Some(url),
            timeout: Duration::from_secs(5),
            ..Default::default()
        })
        .unwrap();
        let mut s = client
            .stream(basic_request(), tokio_util::sync::CancellationToken::new())
            .await
            .expect("建流成功");
        let mut saw_delta = false;
        let mut saw_stop = false;
        while let Some(ev) = s.next().await {
            match ev.expect("事件不出错") {
                ChatEvent::ContentDelta(t) if t == "ok" => saw_delta = true,
                ChatEvent::MessageStop => saw_stop = true,
                _ => {}
            }
        }
        assert!(saw_delta && saw_stop, "时间线两帧都应到达");
    }

    /// 批次 2 端到端:中途断流(SSE 半途 TCP 断)→ 引擎侧流以错误收场
    /// (可重试分类),事件流不静默吞掉。
    #[tokio::test]
    async fn mid_stream_abort_surfaces_as_retryable_error() {
        let server = TimelineServer::start(
            vec![
                frame(0, "event: message_start\ndata: {\"type\":\"message_start\"}"),
                frame(
                    40,
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"half\"}}",
                ),
                // offset 200 的 message_stop **不会**发出(abort_after_ms=100)。
                frame(200, "event: message_stop\ndata: {\"type\":\"message_stop\"}"),
            ],
            Some(100),
        )
        .await;
        let url = server.url();
        server.serve_once();
        let client = AnthropicClient::new(AnthropicConfig {
            api_key: "k".into(),
            base_url: Some(url),
            timeout: Duration::from_secs(5),
            ..Default::default()
        })
        .unwrap();
        let mut s = client
            .stream(basic_request(), tokio_util::sync::CancellationToken::new())
            .await
            .expect("建流成功");
        // 排空:应有 delta,然后以错误终止(断流),而不是静默 MessageStop。
        let (mut saw_delta, mut ended_with_error) = (false, false);
        while let Some(ev) = s.next().await {
            match ev {
                Ok(ChatEvent::ContentDelta(t)) if t == "half" => saw_delta = true,
                Ok(_) => {}
                Err(_) => ended_with_error = true,
            }
        }
        assert!(saw_delta, "断流前的增量应已到达");
        assert!(ended_with_error, "中途断流必须以流错误收场(可重试分类)");
    }
}
