//! 用 stub `ModelClient` 对 `AgentThread` 进行端到端测试。

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::{Stream, StreamExt, stream};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use zhuye_core::{AgentConfig, AgentThread};
use zhuye_llm::{
    Capabilities, ChatEvent, ChatRequest, CredentialPool, LlmError, ModelClient, ModelRegistry,
    PoolEntry,
};
use zhuye_protocol::{
    AbortReason, AgentMessageResetEvent, EventMsg, RoutingEventKind, Submission, UserInputItem,
};
use zhuye_tools::{ToolRegistry, builtins::EchoTool};

struct StubClient {
    events: Mutex<Vec<ChatEvent>>,
    /// 若设置,`stream()` 直接同步返回该错误(同步失败)。
    sync_error: Mutex<Option<LlmError>>,
    /// 若设置,流中每个 Ok 事件之后紧跟该错误(流中途失败)。
    mid_stream_error: Mutex<Option<LlmError>>,
    /// `stream` 被调用的次数。
    call_count: Mutex<u32>,
}

impl StubClient {
    fn new(events: Vec<ChatEvent>) -> Self {
        Self {
            events: Mutex::new(events),
            sync_error: Mutex::new(None),
            mid_stream_error: Mutex::new(None),
            call_count: Mutex::new(0),
        }
    }

    fn with_sync_error(err: LlmError) -> Self {
        let s = Self::new(vec![]);
        *s.sync_error.lock() = Some(err);
        s
    }

    fn with_mid_stream_error(events: Vec<ChatEvent>, err: LlmError) -> Self {
        let s = Self::new(events);
        *s.mid_stream_error.lock() = Some(err);
        s
    }

    /// 当前 `stream()` 已被调用的次数(供测试断言「调用上限」)。
    fn calls(&self) -> u32 {
        *self.call_count.lock()
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
        *self.call_count.lock() += 1;
        if let Some(e) = self.sync_error.lock().clone() {
            return Err(e);
        }
        let events = self.events.lock().clone();
        let mid = self.mid_stream_error.lock().clone();
        let s: Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>> =
            if let Some(e) = mid {
                let head = stream::iter(events.into_iter().map(Ok));
                let tail = stream::iter(std::iter::once(Err(e)));
                Box::pin(head.chain(tail))
            } else {
                Box::pin(stream::iter(events.into_iter().map(Ok)))
            };
        Ok(s)
    }
}

/// 按调用次数依次返回不同事件序列的 stub。第 N 次 `stream()` 返回第 N 段
/// 事件(用于测试多步回退:工具调用 → 工具结果 → 模型继续)。
struct SequentialStubClient {
    /// 每段是一次 `stream()` 调用的事件序列;超出段数时返回空流。
    rounds: Mutex<Vec<Vec<ChatEvent>>>,
}

impl SequentialStubClient {
    fn new(rounds: Vec<Vec<ChatEvent>>) -> Self {
        Self {
            rounds: Mutex::new(rounds),
        }
    }
}

#[async_trait]
impl ModelClient for SequentialStubClient {
    fn name(&self) -> &str {
        "stub-seq"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    async fn stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        // 从头部取(FIFO):第 1 次调用返回第 1 段。Vec::pop 是从尾部,
        // 故用 remove(0)。
        let events = if self.rounds.lock().is_empty() {
            vec![]
        } else {
            self.rounds.lock().remove(0)
        };
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

fn build_thread(registry: Arc<ModelRegistry>) -> AgentThread {
    let cfg = AgentConfig::new("stub/m1", Path::new("."));
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(EchoTool));
    AgentThread::new(cfg, registry, tools, None, None)
}

fn make_sub(text: &str) -> Submission {
    Submission {
        id: "test-sub".into(),
        op: zhuye_protocol::Op::UserInput {
            items: vec![UserInputItem::Text { text: text.into() }],
            thread_settings: Default::default(),
        },
        client_user_message_id: None,
        trace: None,
        workspace: None,
        source_command: None,
    }
}

#[tokio::test]
async fn single_turn_emits_expected_events() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("hi".into()),
        ChatEvent::Usage {
            input_tokens: 5,
            output_tokens: 1,
            cached_tokens: 0,
            cache_write_tokens: 0,
        },
        ChatEvent::MessageStop,
    ]));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = build_thread(registry);

    let mut handle = thread.submit(make_sub("echo hi")).await;

    let mut got: Vec<EventMsg> = Vec::new();
    while let Some(ev) = handle.next().await {
        got.push(ev.msg);
    }

    // 事件序列:SessionConfigured, TurnStarted, AgentMessageDelta("hi"),
    // TokenCount, TurnComplete
    assert!(matches!(got[0], EventMsg::SessionConfigured(_)));
    assert!(matches!(got[1], EventMsg::TurnStarted(_)));
    match &got[2] {
        EventMsg::AgentMessageDelta(d) => assert_eq!(d.delta, "hi"),
        other => panic!("expected AgentMessageDelta, got {other:?}"),
    }
    assert!(matches!(got[3], EventMsg::TokenCount(_)));
    let last = got.last().unwrap();
    assert!(matches!(last, EventMsg::TurnComplete(_)));
}

