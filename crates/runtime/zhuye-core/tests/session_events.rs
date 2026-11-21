//! M6.1.2 — `AgentThread::subscribe_session` 对 `SessionConfigured` 与
//! `ShutdownComplete` 的事件分发。事件仍继续到达每 turn channel;
//! session 订阅者是给不愿把生命周期事件绑到具体提交的 TUI 消费者
//! 提供的额外 sink。

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{Stream, stream};
use parking_lot::Mutex;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use zhuye_core::{AgentConfig, AgentThread};
use zhuye_llm::{
    Capabilities, ChatEvent, ChatRequest, CredentialPool, LlmError, ModelClient, ModelRegistry,
    PoolEntry,
};
use zhuye_protocol::{EventMsg, Op, Submission, UserInputItem};
use zhuye_tools::ToolRegistry;

struct StubClient {
    events: Mutex<Vec<ChatEvent>>,
}

impl StubClient {
    fn new(events: Vec<ChatEvent>) -> Self {
        Self {
            events: Mutex::new(events),
        }
    }
}

#[async_trait]
impl ModelClient for StubClient {
    fn name(&self) -> &str {
        "stub"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    async fn stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        let events = self.events.lock().clone();
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

fn build_thread(registry: Arc<ModelRegistry>) -> AgentThread {
    let cfg = AgentConfig::new("stub/m1", Path::new("."));
    let tools = Arc::new(ToolRegistry::default());
    AgentThread::new(cfg, registry, tools, None, None)
}

fn user_input_sub(id: &str, text: &str) -> Submission {
    Submission {
        id: id.into(),
        op: Op::UserInput {
            items: vec![UserInputItem::Text { text: text.into() }],
            thread_settings: Default::default(),
        },
        client_user_message_id: None,
        trace: None,
        workspace: None,
        source_command: None,
    }
}

fn shutdown_sub(id: &str) -> Submission {
    Submission {
        id: id.into(),
        op: Op::Shutdown,
        client_user_message_id: None,
        trace: None,
        workspace: None,
        source_command: None,
    }
}

#[tokio::test]
async fn subscribe_session_receives_session_configured() {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(StubClient::new(vec![
                    ChatEvent::MessageStart {
                        id: "m1".into(),
                        model: "stub-1".into(),
                    },
                    ChatEvent::ContentDelta("hi".into()),
                    ChatEvent::MessageStop,
                ])),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = build_thread(registry);

    let mut session_rx = thread.subscribe_session();
    let _handle = thread.submit(user_input_sub("s1", "hi")).await;

    // SessionConfigured 必须送达 session 订阅者,即使它也会进入 per-turn 通道。
    let ev = timeout(Duration::from_secs(2), session_rx.recv())
        .await
        .expect("session event arrived within 2s")
        .expect("session channel still open");
    assert!(
        matches!(ev.msg, EventMsg::SessionConfigured(_)),
        "got {:?}",
        ev.msg
    );
}

#[tokio::test]
async fn subscribe_session_receives_shutdown_complete() {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(StubClient::new(vec![])),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = build_thread(registry);

    let mut session_rx = thread.subscribe_session();
    let _handle = thread.submit(shutdown_sub("s-shut")).await;

    let ev = timeout(Duration::from_secs(2), session_rx.recv())
        .await
        .expect("shutdown event arrived")
        .expect("channel open");
    assert!(
        matches!(ev.msg, EventMsg::ShutdownComplete),
        "got {:?}",
        ev.msg
    );
}

#[tokio::test]
async fn multiple_subscribers_each_get_the_event() {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(StubClient::new(vec![
                    ChatEvent::MessageStart {
                        id: "m1".into(),
                        model: "stub-1".into(),
                    },
                    ChatEvent::MessageStop,
                ])),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = build_thread(registry);

    let mut a = thread.subscribe_session();
    let mut b = thread.subscribe_session();
    let _handle = thread.submit(user_input_sub("s2", "hi")).await;

    let ev_a = timeout(Duration::from_secs(2), a.recv())
        .await
        .unwrap()
        .unwrap();
    let ev_b = timeout(Duration::from_secs(2), b.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(ev_a.msg, EventMsg::SessionConfigured(_)));
    assert!(matches!(ev_b.msg, EventMsg::SessionConfigured(_)));
}

#[tokio::test]
async fn session_configured_emitted_only_once_per_thread() {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(StubClient::new(vec![
                    ChatEvent::MessageStart {
                        id: "m1".into(),
                        model: "stub-1".into(),
                    },
                    ChatEvent::MessageStop,
                ])),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = build_thread(registry);

    let mut session_rx = thread.subscribe_session();
    let _h1 = thread.submit(user_input_sub("t1", "hi")).await;
    let _h2 = thread.submit(user_input_sub("t2", "hi again")).await;

    // 第一个事件必须是 SessionConfigured。
    let ev1 = timeout(Duration::from_secs(2), session_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(ev1.msg, EventMsg::SessionConfigured(_)));

    // 第二次提交不再产生 SessionConfigured。
    let next = timeout(Duration::from_millis(300), session_rx.recv()).await;
    match next {
        Err(_) => {} // timeout — expected: nothing else session-level
        Ok(Some(ev)) => assert!(
            !matches!(ev.msg, EventMsg::SessionConfigured(_)),
            "second SessionConfigured leaked: {:?}",
            ev.msg
        ),
        Ok(None) => {} // channel closed — also acceptable
    }
}

/// v1.x:`.with_session_id(sid)` 预分配 id 时,SessionConfigured 携带的
/// session_id 必须与之一致(而非 `new()` 内部的随机 id)—— GUI 按路由 id
/// 预分配 session,依赖事件 id == recorder 文件名 == SessionMeta 三者一致。
#[tokio::test]
async fn session_configured_reports_preallocated_session_id() {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(StubClient::new(vec![
                    ChatEvent::MessageStart {
                        id: "m1".into(),
                        model: "stub-1".into(),
                    },
                    ChatEvent::ContentDelta("hi".into()),
                    ChatEvent::MessageStop,
                ])),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let sid = zhuye_protocol::ThreadId::new();
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_session_id(sid);
    let tools = Arc::new(ToolRegistry::default());
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    let mut session_rx = thread.subscribe_session();
    let _handle = thread.submit(user_input_sub("s-pre", "hi")).await;

    let ev = timeout(Duration::from_secs(2), session_rx.recv())
        .await
        .expect("session event arrived within 2s")
        .expect("session channel still open");
    match ev.msg {
        EventMsg::SessionConfigured(sc) => assert_eq!(
            sc.session_id, sid,
            "SessionConfigured 必须回显预分配的 session_id"
        ),
        other => panic!("expected SessionConfigured, got {other:?}"),
    }
}

// ── v1.12:会话扇出序号(Event.seq)+ 挂起交互查询 ────────────────────

/// 会话扇出出口统一赋号:session 流上所有带 seq 的事件严格按 1,2,3…
/// 单调递增(同 loop 内无缺口);per-turn 专属流事件 seq = 0,不参与判定。
#[tokio::test]
async fn session_stream_events_have_monotonic_seq() {
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(StubClient::new(vec![
                    ChatEvent::MessageStart {
                        id: "m1".into(),
                        model: "stub-1".into(),
                    },
                    ChatEvent::ContentDelta("hi".into()),
                    ChatEvent::MessageStop,
                ])),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = build_thread(registry);

    let mut session_rx = thread.subscribe_session();
    let _handle = thread.submit(user_input_sub("s-seq", "hi")).await;
    // 首条提交触发 SessionConfigured,Shutdown 后再收终态。
    let _ = thread.submit(shutdown_sub("s-seq-shut")).await;

    let mut last_seq = 0u64;
    let mut stamped_seen = 0usize;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        match timeout(Duration::from_millis(300), session_rx.recv()).await {
            Ok(Some(ev)) => {
                if ev.seq > 0 {
                    assert_eq!(
                        ev.seq,
                        last_seq + 1,
                        "session 流 seq 必须严格 +1 单调,got {last_seq} → {}",
                        ev.seq
                    );
                    last_seq = ev.seq;
                    stamped_seen += 1;
                } else {
                    panic!("session 流事件不允许 seq = 0(必须由扇出出口赋号)");
                }
                if matches!(ev.msg, EventMsg::ShutdownComplete) {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    assert!(
        stamped_seen >= 2,
        "至少应收到 SessionConfigured 与 ShutdownComplete 两条赋号事件,实际 {stamped_seen}"
    );
}

/// 挂起交互查询面:注册表随 gate 注册而可见、随进程存活而权威。
/// 本测试验证空会话快照为空(不依赖审批流程;注册→可见的完整闭环由
/// zhuye-tools 的 approval 测试覆盖)。
#[tokio::test]
async fn pending_interactions_empty_on_fresh_thread() {
    let registry = Arc::new(ModelRegistry::new());
    let thread = build_thread(registry);
    assert!(
        thread.pending_interactions().is_empty(),
        "新会话必须无挂起交互"
    );
}
