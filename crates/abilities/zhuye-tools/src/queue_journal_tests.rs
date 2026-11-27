//! 工具执行记录的失败关闭与身份关联测试。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use zhuye_hooks::HookEngine;
use zhuye_protocol::{
    ContentBlock, RolloutError, RolloutRecord, RolloutRecorder, SessionInfo, ThreadId,
    ToolExecutionPhase, ToolOutput, TurnId,
};

use super::{ToolCallRequest, ToolExecutionQueue};
use crate::{Tool, ToolContext, ToolError, ToolEventForwarder, ToolRegistry};

#[derive(Debug, Default)]
struct Journal {
    records: parking_lot::Mutex<Vec<RolloutRecord>>,
    fail_at: usize,
    writes: AtomicUsize,
    supported: bool,
}

#[async_trait]
impl RolloutRecorder for Journal {
    fn supports_strict_tool_journal(&self) -> bool {
        self.supported
    }

    async fn record(&self, record: RolloutRecord) -> Result<(), RolloutError> {
        let write = self.writes.fetch_add(1, Ordering::SeqCst) + 1;
        if write == self.fail_at {
            return Err(RolloutError::NoActiveFile);
        }
        self.records.lock().push(record);
        Ok(())
    }

    async fn replay(&self, _: ThreadId) -> Result<Vec<RolloutRecord>, RolloutError> {
        Ok(self.records.lock().clone())
    }
    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, RolloutError> {
        Ok(vec![])
    }
    async fn truncate_after(&self, _: Option<&TurnId>) -> Result<usize, RolloutError> {
        Ok(0)
    }
}

struct WriteProbe(Arc<AtomicUsize>);
#[async_trait]
impl Tool for WriteProbe {
    fn name(&self) -> &str {
        "write_probe"
    }
    fn description(&self) -> &str {
        "执行一次写入"
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({"type":"object"})
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    async fn execute(&self, _: ToolContext, _: Value) -> Result<ToolOutput, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput {
            content: vec![ContentBlock::text("完成")],
            is_error: false,
            metadata: serde_json::json!({}),
            elapsed_ms: 0,
        })
    }
}

async fn run(
    recorder: Option<Arc<Journal>>,
    strict: bool,
) -> (
    Vec<super::ToolResult>,
    Arc<AtomicUsize>,
    Vec<zhuye_protocol::Event>,
    ThreadId,
    TurnId,
) {
    let writes = Arc::new(AtomicUsize::new(0));
    let registry = Arc::new(ToolRegistry::new());
    registry.register(Arc::new(WriteProbe(writes.clone())));
    let queue = ToolExecutionQueue::with_defaults(
        registry,
        Arc::new(HookEngine::new()),
        ToolContext::default(),
    );
    let (tx, mut rx) = mpsc::channel(8);
    let mut fwd = ToolEventForwarder::new("sub", tx, vec![]);
    let session_id = ThreadId::new();
    let turn_id = TurnId::new();
    fwd.session_id = Some(session_id);
    fwd.turn_id = Some(turn_id);
    fwd.recorder = recorder.map(|r| r as Arc<dyn RolloutRecorder>);
    fwd.strict_tool_journal = strict;
    let result = queue
        .execute_all_with_progress(
            vec![ToolCallRequest {
                id: "call-1".into(),
                name: "write_probe".into(),
                args: serde_json::json!({}),
            }],
            None,
            Some(Arc::new(fwd)),
        )
        .await;
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    (result, writes, events, session_id, turn_id)
}

#[tokio::test]
async fn 严格模式缺少可靠记录器时不执行工具() {
    for recorder in [
        None,
        Some(Arc::new(Journal::default())),
        Some(Arc::new(Journal {
            fail_at: 1,
            supported: true,
            ..Default::default()
        })),
    ] {
        let (result, writes, _, _, _) = run(recorder, true).await;
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        assert!(result[0].is_error);
        assert_eq!(result[0].metadata["error"], "tool_journal_unavailable");
    }
}

#[tokio::test]
async fn 非严格模式记录失败不阻止工具执行() {
    let recorder = Arc::new(Journal {
        fail_at: 1,
        supported: false,
        ..Default::default()
    });
    let (result, writes, _, _, _) = run(Some(recorder), false).await;
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    assert!(!result[0].is_error);
}