#[tokio::test]
async fn handles_tool_call_gracefully() {
    let registry = Arc::new(ModelRegistry::new());
    // 修复 ToolExec→PreLoop 后,工具执行完会回到模型继续推理。故 stub 需区分
    // 调用次数:第 1 次返回 bash 工具调用(未注册 → is_error),第 2 次返回
    // 纯文本让 turn 正常 TurnComplete(否则同一 ToolUse 会被无限重放直到
    // max_iterations)。
    let stub = Arc::new(SequentialStubClient::new(vec![
        // 第 1 次调用:工具调用
        vec![
            ChatEvent::MessageStart {
                id: "m1".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ToolUseStart {
                id: "tc1".into(),
                name: "bash".into(),
                input_json: String::new(),
            },
            ChatEvent::ToolUseDelta("{\"cmd\":\"ls\"}".into()),
            ChatEvent::MessageStop,
        ],
        // 第 2 次调用:工具失败后,模型给出纯文本并停止(无工具调用)
        vec![
            ChatEvent::MessageStart {
                id: "m2".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ContentDelta("done".into()),
            ChatEvent::MessageStop,
        ],
    ]));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = build_thread(registry);
    let mut handle = thread.submit(make_sub("list")).await;

    let mut got: Vec<EventMsg> = Vec::new();
    while let Some(ev) = handle.next().await {
        got.push(ev.msg);
    }
    let saw_begin = got
        .iter()
        .any(|m| matches!(m, EventMsg::ToolCallBegin(e) if e.call_id == "tc1"));
    let saw_end = got
        .iter()
        .any(|m| matches!(m, EventMsg::ToolCallEnd(e) if e.is_error));
    assert!(saw_begin, "expected ToolCallBegin");
    assert!(saw_end, "expected ToolCallEnd with is_error=true");
    // 仍必须产出 TurnComplete。
    assert!(got.iter().any(|m| matches!(m, EventMsg::TurnComplete(_))));
}

#[tokio::test]
async fn propagates_auth_error_without_turn_complete() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(StubClient::with_sync_error(LlmError::Auth));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = build_thread(registry);
    let mut handle = thread.submit(make_sub("hi")).await;

    let mut saw_error = false;
    let mut saw_turn_complete = false;
    while let Some(ev) = handle.next().await {
        match ev.msg {
            // v1.0 多 Provider 路由:Auth 走 CooldownAndFailover,单
            // credential 池时最终报 `ALL_CREDENTIALS_EXHAUSTED`,
            // details.tried 含原 AUTH_FAILED outcome。
            EventMsg::Error(ref e) if e.code == "ALL_CREDENTIALS_EXHAUSTED" => {
                saw_error = true;
                if let Some(details) = &e.details {
                    let tried = details.get("tried").and_then(|v| v.as_array());
                    if let Some(arr) = tried {
                        assert!(
                            arr.iter()
                                .any(|c| c.get("outcome").and_then(|o| o.as_str()) == Some("auth")),
                            "expected an auth outcome in tried: {details}"
                        );
                    }
                }
            }
            EventMsg::TurnComplete(_) => saw_turn_complete = true,
            _ => {}
        }
    }
    assert!(saw_error, "expected ALL_CREDENTIALS_EXHAUSTED error event");
    assert!(
        !saw_turn_complete,
        "auth error should not produce TurnComplete"
    );
}

#[tokio::test]
async fn retries_on_rate_limit_then_succeeds() {
    let registry = Arc::new(ModelRegistry::new());
    // v1.0 多 Provider 路由:RateLimited 触发 CooldownAndFailover,
    // 所以测试场景是"两个 credential 共享同一 pool,第一个 rate
    // -limited 失败,第二个成功" —— 旧测试"同 credential 重试"在
    // 新语义下已不再适用。
    let call_count_1 = Arc::new(Mutex::new(0u32));
    let call_count_2 = Arc::new(Mutex::new(0u32));
    let call_count_1_c = call_count_1.clone();
    let call_count_2_c = call_count_2.clone();
    struct FlakyClient {
        #[allow(dead_code)] // 留作诊断,registry 不读
        label: &'static str,
        call_count: Arc<Mutex<u32>>,
        first_error: Option<LlmError>,
    }
    #[async_trait]
    impl ModelClient for FlakyClient {
        fn name(&self) -> &str {
            "flaky"
        }
        async fn stream(
            &self,
            _req: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError>
        {
            let mut n = self.call_count.lock();
            *n += 1;
            if *n == 1
                && let Some(err) = self.first_error.clone()
            {
                return Err(err);
            }
            Ok(Box::pin(stream::iter(vec![
                Ok(ChatEvent::ContentDelta("ok".into())),
                Ok(ChatEvent::MessageStop),
            ])))
        }
    }
    let c1: Arc<dyn ModelClient> = Arc::new(FlakyClient {
        label: "work",
        call_count: call_count_1_c,
        first_error: Some(LlmError::RateLimited { retry_after_ms: 5 }),
    });
    let c2: Arc<dyn ModelClient> = Arc::new(FlakyClient {
        label: "personal",
        call_count: call_count_2_c,
        first_error: None,
    });
    registry.register_pool(
        "flaky",
        CredentialPool {
            entries: vec![
                PoolEntry {
                    client: c1,
                    label: "work".into(),
                    weight: 1,
                },
                PoolEntry {
                    client: c2,
                    label: "personal".into(),
                    weight: 1,
                },
            ],
        },
    );
    let cfg = AgentConfig::new("flaky/x", Path::new("."));
    let tools = Arc::new(ToolRegistry::default());
    let thread = AgentThread::new(cfg, registry, tools, None, None);
    let mut handle = thread.submit(make_sub("retry me")).await;

    let mut saw_stream_error = false;
    let mut saw_routing_switched = false;
    let mut saw_completion = false;
    while let Some(ev) = handle.next().await {
        match ev.msg {
            EventMsg::StreamError(ref s) if s.code == "RATE_LIMITED" => {
                saw_stream_error = true;
            }
            EventMsg::Routing(ref r)
                if matches!(r.kind, RoutingEventKind::Switched) && r.role == "main" =>
            {
                saw_routing_switched = true;
            }
            EventMsg::TurnComplete(_) => saw_completion = true,
            _ => {}
        }
    }
    assert!(saw_stream_error, "expected StreamError on rate limit");
    assert!(
        saw_routing_switched,
        "expected RoutingEvent(Switched) on credential failover"
    );
    assert!(saw_completion, "expected successful completion after retry");
    assert_eq!(*call_count_1.lock(), 1, "work credential called once");
    assert_eq!(
        *call_count_2.lock(),
        1,
        "personal credential called once after failover"
    );
}

#[tokio::test]
async fn multi_turn_emits_session_configured_only_once() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("hi".into()),
        ChatEvent::MessageStop,
    ]));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = build_thread(registry);

    // 第一回合。
    let mut h1 = thread.submit(make_sub("first")).await;
    let mut count1 = 0;
    while let Some(ev) = h1.next().await {
        if matches!(ev.msg, EventMsg::SessionConfigured(_)) {
            count1 += 1;
        }
    }
    assert_eq!(
        count1, 1,
        "SessionConfigured should be emitted exactly once on first turn"
    );

    // 第二回合 —— SessionConfigured 不应再次发出。
    let mut h2 = thread.submit(make_sub("second")).await;
    let mut count2 = 0;
    while let Some(ev) = h2.next().await {
        if matches!(ev.msg, EventMsg::SessionConfigured(_)) {
            count2 += 1;
        }
    }
    assert_eq!(
        count2, 0,
        "SessionConfigured should not re-emit on second turn"
    );
    assert!(h2.next().await.is_none(), "channel should close after turn");
}

