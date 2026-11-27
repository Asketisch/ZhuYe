//! 串行回合调度的顺序、历史与取消回归。

use std::{pin::Pin, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::{Stream, stream};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use zhuye_core::{AgentConfig, AgentThread};
use zhuye_llm::{
    Capabilities, ChatEvent, ChatRequest, CredentialPool, LlmError, ModelClient, ModelRegistry,
    PoolEntry,
};
use zhuye_protocol::{AbortReason, EventMsg, Op, Submission, UserInputItem};
use zhuye_tools::ToolRegistry;

struct ControlledClient {
    calls: mpsc::UnboundedSender<ChatRequest>,
    permits: Arc<Semaphore>,
}

#[async_trait]
impl ModelClient for ControlledClient {
    fn name(&self) -> &str {
        "controlled"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    async fn stream(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>, LlmError> {
        self.calls.send(request).unwrap();
        let permits = self.permits.clone();
        Ok(Box::pin(stream::unfold(0, move |index| {
            let permits = permits.clone();
            async move {
                let event = match index {
                    0 => {
                        permits.acquire().await.unwrap().forget();
                        ChatEvent::MessageStart {
                            id: "reply".into(),
                            model: "controlled".into(),
                        }
                    }
                    1 => ChatEvent::ContentDelta("已完成上一轮".into()),
                    2 => ChatEvent::Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        cached_tokens: 0,
                        cache_write_tokens: 0,
                    },
                    3 => ChatEvent::MessageStop,
                    _ => return None,
                };
                Some((Ok(event), index + 1))
            }
        })))
    }
}

fn setup(
    serial: bool,
) -> (
    AgentThread,
    mpsc::UnboundedReceiver<ChatRequest>,
    Arc<Semaphore>,
) {
    setup_with_budget(serial, None)
}

fn setup_with_budget(
    serial: bool,
    budget: Option<Arc<zhuye_llm::task_budget::TaskBudget>>,
) -> (
    AgentThread,
    mpsc::UnboundedReceiver<ChatRequest>,
    Arc<Semaphore>,
) {
    let (calls, rx) = mpsc::unbounded_channel();
    let permits = Arc::new(Semaphore::new(0));
    let registry = Arc::new(ModelRegistry::new());
    registry.register_pool(
        "controlled",
        CredentialPool {
            entries: vec![PoolEntry {
                client: Arc::new(ControlledClient {
                    calls,
                    permits: permits.clone(),
                }),
                label: "test".into(),
                weight: 1,
            }],
        },
    );
    let mut cfg = AgentConfig::new("controlled/model", ".");
    cfg.serial_turns = serial;
    cfg.inherited_task_budget = budget;
    (
        AgentThread::new(cfg, registry, Arc::new(ToolRegistry::default()), None, None),
        rx,
        permits,
    )
}

fn submission(id: &str, op: Op) -> Submission {
    Submission {
        id: id.into(),
        op,
        client_user_message_id: None,
        trace: None,
        workspace: None,
        source_command: None,
    }
}

fn input(text: &str) -> Op {
    Op::UserInput {
        items: vec![UserInputItem::Text { text: text.into() }],
        thread_settings: Default::default(),
    }
}

async fn finished(handle: &mut zhuye_core::TurnHandle) -> EventMsg {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), handle.next())
            .await
            .unwrap()
            .unwrap();
        if matches!(
            event.msg,
            EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
        ) {
            return event.msg;
        }
    }
}

#[tokio::test]
async fn serial_fifo_rebuilds_history_after_previous_turn_finishes_without_recorder() {
    let (thread, mut calls, permits) = setup(true);
    let mut first = thread.submit(submission("first", input("第一条"))).await;
    let first_request = tokio::time::timeout(Duration::from_secs(5), calls.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        serde_json::to_string(&first_request.messages)
            .unwrap()
            .contains("第一条")
    );
    let mut second = thread.submit(submission("second", input("第二条"))).await;
    let mut third = thread.submit(submission("third", input("第三条"))).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), calls.recv())
            .await
            .is_err()
    );
    permits.add_permits(1);
    assert!(matches!(
        finished(&mut first).await,
        EventMsg::TurnComplete(_)
    ));
    let second_request = tokio::time::timeout(Duration::from_secs(5), calls.recv())
        .await
        .unwrap()
        .unwrap();
    let history = serde_json::to_string(&second_request.messages).unwrap();
    assert!(
        history.contains("第一条")
            && history.contains("已完成上一轮")
            && history.contains("第二条"),
        "{history}"
    );
    assert!(!history.contains("第三条"));
    permits.add_permits(1);
    assert!(matches!(
        finished(&mut second).await,
        EventMsg::TurnComplete(_)
    ));
    let third_request = tokio::time::timeout(Duration::from_secs(5), calls.recv())
        .await
        .unwrap()
        .unwrap();
    let history = serde_json::to_string(&third_request.messages).unwrap();
    assert!(
        history.contains("第一条") && history.contains("第二条") && history.contains("第三条"),
        "{history}"
    );
    permits.add_permits(1);
    assert!(matches!(
        finished(&mut third).await,
        EventMsg::TurnComplete(_)
    ));
}