#[tokio::test]
async fn 严格模式允许明确只读工具在没有记录器时执行() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.txt"), "内容").unwrap();
    let registry = Arc::new(ToolRegistry::new());
    registry.register(Arc::new(crate::builtins::ReadTool));
    let queue = ToolExecutionQueue::with_defaults(
        registry,
        Arc::new(HookEngine::new()),
        ToolContext::for_workspace(dir.path()),
    );
    let (tx, _rx) = mpsc::channel(8);
    let mut fwd = ToolEventForwarder::new("sub", tx, vec![]);
    fwd.strict_tool_journal = true;
    let results = queue
        .execute_all_with_progress(
            vec![ToolCallRequest {
                id: "read-1".into(),
                name: "read".into(),
                args: serde_json::json!({"path": "data.txt"}),
            }],
            None,
            Some(Arc::new(fwd)),
        )
        .await;
    assert!(!results[0].is_error);
    assert!(matches!(&results[0].content[0], ContentBlock::Text { text } if text.contains("内容")));
}

#[tokio::test]
async fn 等待开始记录时取消不会执行工具() {
    #[derive(Debug, Default)]
    struct WaitingJournal(tokio::sync::Notify);
    #[async_trait]
    impl RolloutRecorder for WaitingJournal {
        fn supports_strict_tool_journal(&self) -> bool {
            true
        }
        async fn record(&self, _: RolloutRecord) -> Result<(), RolloutError> {
            self.0.notify_one();
            std::future::pending().await
        }
        async fn replay(&self, _: ThreadId) -> Result<Vec<RolloutRecord>, RolloutError> {
            Ok(vec![])
        }
        async fn list_sessions(&self) -> Result<Vec<SessionInfo>, RolloutError> {
            Ok(vec![])
        }
        async fn truncate_after(&self, _: Option<&TurnId>) -> Result<usize, RolloutError> {
            Ok(0)
        }
    }
    let writes = Arc::new(AtomicUsize::new(0));
    let registry = Arc::new(ToolRegistry::new());
    registry.register(Arc::new(WriteProbe(writes.clone())));
    let queue = ToolExecutionQueue::with_defaults(
        registry,
        Arc::new(HookEngine::new()),
        ToolContext::default(),
    );
    let recorder = Arc::new(WaitingJournal::default());
    let (tx, _rx) = mpsc::channel(8);
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut fwd = ToolEventForwarder::new("sub", tx, vec![]);
    fwd.turn_cancel = Some(cancel.clone());
    fwd.recorder = Some(recorder.clone());
    fwd.strict_tool_journal = true;
    let task = tokio::spawn(async move {
        queue
            .execute_all_with_progress(
                vec![ToolCallRequest {
                    id: "call".into(),
                    name: "write_probe".into(),
                    args: serde_json::json!({}),
                }],
                None,
                Some(Arc::new(fwd)),
            )
            .await
    });
    recorder.0.notified().await;
    cancel.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert!(result[0].is_error);
    assert_eq!(result[0].metadata["error"], "cancelled");
    assert_eq!(writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn 开始和终态记录携带准确会话与回合标识() {
    let recorder = Arc::new(Journal {
        supported: true,
        ..Default::default()
    });
    let (result, writes, _, session_id, turn_id) = run(Some(recorder.clone()), true).await;
    assert!(!result[0].is_error);
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    let records = recorder.records.lock();
    assert_eq!(records.len(), 2);
    for (index, phase) in [ToolExecutionPhase::Started, ToolExecutionPhase::Completed]
        .into_iter()
        .enumerate()
    {
        assert!(matches!(&records[index], RolloutRecord::ToolExecution {
            session_id: sid, turn_id: tid, call_id, tool_name, phase: actual, ..
        } if *sid == session_id && *tid == turn_id && call_id == "call-1"
             && tool_name == "write_probe" && *actual == phase));
    }
}

#[tokio::test]
async fn 终态写入失败会通知宿主并留下未完成开始记录() {
    let recorder = Arc::new(Journal {
        supported: true,
        fail_at: 2,
        ..Default::default()
    });
    let (result, writes, events, _, _) = run(Some(recorder.clone()), true).await;
    assert!(!result[0].is_error);
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    assert_eq!(recorder.records.lock().len(), 1);
    assert!(events.iter().any(|event| matches!(&event.msg,
        zhuye_protocol::EventMsg::Error(error) if error.code == "tool_journal_completion_failed")));
}