#[tokio::test]
async fn cancellation_mid_stream_emits_turn_aborted() {
    use std::time::Duration;
    let registry = Arc::new(ModelRegistry::new());

    // 一个流保持打开直到被取消的 stub。
    struct SlowClient;
    #[async_trait]
    impl ModelClient for SlowClient {
        fn name(&self) -> &str {
            "slow"
        }
        async fn stream(
            &self,
            _req: ChatRequest,
            cancel: CancellationToken,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError>
        {
            // 先发 MessageStart,然后睡眠直到被取消。
            let s = async_stream::stream! {
                yield Ok(ChatEvent::MessageStart { id: "m".into(), model: "slow-1".into() });
                tokio::time::sleep(Duration::from_millis(500)).await;
                yield Ok(ChatEvent::MessageStop);
            };
            // 让流与 cancel token 竞争:cancel 先触发时上报 Cancelled。
            let cancel = cancel;
            let s = async_stream::stream! {
                let mut s = std::pin::pin!(s);
                loop {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            yield Ok(ChatEvent::Error(LlmError::Cancelled));
                            return;
                        }
                        evt = s.next() => {
                            match evt {
                                Some(e) => yield e,
                                None => return,
                            }
                        }
                    }
                }
            };
            Ok(Box::pin(s))
        }
    }
    registry.register_pool(
        "slow",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(SlowClient),
                label: "default".into(),
                weight: 1,
            }],
        },
    );

    let cancel = CancellationToken::new();
    let cfg = AgentConfig::new("slow/x", Path::new(".")).with_cancel(cancel.clone());
    let tools = Arc::new(ToolRegistry::default());
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    let mut handle = thread.submit(make_sub("hi")).await;
    // 给循环片刻时间开始流式输出,然后取消。
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();

    let mut aborted = Vec::new();
    let mut saw_turn_complete = false;
    while let Some(ev) = handle.next().await {
        match ev.msg {
            EventMsg::TurnAborted(a) => aborted.push(a.reason),
            EventMsg::TurnComplete(_) => saw_turn_complete = true,
            _ => {}
        }
    }
    assert!(
        matches!(aborted.as_slice(), [zhuye_protocol::AbortReason::Shutdown]),
        "会话令牌取消应产生唯一的关闭终态: {aborted:?}"
    );
    assert!(!saw_turn_complete, "取消后不应产生成功终态");
}

/// GAIA-fix: 当 agent 达到 `max_iterations` 上限时,turn 状态必须精确报告
/// `MaxIterations`,而非旧的硬编码 `> 32` 判断误报的 `Success`。
///
/// 设置 `max_iterations = 3`,用一个总是返回工具调用的 stub 强制触顶。
/// 预期:TurnComplete.status == `MaxIterations`(此前为 `Success`)。
#[tokio::test]
async fn max_iterations_reports_correct_status() {
    use zhuye_protocol::TurnStatus;
    let registry = Arc::new(ModelRegistry::new());
    // stub 每次都返回一个 bash 工具调用,迫使 agent 循环到 max_iterations。
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ToolUseStart {
            id: "tc1".into(),
            name: "bash".into(),
            input_json: String::new(),
        },
        ChatEvent::Usage {
            input_tokens: 5,
            output_tokens: 1,
            cached_tokens: 0,
            cache_write_tokens: 0,
        },
        ChatEvent::MessageStop,
    ]));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    // 关键:max_iterations 设为 3(< 旧的硬编码阈值 32),验证不再误报 Success。
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_max_iterations(3);
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(EchoTool));
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    let mut handle = thread.submit(make_sub("loop forever")).await;
    let mut final_status: Option<TurnStatus> = None;
    while let Some(ev) = handle.next().await {
        if let EventMsg::TurnComplete(tc) = ev.msg {
            final_status = Some(tc.status);
        }
    }
    assert_eq!(
        final_status,
        Some(TurnStatus::MaxIterations),
        "达到 max_iterations=3 时应报告 MaxIterations,而非 Success"
    );
}

/// GAIA-fix: 触顶后**不应直接终止**而让模型从未作答。改为发一次**无工具**
/// 的强制收口调用:模型在收口轮只能写文本,必然产出 `FINAL ANSWER:`。
///
/// 设置 max_iterations=3:前 3 轮只返回 bash 工具调用(逼到触顶),
/// 第 4 轮(强制收口)返回纯文本 "FINAL ANSWER: 42"。
/// 预期:聚合文本含 `FINAL ANSWER:`,turn 状态仍为 `MaxIterations`
/// (触顶是真因,但现在模型给出了答案 —— 此前直接 return None,无任何答案)。
#[tokio::test]
async fn max_iterations_emits_final_answer() {
    use zhuye_protocol::TurnStatus;
    let registry = Arc::new(ModelRegistry::new());

    // 一个工具调用事件(逼 agent 继续循环)
    let tool_call_round = vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ToolUseStart {
            id: "tc1".into(),
            name: "bash".into(),
            input_json: String::new(),
        },
        ChatEvent::Usage {
            input_tokens: 5,
            output_tokens: 1,
            cached_tokens: 0,
            cache_write_tokens: 0,
        },
        ChatEvent::MessageStop,
    ];
    // 强制收口轮:纯文本答案(无工具)
    let final_round = vec![
        ChatEvent::MessageStart {
            id: "m4".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("Based on what I found: FINAL ANSWER: 42".into()),
        ChatEvent::Usage {
            input_tokens: 5,
            output_tokens: 8,
            cached_tokens: 0,
            cache_write_tokens: 0,
        },
        ChatEvent::MessageStop,
    ];

    let stub = Arc::new(SequentialStubClient::new(vec![
        tool_call_round.clone(),
        tool_call_round.clone(),
        tool_call_round,
        final_round,
    ]));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_max_iterations(3);
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(EchoTool));
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    let mut handle = thread.submit(make_sub("keep searching")).await;
    let mut final_status: Option<TurnStatus> = None;
    let mut full_text = String::new();
    while let Some(ev) = handle.next().await {
        match ev.msg {
            EventMsg::AgentMessageDelta(d) => full_text.push_str(&d.delta),
            EventMsg::TurnComplete(tc) => final_status = Some(tc.status),
            _ => {}
        }
    }
    // 关键断言:触顶后模型**给出了 FINAL ANSWER**(此前直接终止,full_text 为空)
    assert!(
        full_text.contains("FINAL ANSWER: 42"),
        "强制收口轮应产出 FINAL ANSWER,聚合文本: {full_text:?}"
    );
    // 状态仍正确反映触顶(非 Success,保留诊断信号)
    assert_eq!(
        final_status,
        Some(TurnStatus::MaxIterations),
        "触顶 + 强制收口后状态应为 MaxIterations,实际: {final_status:?}"
    );
}