#[tokio::test]
async fn interrupt_and_shutdown_abort_queued_inputs_without_starting_them() {
    for shutdown in [false, true] {
        let (thread, mut calls, _permits) = setup(true);
        let mut first = thread.submit(submission("first", input("第一条"))).await;
        tokio::time::timeout(Duration::from_secs(5), calls.recv())
            .await
            .unwrap()
            .unwrap();
        let mut queued = thread.submit(submission("queued", input("排队"))).await;
        let op = if shutdown {
            Op::Shutdown
        } else {
            Op::Interrupt { child_id: None }
        };
        let _control = thread.submit(submission("control", op)).await;
        let event = finished(&mut queued).await;
        match event {
            EventMsg::TurnAborted(ab) if shutdown => {
                assert!(matches!(ab.reason, AbortReason::Shutdown))
            }
            EventMsg::TurnAborted(ab) => assert!(matches!(ab.reason, AbortReason::UserInterrupt)),
            other => panic!("排队输入应当取消: {other:?}"),
        }
        assert!(matches!(
            finished(&mut first).await,
            EventMsg::TurnAborted(_)
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), calls.recv())
                .await
                .is_err()
                || shutdown
        );
    }
}

#[tokio::test]
async fn concurrent_mode_remains_default() {
    let (thread, mut calls, permits) = setup(false);
    let mut first = thread.submit(submission("first", input("第一条"))).await;
    tokio::time::timeout(Duration::from_secs(5), calls.recv())
        .await
        .unwrap()
        .unwrap();
    let mut second = thread.submit(submission("second", input("第二条"))).await;
    tokio::time::timeout(Duration::from_secs(5), calls.recv())
        .await
        .unwrap()
        .unwrap();
    permits.add_permits(2);
    assert!(matches!(
        finished(&mut first).await,
        EventMsg::TurnComplete(_)
    ));
    assert!(matches!(
        finished(&mut second).await,
        EventMsg::TurnComplete(_)
    ));
}

#[tokio::test]
async fn task_budget_denial_prevents_the_model_request() {
    use zhuye_llm::task_budget::{TaskBudget, TaskBudgetLimits};
    let budget = TaskBudget::new(TaskBudgetLimits {
        total_tokens: Some(1),
        ..Default::default()
    });
    let (thread, mut calls, _permits) = setup_with_budget(false, Some(budget.clone()));
    let mut handle = thread.submit(submission("denied", input("请求"))).await;
    match finished(&mut handle).await {
        EventMsg::TurnComplete(done) => assert!(matches!(
            done.status,
            zhuye_protocol::TurnStatus::TokenBudgetExceeded
        )),
        event => panic!("预算耗尽应产生明确终态: {event:?}"),
    }
    assert!(calls.try_recv().is_err());
    assert_eq!(budget.snapshot().reserved_tokens, 0);
    assert_eq!(budget.snapshot().settled_calls, 0);
}

#[tokio::test]
async fn task_budget_final_answer_is_bounded_and_settled() {
    use zhuye_llm::task_budget::{TaskBudget, TaskBudgetLimits};
    let budget = TaskBudget::new(TaskBudgetLimits {
        total_tokens: Some(1000),
        final_answer_tokens: 500,
        ..Default::default()
    });
    let (thread, mut calls, permits) = setup_with_budget(false, Some(budget.clone()));
    let mut handle = thread.submit(submission("final", input("答复"))).await;
    let request = tokio::time::timeout(Duration::from_secs(5), calls.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(request.tools.is_empty());
    assert!(request.max_tokens.is_some_and(|max| max <= 1000));
    permits.add_permits(1);
    assert!(matches!(
        finished(&mut handle).await,
        EventMsg::TurnComplete(_)
    ));
    assert_eq!(budget.snapshot().actual_tokens, 15);
    assert_eq!(budget.snapshot().settled_calls, 1);
    assert_eq!(budget.snapshot().reserved_tokens, 0);
}

#[tokio::test]
async fn task_deadline_cancels_an_inflight_model_and_releases_reservation() {
    use zhuye_llm::task_budget::{TaskBudget, TaskBudgetLimits};
    let budget = TaskBudget::new(TaskBudgetLimits {
        duration: Some(Duration::from_millis(500)),
        ..Default::default()
    });
    let (thread, mut calls, _permits) = setup_with_budget(false, Some(budget.clone()));
    let mut handle = thread.submit(submission("deadline", input("等待"))).await;
    tokio::time::timeout(Duration::from_secs(5), calls.recv())
        .await
        .unwrap()
        .unwrap();
    match finished(&mut handle).await {
        EventMsg::TurnComplete(done) => assert!(matches!(
            done.status,
            zhuye_protocol::TurnStatus::TokenBudgetExceeded
        )),
        event => panic!("截止时间应产生预算终态: {event:?}"),
    }
    assert_eq!(budget.snapshot().reserved_tokens, 0);
    assert_eq!(budget.snapshot().unknown_calls, 1);
}