/// GAIA-fix 回归:强制收口是一次性的。若收口轮模型(异常地)仍只返回工具调用
/// (用总是返回 tool-call 的 StubClient 模拟),agent 必须在收口轮后**终止**,
/// 不能无限循环或无限烧 token。
#[tokio::test]
async fn max_iterations_force_final_answer_is_one_shot() {
    use zhuye_protocol::TurnStatus;
    let registry = Arc::new(ModelRegistry::new());
    // StubClient 每次都返回同一组 tool-call 事件(无限重复)。
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ToolUseStart {
            id: "tc1".into(),
            name: "bash".into(),
            input_json: String::new(),
        },
        ChatEvent::Usage {
            input_tokens: 5,
            output_tokens: 1,
            cached_tokens: 0,
            cache_write_tokens: 0,
        },
        ChatEvent::MessageStop,
    ]));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_max_iterations(3);
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(EchoTool));
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    let mut handle = thread.submit(make_sub("loop forever")).await;
    let mut final_status: Option<TurnStatus> = None;
    while let Some(ev) = handle.next().await {
        if let EventMsg::TurnComplete(tc) = ev.msg {
            final_status = Some(tc.status);
        }
    }
    // 收口轮一次性:stream 调用次数应 = max_iterations + 1(3 轮工具 + 1 轮收口),
    // 绝不会因收口轮仍带 tool-call 而继续。
    let calls = stub.calls();
    assert!(
        calls <= 4,
        "强制收口应一次性终止,stream 调用次数不应超过 {},实际: {calls}",
        4
    );
    assert_eq!(
        final_status,
        Some(TurnStatus::MaxIterations),
        "一次性收口后状态应为 MaxIterations"
    );
}

/// GAIA-fix: 当模型本轮输出被 provider 的 `max_tokens` 截断(发
/// `MessageStopTruncated`、无工具调用、文本里没有 `FINAL ANSWER:`)时,
/// `model_call` 应 auto-continue —— 追加一条 User "请继续并收口" 消息回到
/// `PreLoop`,让模型在下一轮补完作答,而非把半句话当完整答案收尾。
///
/// 设置:第 1 轮返回被截断的文本 + `MessageStopTruncated`(无 FINAL ANSWER);
/// 第 2 轮返回补全的作答 + 普通 `MessageStop`。
/// 预期:turn 最终正常 `TurnComplete(Success)`,且聚合文本含 `FINAL ANSWER:`。
#[tokio::test]
async fn auto_continue_after_max_tokens_truncation() {
    use zhuye_protocol::TurnStatus;
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(SequentialStubClient::new(vec![
        // 第 1 轮:输出被 max_tokens 截断(残缺文本,无 FINAL ANSWER,无工具)
        vec![
            ChatEvent::MessageStart {
                id: "m1".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ContentDelta("Let me reason about the cube... the removed cube is ".into()),
            ChatEvent::Usage {
                input_tokens: 5,
                output_tokens: 4096,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
            ChatEvent::MessageStopTruncated {
                stop_reason: "max_tokens".into(),
            },
        ],
        // 第 2 轮:模型续作并收口(给出 FINAL ANSWER)
        vec![
            ChatEvent::MessageStart {
                id: "m2".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ContentDelta("green, white.\n\nFINAL ANSWER: green, white".into()),
            ChatEvent::Usage {
                input_tokens: 6,
                output_tokens: 8,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
            ChatEvent::MessageStop,
        ],
    ]));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_max_iterations(20);
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(EchoTool));
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    let mut handle = thread
        .submit(make_sub("solve the rubiks cube riddle"))
        .await;
    let mut final_status: Option<TurnStatus> = None;
    let mut full_text = String::new();
    while let Some(ev) = handle.next().await {
        match ev.msg {
            EventMsg::AgentMessageDelta(d) => full_text.push_str(&d.delta),
            EventMsg::TurnComplete(tc) => final_status = Some(tc.status),
            _ => {}
        }
    }
    // auto-continue 发生:聚合文本应含续作后的 FINAL ANSWER。
    assert!(
        full_text.contains("FINAL ANSWER: green, white"),
        "auto-continue 后应含 FINAL ANSWER,聚合文本: {full_text:?}"
    );
    // 而且是正常 Success(不是 MaxIterations / TokenBudgetExceeded)。
    assert_eq!(
        final_status,
        Some(TurnStatus::Success),
        "auto-continue 续作完成后应正常 Success,而非其他状态"
    );
}

/// GAIA-fix 回归:auto-continue 受 `MAX_AUTO_CONTINUATIONS` 上限保护。当模型
/// 每轮都被截断(总是发 `MessageStopTruncated`)且不调用工具时,续作到上限后
/// 必须停止,不能无限续作。max_iterations 设得很大,验证真正叫停的是续作上限
/// 而非 max_iterations;turn 正常结束(不挂起、不无限烧 token)。
#[tokio::test]
async fn auto_continue_respects_max_continuations_cap() {
    use zhuye_protocol::TurnStatus;
    let registry = Arc::new(ModelRegistry::new());
    // 每一轮都返回截断(永远收不了口)。
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("still reasoning...".into()),
        ChatEvent::Usage {
            input_tokens: 5,
            output_tokens: 4096,
            cached_tokens: 0,
            cache_write_tokens: 0,
        },
        ChatEvent::MessageStopTruncated {
            stop_reason: "max_tokens".into(),
        },
    ]));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    // max_iterations 设得足够大,确保真正叫停的是 MAX_AUTO_CONTINUATIONS。
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_max_iterations(100);
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(EchoTool));
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    let mut handle = thread.submit(make_sub("never finish")).await;
    let mut final_status: Option<TurnStatus> = None;
    let mut stream_calls = 0u32;
    while let Some(ev) = handle.next().await {
        match ev.msg {
            EventMsg::TurnComplete(tc) => final_status = Some(tc.status),
            EventMsg::TokenCount(_) => stream_calls += 1,
            _ => {}
        }
    }
    // turn 必须终止(非无限续作),且因为最后一次仍截断 → 走 CheckStop → Success。
    assert_eq!(
        final_status,
        Some(TurnStatus::Success),
        "续作达上限后应正常 CheckStop 结束(非无限续作)"
    );
    // 续作必须停在 MAX_AUTO_CONTINUATIONS=3(即 1 次初始 + 3 次续作 = 4 次
    // model_call,另加 submission_loop 收尾的 1 次 TokenCount),绝不应跑到
    // max_iterations=100。给一个略宽的上界(6)容忍收尾事件,关键是「有界」。
    assert!(
        stream_calls <= 6,
        "续作应停在 ~4 次 model_call(有界),而非无限续作到 max_iterations=100(实际 {stream_calls})"
    );
}

/// thinking 转发回归:provider 发出 `ChatEvent::ThinkingDelta` 时,agent 必须
/// 把它转成 `EventMsg::ThinkingDelta` 发给 TUI(此前该变体落入 stream.rs 的
/// 通配符 `Some(Ok(_)) => {}` 被静默丢弃,TUI 永远收不到思考内容)。
#[tokio::test]
async fn thinking_delta_is_forwarded_as_event() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ThinkingDelta("let me think... ".into()),
        ChatEvent::ThinkingDelta("step by step".into()),
        ChatEvent::ContentDelta("answer".into()),
        ChatEvent::MessageStop,
    ]));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = build_thread(registry);

    let mut handle = thread.submit(make_sub("think hard")).await;

    let mut thinking_text = String::new();
    let mut saw_thinking_event = false;
    while let Some(ev) = handle.next().await {
        if let EventMsg::ThinkingDelta(td) = ev.msg {
            saw_thinking_event = true;
            thinking_text.push_str(&td.delta);
        }
    }
    assert!(
        saw_thinking_event,
        "ChatEvent::ThinkingDelta 必须被转发为 EventMsg::ThinkingDelta(此前被通配符吞掉)"
    );
    assert_eq!(
        thinking_text, "let me think... step by step",
        "多段 thinking delta 应被原样顺序转发"
    );
}

// ── v1.5 review:重试上限(failover cap)语义回归 ─────────────────

use zhuye_llm::RoutingPolicy;

/// 自定义 `RoutingPolicy` 的线程构造(`build_thread` 用默认策略)。
fn build_thread_with_policy(registry: Arc<ModelRegistry>, policy: RoutingPolicy) -> AgentThread {
    let cfg = AgentConfig::new("stub/m1", Path::new(".")).with_policy(Arc::new(policy));
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(EchoTool));
    AgentThread::new(cfg, registry, tools, None, None)
}

/// G2:总尝试上限触顶 → `MAX_ATTEMPTS` 错误事件收尾,不产生 TurnComplete,
/// 且排在后面的凭证不再被尝试。
///
/// 场景:4 个凭证全部在 stream 初始化即失败(SseParse → RetrySame 500ms
/// 退避),`max_attempts = 2`。第 3 次尝试前触顶 —— c1/c2 各被调 1 次,
/// c3/c4 不应被调用。此前该终态路径零测试覆盖。
#[tokio::test]
async fn max_attempts_cap_ends_turn_with_error_event() {
    let registry = Arc::new(ModelRegistry::new());
    let c1 = Arc::new(StubClient::with_sync_error(LlmError::SseParse(
        "bad-1".into(),
    )));
    let c2 = Arc::new(StubClient::with_sync_error(LlmError::SseParse(
        "bad-2".into(),
    )));
    let c3 = Arc::new(StubClient::with_sync_error(LlmError::SseParse(
        "bad-3".into(),
    )));
    let c4 = Arc::new(StubClient::with_sync_error(LlmError::SseParse(
        "bad-4".into(),
    )));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![
                PoolEntry {
                    client: c1.clone(),
                    label: "w1".into(),
                    weight: 1,
                },
                PoolEntry {
                    client: c2.clone(),
                    label: "w2".into(),
                    weight: 1,
                },
                PoolEntry {
                    client: c3.clone(),
                    label: "w3".into(),
                    weight: 1,
                },
                PoolEntry {
                    client: c4.clone(),
                    label: "w4".into(),
                    weight: 1,
                },
            ],
        },
    );
    let policy = RoutingPolicy {
        max_attempts: 2,
        ..RoutingPolicy::default()
    };
    let thread = build_thread_with_policy(registry, policy);
    let mut handle = thread.submit(make_sub("retry cap")).await;

    let mut saw_max_attempts = false;
    let mut saw_complete = false;
    while let Some(ev) = handle.next().await {
        match ev.msg {
            EventMsg::Error(e) if e.code == "MAX_ATTEMPTS" => {
                saw_max_attempts = true;
            }
            EventMsg::TurnComplete(_) => saw_complete = true,
            _ => {}
        }
    }
    assert!(saw_max_attempts, "触顶必须发 MAX_ATTEMPTS 错误事件");
    assert!(!saw_complete, "触顶的回合不应发 TurnComplete");
    // 注意:池按加权轮询派位,具体命中哪两个凭证不定 —— 只断言总量:
    // 恰好 2 次尝试(分布在不重复的 2 个凭证上,各 1 次)。
    let total: u32 = [c1.calls(), c2.calls(), c3.calls(), c4.calls()]
        .into_iter()
        .sum();
    assert_eq!(total, 2, "max_attempts=2 应恰好尝试 2 次后触顶");
    let per_cred = [c1.calls(), c2.calls(), c3.calls(), c4.calls()];
    assert_eq!(
        per_cred.iter().filter(|&&n| n == 1).count(),
        2,
        "应有 2 个凭证各被尝试 1 次: {per_cred:?}"
    );
    assert_eq!(
        per_cred.iter().filter(|&&n| n == 0).count(),
        2,
        "其余 2 个凭证不应被尝试: {per_cred:?}"
    );
}

/// G3:mid-stream RetrySame 的同凭证上限 —— 同一凭证最多连续尝试 2 次,
/// 超限且**还有备选凭证**时强制 failover(exclude)+ Switched 路由事件。
///
/// v1.10 语义变更:全部凭证触顶后不再以 `ALL_CREDENTIALS_EXHAUSTED`
/// 提前收场 —— 自动重试要烧满 `max_attempts` 总预算才放弃(用户要求
/// 「重试默认 10 次失败后再停下」),最后一个凭证在无备选时重置
/// per-cred 计数继续重试。本测试用 `max_attempts = 4` 验证:
/// c1 触顶 2 次 → failover 到 c2;c2 触顶后无备选 → 重置继续 →
/// 总尝试恰为 4 次,以 `MAX_ATTEMPTS` 收场。
///
/// 修复前(v1.5):双凭证全部触顶直接 ALL_CREDENTIALS_EXHAUSTED,
/// 总预算未烧完就放弃 —— 持续瞬时错误下重试次数远小于 max_attempts。
#[tokio::test(start_paused = true)]
async fn mid_stream_retry_same_is_capped_then_fails_over() {
    let registry = Arc::new(ModelRegistry::new());
    // 两个凭证在产生可见输出前都遇到持久性 SSE 错误,可以安全重试。
    let c1 = Arc::new(StubClient::with_mid_stream_error(
        vec![],
        LlmError::SseParse("garbled-a".into()),
    ));
    let c2 = Arc::new(StubClient::with_mid_stream_error(
        vec![],
        LlmError::SseParse("garbled-b".into()),
    ));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![
                PoolEntry {
                    client: c1.clone(),
                    label: "broken-a".into(),
                    weight: 1,
                },
                PoolEntry {
                    client: c2.clone(),
                    label: "broken-b".into(),
                    weight: 1,
                },
            ],
        },
    );
    let thread = build_thread_with_policy(
        registry.clone(),
        RoutingPolicy {
            max_attempts: 4,
            ..RoutingPolicy::default()
        },
    );
    let mut handle = thread.submit(make_sub("cap me")).await;

    let mut saw_all_exhausted = false;
    let mut saw_max_attempts = false;
    let mut saw_switched_exhausted = false;
    while let Some(ev) = handle.next().await {
        match ev.msg {
            EventMsg::Error(e) if e.code == "ALL_CREDENTIALS_EXHAUSTED" => {
                saw_all_exhausted = true;
            }
            EventMsg::Error(e) if e.code == "MAX_ATTEMPTS" => {
                saw_max_attempts = true;
            }
            EventMsg::Routing(r)
                if r.reason == "retry_same_exhausted"
                    && matches!(r.kind, RoutingEventKind::Switched) =>
            {
                saw_switched_exhausted = true;
            }
            _ => {}
        }
    }
    let total = c1.calls() + c2.calls();
    assert_eq!(
        total, 4,
        "总尝试烧满 max_attempts=4(v1.10 前为双凭证各 2 次提前收场)"
    );
    assert_eq!(c1.calls(), 2, "c1 触发同凭证上限后应 failover,恰好 2 次");
    assert_eq!(c2.calls(), 2, "c2 是最后凭证,与 c1 分摊剩余预算");
    assert!(
        !saw_all_exhausted,
        "v1.10:凭证触顶不再提前放弃,不应出现 ALL_CREDENTIALS_EXHAUSTED"
    );
    assert!(saw_max_attempts, "应烧满预算后以 MAX_ATTEMPTS 收场");
    assert!(
        saw_switched_exhausted,
        "超限 failover 应发 Switched(retry_same_exhausted) 路由事件"
    );
}

/// provider 把 usage 放在**结束原因之后**的额外 chunk 里
/// (OpenAI `stream_options.include_usage` / Kilo 免费池实测均如此)时,
/// `token_count` 必须仍能拿到真实用量 —— 否则配额与成本统计全部失真。
///
/// 回归:此前在 `MessageStop` 处直接 break,尾部的 usage chunk 被丢弃。
#[tokio::test]
async fn usage_after_message_stop_is_still_reported() {
    let registry = Arc::new(ModelRegistry::new());
    // 事件顺序刻意复刻真实 wire:先 finish_reason,后 usage。
    let stub = Arc::new(StubClient::new(vec![
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::ContentDelta("hi".into()),
        ChatEvent::MessageStop,
        // 真实 wire 形状:每个 chunk 都先产出一个 MessageStart(id/model 每帧都有),
        // usage chunk 前面还隔着一个 MessageStart —— 排空逻辑必须跳过它。
        ChatEvent::MessageStart {
            id: "m1".into(),
            model: "stub-1".into(),
        },
        ChatEvent::Usage {
            input_tokens: 42,
            output_tokens: 7,
            cached_tokens: 3,
            cache_write_tokens: 0,
        },
    ]));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = build_thread(registry);

    let mut handle = thread.submit(make_sub("echo hi")).await;
    let mut got: Vec<EventMsg> = Vec::new();
    while let Some(ev) = handle.next().await {
        got.push(ev.msg);
    }

    let token = got
        .iter()
        .find_map(|m| match m {
            EventMsg::TokenCount(t) => Some(t.clone()),
            _ => None,
        })
        .expect("应有 TokenCount 事件");
    assert_eq!(token.input_tokens, 42, "结束原因之后的 usage 必须被采集");
    assert_eq!(token.output_tokens, 7);
    assert_eq!(token.cached_tokens, 3);
    assert_eq!(token.total_tokens, 49);
}

// ── 工具参数投递:整包形态 ────────────────────────────────────────────────
//
// provider 投递工具参数有两种形态:
// - **分片形态**:ToolUseStart 带空 `input_json`,参数由若干 ToolUseDelta
//   分片送来(Anthropic / OpenAI / Responses)。
// - **整包形态**:ToolUseStart 直接带**完整** `input_json`,**没有** delta
//   (Ollama 原生格式;mock provider 发整包 + 一份重复 delta)。
//
// 此前核心只累积 delta、把 `input_json` 整个忽略 → 整包形态的 provider
// (Ollama)**参数全丢**,工具收到 `args = null`,表现为「missing 'cmd'」
// 这类假故障(实测于 L6 实模型演练层跑本地 Ollama 时)。

/// 记录收到的参数的探针工具。
struct ArgsProbeTool {
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
}

#[async_trait]
impl zhuye_tools::Tool for ArgsProbeTool {
    fn name(&self) -> &str {
        "probe"
    }
    fn description(&self) -> &str {
        "records the args it received"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "cmd": { "type": "string" } },
            "required": ["cmd"]
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        _ctx: zhuye_tools::ToolContext,
        args: serde_json::Value,
    ) -> Result<zhuye_tools::ToolOutput, zhuye_tools::ToolError> {
        self.seen.lock().push(args);
        Ok(zhuye_tools::ToolOutput {
            content: vec![zhuye_protocol::ContentBlock::text("ok")],
            is_error: false,
            metadata: serde_json::Value::Null,
            elapsed_ms: 0,
        })
    }
}

/// 整包形态(ToolUseStart 带完整 input_json、**没有** delta)→ 参数必须送达工具。
#[tokio::test]
async fn tool_args_from_start_only_are_not_lost() {
    let registry = Arc::new(ModelRegistry::new());
    let stub = Arc::new(SequentialStubClient::new(vec![
        // 第 1 次:整包形态 —— 没有 ToolUseDelta。
        vec![
            ChatEvent::MessageStart {
                id: "m1".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ToolUseStart {
                id: "tc1".into(),
                name: "probe".into(),
                input_json: r#"{"cmd":"echo hi"}"#.into(),
            },
            ChatEvent::MessageStop,
        ],
        // 第 2 次:纯文本收尾。
        vec![
            ChatEvent::MessageStart {
                id: "m2".into(),
                model: "stub-1".into(),
            },
            ChatEvent::ContentDelta("done".into()),
            ChatEvent::MessageStop,
        ],
    ]));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: stub.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );

    let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let cfg = AgentConfig::new("stub/m1", Path::new("."));
    let tools = Arc::new(ToolRegistry::default());
    tools.register(Arc::new(ArgsProbeTool { seen: seen.clone() }));
    let thread = AgentThread::new(cfg, registry, tools, None, None);

    let mut handle = thread.submit(make_sub("go")).await;
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, EventMsg::TurnComplete(_)) {
            break;
        }
    }

    let seen = seen.lock().clone();
    assert_eq!(seen.len(), 1, "探针工具应被调用一次,实际 {seen:?}");
    assert_eq!(
        seen[0]["cmd"], "echo hi",
        "整包形态的工具参数必须送达工具,实际:{:?}",
        seen[0]
    );
}

// ── 目标模式:没有 LLM 客户端时必须**如实上报** ──────────────────────────

/// 目标模式在**没有可用 LLM 客户端**时必须**如实上报**,不能静默丢弃。
///
/// 此前是 `warn` + `continue`:命令返回了 submission id(看着像成功),
/// GUI 的「目标模式」投影还亮着,而引擎里**根本没有** controller ——
/// 校验永不发生,用户无从得知。现在发一条 `GOAL_NO_CLIENT` 的 error 事件。
#[tokio::test]
async fn goal_mode_without_client_reports_error() {
    // 空 registry —— 没有任何 provider / 凭据 → `next_for` 必然 None。
    let registry = Arc::new(ModelRegistry::new());
    let thread = build_thread(registry);
    let mut session_rx = thread.subscribe_session();

    let submission = Submission {
        id: "goal-sub".into(),
        op: zhuye_protocol::Op::EnterGoalMode {
            goal: "probe goal".into(),
            verify_command: None,
            token_budget: None,
        },
        client_user_message_id: None,
        trace: None,
        workspace: None,
        source_command: None,
    };
    let mut handle = thread.submit(submission).await;

    // 收事件:要么是 error(GOAL_NO_CLIENT),要么是 submission_closed。
    let mut saw_error = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_secs(2), session_rx.recv()).await {
            Ok(Some(ev)) => {
                if let zhuye_protocol::EventMsg::Error(e) = &ev.msg {
                    assert_eq!(e.code, "GOAL_NO_CLIENT", "错误码不对:{e:?}");
                    assert!(
                        e.message.contains("目标模式"),
                        "错误文案应说明目标模式没开成:{e:?}"
                    );
                    saw_error = true;
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    // turn 侧也应收尾(不能挂住)。
    while let Some(ev) = handle.next().await {
        if matches!(ev.msg, zhuye_protocol::EventMsg::TurnComplete(_)) {
            break;
        }
    }
    assert!(
        saw_error,
        "没有 LLM 客户端时目标模式必须发 GOAL_NO_CLIENT 错误事件,而不是静默丢弃"
    );
}

// ── v1.10:自动重试(可见增量重试化 / 单凭证烧满预算)+ 失败终态 ──────

/// v1.10 夹具:第 1 次调用在已产出可见增量后中途断流(SSE 中断的典型
/// 形态 —— 正是 SSE_PARSE_ERROR 事故的复现形态),第 2 次起返回完整
/// 正常流。用于验证「有可见增量的 mid-stream 错误自动重试 +
/// AgentMessageReset 清缓冲、两次输出不拼接」。
struct RecoveringClient {
    calls: Mutex<u32>,
}

#[async_trait]
impl ModelClient for RecoveringClient {
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
        let n = {
            let mut c = self.calls.lock();
            *c += 1;
            *c
        };
        let items: Vec<Result<ChatEvent, LlmError>> = if n == 1 {
            vec![
                Ok(ChatEvent::ContentDelta("partial".into())),
                Err(LlmError::SseParse("connection reset mid-stream".into())),
            ]
        } else {
            vec![
                Ok(ChatEvent::ContentDelta("complete answer".into())),
                Ok(ChatEvent::MessageStop),
            ]
        };
        Ok(Box::pin(stream::iter(items)))
    }
}

/// v1.10:有可见增量的 mid-stream 可重试错误(SSE 中断)→ 自动重试,
/// 重试前发 `AgentMessageReset` 让客户端清半成品缓冲;最终文本只有
/// 重试后的完整输出,两次输出不拼接。
///
/// 修复前:可见增量直接 GiveUp(「重新调用模型会把两次输出拼在一
/// 起」),一次瞬时断流就把整回合打死 —— Desktop 会话
/// SSE_PARSE_ERROR 一击必杀的直接原因。
#[tokio::test(start_paused = true)]
async fn mid_stream_visible_output_error_retries_with_reset() {
    let registry = Arc::new(ModelRegistry::new());
    let client = Arc::new(RecoveringClient {
        calls: Mutex::new(0),
    });
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client,
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = build_thread(registry);
    let mut handle = thread.submit(make_sub("flaky stream")).await;

    let mut reset_events: Vec<AgentMessageResetEvent> = Vec::new();
    let mut saw_stream_error = false;
    // 模拟客户端语义:AgentMessageReset 到达时清空流式缓冲(与
    // Desktop / CLI 的 adapter 行为一致),再累积后续增量 —— 最终
    // 文本只能来自重试后的完整输出。
    let mut streamed_text = String::new();
    let mut saw_turn_complete = false;
    while let Some(ev) = handle.next().await {
        match ev.msg {
            EventMsg::AgentMessageReset(r) => {
                streamed_text.clear();
                reset_events.push(r);
            }
            EventMsg::StreamError(s) if s.code == "SSE_PARSE_ERROR" => saw_stream_error = true,
            EventMsg::AgentMessageDelta(d) => streamed_text.push_str(&d.delta),
            EventMsg::TurnComplete(_) => saw_turn_complete = true,
            _ => {}
        }
    }
    assert_eq!(
        reset_events.len(),
        1,
        "重试前应恰好发一次 AgentMessageReset"
    );
    assert_eq!(
        reset_events[0].reason_code, "SSE_PARSE_ERROR",
        "reset 事件携带触发重试的错误码"
    );
    assert!(
        reset_events[0].retry_in_ms > 0,
        "reset 事件应携带重试退避时长"
    );
    assert!(saw_stream_error, "重试前应发 StreamError 说明失败原因");
    assert_eq!(
        streamed_text, "complete answer",
        "最终文本只能是重试后的完整输出(partial 已被 reset 清除,不拼接)"
    );
    assert!(saw_turn_complete, "重试成功后 turn 正常完成");
    // 夹具自证:恰好调用了 2 次(1 次失败 + 1 次成功)。
    // (RecoveringClient 无 calls() 访问器,次数由 reset_events.len()==1
    //  与成功收尾共同佐证。)
}

/// v1.10:不可重试错误(4xx InvalidRequest)+ 可见增量 → 保留部分文本、
/// 发 Error 后**补发 TurnAborted(AbortReason::Error)终态**。
///
/// 修复前:失败回合只发 Error、无任何 turn 终态事件,客户端 turn 状态
/// 永远停在 streaming(Desktop 失败后停止按钮恒显、点击 no-op 的根因)。
#[tokio::test]
async fn failed_turn_emits_turn_aborted_terminal() {
    let registry = Arc::new(ModelRegistry::new());
    // InvalidRequest(400)分类为 GiveUp:不重试,走 deferred_error 路径。
    let client = Arc::new(StubClient::with_mid_stream_error(
        vec![ChatEvent::ContentDelta("partial".into())],
        LlmError::InvalidRequest {
            message: "bad request".into(),
        },
    ));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client,
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = build_thread(registry);
    let mut handle = thread.submit(make_sub("will fail")).await;

    let mut started_turn_id: Option<String> = None;
    let mut error_code: Option<String> = None;
    let mut aborted: Option<(String, AbortReason)> = None;
    let mut saw_turn_complete = false;
    // 记录事件顺序:Error 必须先于 TurnAborted。
    let mut error_before_abort = false;
    while let Some(ev) = handle.next().await {
        match ev.msg {
            EventMsg::TurnStarted(t) => started_turn_id = Some(t.turn_id.to_string()),
            EventMsg::Error(e) => {
                error_code = Some(e.code);
                error_before_abort = aborted.is_none();
            }
            EventMsg::TurnAborted(t) => aborted = Some((t.turn_id.to_string(), t.reason)),
            EventMsg::TurnComplete(_) => saw_turn_complete = true,
            _ => {}
        }
    }
    assert_eq!(error_code.as_deref(), Some("INVALID_REQUEST"));
    let (abort_turn_id, reason) = aborted.expect("失败回合必须补发 TurnAborted 终态");
    assert_eq!(
        Some(&abort_turn_id),
        started_turn_id.as_ref(),
        "TurnAborted 必须携带真实 turn_id(此前空闲中断回执用现编 id,客户端配对不到)"
    );
    // 失败终态的 reason 是 AbortReason::Error,携带与 Error 事件一致的错误码。
    match reason {
        AbortReason::Error { code, message } => {
            assert_eq!(code, "INVALID_REQUEST");
            assert!(message.contains("bad request"));
        }
        other => panic!("失败终态 reason 应为 AbortReason::Error,实际 {other:?}"),
    }
    assert!(error_before_abort, "Error 应先于 TurnAborted 发出");
    assert!(!saw_turn_complete, "失败回合不应发 TurnComplete");
}

/// v1.10:单凭证 mid-stream 持久瞬时错误 → 烧满 max_attempts 全预算
/// 才放弃(不再被 per-cred 上限 2 提前打死成 ALL_CREDENTIALS_EXHAUSTED)。
#[tokio::test(start_paused = true)]
async fn single_credential_burns_full_retry_budget() {
    let registry = Arc::new(ModelRegistry::new());
    let client = Arc::new(StubClient::with_mid_stream_error(
        vec![],
        LlmError::SseParse("persistent".into()),
    ));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: client.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = build_thread_with_policy(
        registry,
        RoutingPolicy {
            max_attempts: 4,
            ..RoutingPolicy::default()
        },
    );
    let mut handle = thread.submit(make_sub("keep trying")).await;

    let mut saw_max_attempts = false;
    let mut saw_all_exhausted = false;
    while let Some(ev) = handle.next().await {
        match ev.msg {
            EventMsg::Error(e) if e.code == "MAX_ATTEMPTS" => saw_max_attempts = true,
            EventMsg::Error(e) if e.code == "ALL_CREDENTIALS_EXHAUSTED" => {
                saw_all_exhausted = true;
            }
            _ => {}
        }
    }
    assert_eq!(client.calls(), 4, "单凭证应烧满 max_attempts=4 次预算");
    assert!(saw_max_attempts, "应以 MAX_ATTEMPTS 收场");
    assert!(!saw_all_exhausted, "单凭证重试不再被 per-cred 上限提前打死");
}

/// v1.10:单凭证**建流阶段**持久瞬时错误(如连接失败)同样烧满预算 ——
/// 此前初始化错误路径的 RetrySame 无条件 exclude 当前凭证,单凭证一次
/// 失败就直接 ALL_CREDENTIALS_EXHAUSTED,自动重试完全不可达。
#[tokio::test(start_paused = true)]
async fn single_credential_init_error_burns_full_retry_budget() {
    let registry = Arc::new(ModelRegistry::new());
    let client = Arc::new(StubClient::with_sync_error(LlmError::Http(
        "connection refused".into(),
    )));
    registry.register_pool(
        "stub",
        CredentialPool {
            entries: vec![PoolEntry {
                client: client.clone(),
                label: "default".into(),
                weight: 1,
            }],
        },
    );
    let thread = build_thread_with_policy(
        registry,
        RoutingPolicy {
            max_attempts: 3,
            ..RoutingPolicy::default()
        },
    );
    let mut handle = thread.submit(make_sub("init fails")).await;

    let mut saw_max_attempts = false;
    let mut saw_all_exhausted = false;
    while let Some(ev) = handle.next().await {
        match ev.msg {
            EventMsg::Error(e) if e.code == "MAX_ATTEMPTS" => saw_max_attempts = true,
            EventMsg::Error(e) if e.code == "ALL_CREDENTIALS_EXHAUSTED" => {
                saw_all_exhausted = true;
            }
            _ => {}
        }
    }
    assert_eq!(client.calls(), 3, "建流失败应同凭证重试烧满 max_attempts=3");
    assert!(saw_max_attempts, "应以 MAX_ATTEMPTS 收场");
    assert!(
        !saw_all_exhausted,
        "单凭证建流失败不再一次就 ALL_CREDENTIALS_EXHAUSTED"
    );
}
