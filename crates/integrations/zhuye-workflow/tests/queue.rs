use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use zhuye_workflow::{Control, Flow, Node, NodeInput, NodeResult, NodeSpec, Status};

struct Echo;
#[async_trait]
impl Node for Echo {
    async fn execute(&self, input: NodeInput) -> Result<NodeResult, String> {
        Ok(NodeResult::success(json!({"seen": input.variables["x"]})))
    }
}

#[tokio::test]
async fn queues_same_type_with_distinct_instances_and_runs_finally() {
    let node: Arc<dyn Node> = Arc::new(Echo);
    let mut flow = Flow::new(json!({"x": 7})).unwrap();
    flow.push(NodeSpec::new("one", node.clone())).unwrap();
    flow.push(NodeSpec::new("two", node.clone())).unwrap();
    flow.push_finally(NodeSpec::new("cleanup", node)).unwrap();
    let report = flow.run(Control::default()).await;
    assert_eq!(report.status, Status::Success);
    assert_eq!(report.history.len(), 2);
    assert_eq!(report.history[1].output, json!({"seen": 7}));
    assert_eq!(report.finally_history[0].node_id, "cleanup");
}

struct Fail;
#[async_trait]
impl Node for Fail {
    async fn execute(&self, _: NodeInput) -> Result<NodeResult, String> {
        Err("坏节点".into())
    }
}

#[tokio::test]
async fn failure_stops_main_queue_but_runs_cleanup() {
    let mut flow = Flow::new(Value::Object(Default::default())).unwrap();
    flow.push(NodeSpec::new("fail", Arc::new(Fail))).unwrap();
    flow.push(NodeSpec::new("not-run", Arc::new(Fail))).unwrap();
    flow.push_finally(NodeSpec::new("cleanup", Arc::new(Fail)))
        .unwrap();
    let report = flow.run(Control::default()).await;
    assert_eq!(report.status, Status::Failed);
    assert_eq!(report.history.len(), 1);
    assert_eq!(report.finally_history.len(), 1);
    assert_eq!(report.finally_history[0].status, Status::Failed);
}

struct Back;
#[async_trait]
impl Node for Back {
    async fn execute(&self, input: NodeInput) -> Result<NodeResult, String> {
        let mut result = NodeResult::success(json!(input.attempt));
        if input.attempt == 1 {
            result.action = zhuye_workflow::Action::Back {
                target: "again".into(),
            };
        }
        Ok(result)
    }
}

#[tokio::test]
async fn backward_jump_respects_instance_attempt_limit() {
    let mut flow = Flow::new(json!({})).unwrap();
    let mut spec = NodeSpec::new("again", Arc::new(Back));
    spec.max_runs = 2;
    flow.push(spec).unwrap();
    let report = flow.run(Control::default()).await;
    assert_eq!(report.status, Status::Success);
    assert_eq!(report.history.len(), 2);
    assert_eq!(report.history[1].attempt, 2);
    assert_ne!(report.history[0].attempt_id, report.history[1].attempt_id);
}

struct Wait;
#[async_trait]
impl Node for Wait {
    async fn execute(&self, _: NodeInput) -> Result<NodeResult, String> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn cancellation_interrupts_node_and_runs_cleanup() {
    let mut flow = Flow::new(json!({"x": 5})).unwrap();
    flow.push(NodeSpec::new("wait", Arc::new(Wait))).unwrap();
    flow.push_finally(NodeSpec::new("cleanup", Arc::new(Echo)))
        .unwrap();
    let control = Control::default();
    let child = control.clone();
    let task = tokio::spawn(async move { flow.run(child).await });
    tokio::task::yield_now().await;
    control.cancel();
    let report = task.await.unwrap();
    assert_eq!(report.status, Status::Cancelled);
    assert_eq!(report.history[0].status, Status::Cancelled);
    assert_eq!(report.finally_history[0].output, json!({"seen": 5}));
}

struct Block;
#[async_trait]
impl Node for Block {
    async fn execute(&self, _: NodeInput) -> Result<NodeResult, String> {
        let mut result = NodeResult::success(json!({"message": "等待服务"}));
        result.status = Status::Blocked;
        result.action = zhuye_workflow::Action::Wait {
            reason: "服务不可用".into(),
        };
        result.updates.insert("x".into(), json!(9));
        Ok(result)
    }
}

#[tokio::test]
async fn blocked_snapshot_retains_context_until_explicit_resume() {
    let mut flow = Flow::new(json!({"x": 1})).unwrap();
    flow.push(NodeSpec::new("block", Arc::new(Block))).unwrap();
    flow.push(NodeSpec::new("next", Arc::new(Echo))).unwrap();
    let control = Control::default();
    let runner_control = control.clone();
    let task = tokio::spawn(async move { flow.run(runner_control).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while control.snapshot().state != zhuye_workflow::RunState::Blocked {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let snapshot = control.snapshot();
    assert_eq!(snapshot.reason.as_deref(), Some("服务不可用"));
    assert_eq!(snapshot.variables["x"], 9);
    assert_eq!(snapshot.history[0].status, Status::Blocked);
    control.resume();
    let report = task.await.unwrap();
    assert_eq!(report.status, Status::Success);
    assert_eq!(report.history[1].output, json!({"seen": 9}));
    assert_eq!(
        control.snapshot().state,
        zhuye_workflow::RunState::Completed
    );
}

struct Insert;
#[async_trait]
impl Node for Insert {
    async fn execute(&self, _: NodeInput) -> Result<NodeResult, String> {
        let mut result = NodeResult::success(json!(null));
        result.insertions = vec![
            zhuye_workflow::Insertion {
                id: "late".into(),
                node_type: "echo".into(),
                after: "end".into(),
                config: json!({}),
            },
            zhuye_workflow::Insertion {
                id: "early".into(),
                node_type: "echo".into(),
                after: "insert".into(),
                config: json!({}),
            },
        ];
        Ok(result)
    }
}

#[tokio::test]
async fn insertion_validates_and_orders_nodes_at_different_anchors() {
    let mut flow = Flow::new(json!({"x": 3})).unwrap();
    flow.register("echo", Arc::new(Echo)).unwrap();
    flow.push(NodeSpec::new("insert", Arc::new(Insert)))
        .unwrap();
    flow.push(NodeSpec::new("end", Arc::new(Echo))).unwrap();
    let report = flow.run(Control::default()).await;
    assert_eq!(report.status, Status::Success);
    let ids: Vec<_> = report.history.iter().map(|r| r.node_id.as_str()).collect();
    assert_eq!(ids, vec!["insert", "early", "end", "late"]);
}

#[tokio::test]
async fn conditions_skip_a_node_and_are_recorded() {
    let mut flow = Flow::new(json!({"x": 3})).unwrap();
    let mut node = NodeSpec::new("conditional", Arc::new(Fail));
    node.skip_if = Some(zhuye_workflow::SkipCondition {
        variable: "x".into(),
        equals: json!(3),
    });
    flow.push(node).unwrap();
    flow.push(NodeSpec::new("next", Arc::new(Echo))).unwrap();
    let report = flow.run(Control::default()).await;
    assert_eq!(report.status, Status::Success);
    assert_eq!(report.history[0].status, Status::Skipped);
    assert_eq!(report.history[1].output, json!({"seen": 3}));
}

struct InvalidInsert;
#[async_trait]
impl Node for InvalidInsert {
    async fn execute(&self, _: NodeInput) -> Result<NodeResult, String> {
        let mut result = NodeResult::success(json!(null));
        result.updates.insert("x".into(), json!(99));
        result.insertions = vec![
            zhuye_workflow::Insertion {
                id: "valid".into(),
                node_type: "echo".into(),
                after: "insert".into(),
                config: json!({}),
            },
            zhuye_workflow::Insertion {
                id: "invalid".into(),
                node_type: "missing".into(),
                after: "insert".into(),
                config: json!({}),
            },
        ];
        Ok(result)
    }
}

#[tokio::test]
async fn invalid_insertion_fails_atomically_without_context_update() {
    let mut flow = Flow::new(json!({"x": 3})).unwrap();
    flow.register("echo", Arc::new(Echo)).unwrap();
    flow.push(NodeSpec::new("insert", Arc::new(InvalidInsert)))
        .unwrap();
    let report = flow.run(Control::default()).await;
    assert_eq!(report.status, Status::Failed);
    assert_eq!(report.variables["x"], 3);
    assert_eq!(report.history.len(), 1);
    assert!(
        report.history[0]
            .error
            .as_deref()
            .unwrap()
            .contains("未注册")
    );
}

struct StubAgent;
#[async_trait]
impl zhuye_workflow::AgentBackend for StubAgent {
    async fn run(
        &self,
        prompt: String,
        run_id: uuid::Uuid,
    ) -> Result<zhuye_workflow::AgentReply, String> {
        assert!(prompt.contains("visible"));
        assert!(!prompt.contains("hidden"));
        assert!(prompt.contains("selected_output"));
        assert!(!prompt.contains("private_output"));
        Ok(zhuye_workflow::AgentReply {
            run_id: run_id.to_string(),
            text: json!({"status":"blocked", "output":{}, "error":"外部依赖"}).to_string(),
        })
    }
}

#[tokio::test]
async fn agent_node_selects_context_and_requires_explicit_business_result() {
    let node = zhuye_workflow::AgentNode {
        backend: Arc::new(StubAgent),
        instructions: "处理任务".into(),
        context_keys: vec!["visible".into()],
        output_keys: vec!["selected_output".into()],
    };
    let input = NodeInput {
        node_id: "agent".into(),
        node_type: "agent".into(),
        attempt_id: uuid::Uuid::new_v4(),
        attempt: 1,
        config: json!({}),
        variables: json!({"visible": 1, "hidden": 2}),
        outputs: serde_json::from_value(json!({"selected_output": 3, "private_output": 4}))
            .unwrap(),
    };
    let expected_id = input.attempt_id.to_string();
    let result = node.execute(input).await.unwrap();
    assert_eq!(result.status, Status::Blocked);
    assert_eq!(result.agent_run_id.as_deref(), Some(expected_id.as_str()));
}

struct Panic;
#[async_trait]
impl Node for Panic {
    async fn execute(&self, _: NodeInput) -> Result<NodeResult, String> {
        panic!("测试节点异常");
    }
}

#[tokio::test]
async fn panic_is_recorded_and_does_not_skip_cleanup() {
    let mut flow = Flow::new(json!({"x": 4})).unwrap();
    flow.push(NodeSpec::new("panic", Arc::new(Panic))).unwrap();
    flow.push_finally(NodeSpec::new("cleanup", Arc::new(Echo)))
        .unwrap();
    let report = flow.run(Control::default()).await;
    assert_eq!(report.status, Status::Failed);
    assert!(
        report.history[0]
            .error
            .as_deref()
            .unwrap()
            .contains("panic")
    );
    assert_eq!(report.finally_history[0].output, json!({"seen": 4}));
}

struct InvalidBack;
#[async_trait]
impl Node for InvalidBack {
    async fn execute(&self, _: NodeInput) -> Result<NodeResult, String> {
        let mut result = NodeResult::success(json!(null));
        result.updates.insert("x".into(), json!(99));
        result.action = zhuye_workflow::Action::Back {
            target: "unknown".into(),
        };
        Ok(result)
    }
}

#[tokio::test]
async fn invalid_backward_target_does_not_apply_updates() {
    let mut flow = Flow::new(json!({"x": 3})).unwrap();
    flow.push(NodeSpec::new("bad", Arc::new(InvalidBack)))
        .unwrap();
    let report = flow.run(Control::default()).await;
    assert_eq!(report.status, Status::Failed);
    assert_eq!(report.variables["x"], 3);
    assert!(
        report.history[0]
            .error
            .as_deref()
            .unwrap()
            .contains("回退目标无效")
    );
}

struct Held {
    started: Arc<tokio::sync::Notify>,
    finish: Arc<tokio::sync::Notify>,
}
#[async_trait]
impl Node for Held {
    async fn execute(&self, _: NodeInput) -> Result<NodeResult, String> {
        self.started.notify_one();
        self.finish.notified().await;
        Ok(NodeResult::success(json!({"done": true})))
    }
}

#[tokio::test]
async fn pause_waits_for_current_node_before_stopping_queue() {
    let started = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    let mut flow = Flow::new(json!({"x": 4})).unwrap();
    flow.push(NodeSpec::new(
        "held",
        Arc::new(Held {
            started: started.clone(),
            finish: finish.clone(),
        }),
    ))
    .unwrap();
    flow.push(NodeSpec::new("next", Arc::new(Echo))).unwrap();
    let control = Control::default();
    let runner = control.clone();
    let task = tokio::spawn(async move { flow.run(runner).await });
    started.notified().await;
    control.pause();
    assert_eq!(control.snapshot().state, zhuye_workflow::RunState::Running);
    finish.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while control.snapshot().state != zhuye_workflow::RunState::Paused {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(control.snapshot().history.len(), 1);
    control.resume();
    let report = task.await.unwrap();
    assert_eq!(report.history.len(), 2);
    assert_eq!(report.history[1].output, json!({"seen": 4}));
}

#[tokio::test]
async fn finally_nodes_obey_skip_conditions_and_record_skips() {
    let mut flow = Flow::new(json!({"x": 3})).unwrap();
    flow.push(NodeSpec::new("main", Arc::new(Echo))).unwrap();
    let mut cleanup = NodeSpec::new("skip_cleanup", Arc::new(Fail));
    cleanup.skip_if = Some(zhuye_workflow::SkipCondition {
        variable: "x".into(),
        equals: json!(3),
    });
    flow.push_finally(cleanup).unwrap();
    flow.push_finally(NodeSpec::new("cleanup", Arc::new(Echo)))
        .unwrap();
    let report = flow.run(Control::default()).await;
    assert_eq!(report.status, Status::Success);
    assert_eq!(report.finally_history[0].status, Status::Skipped);
    assert_eq!(report.finally_history[1].output, json!({"seen":3}));
}

struct AlwaysBack;
#[async_trait]
impl Node for AlwaysBack {
    async fn execute(&self, _: NodeInput) -> Result<NodeResult, String> {
        let mut result = NodeResult::success(json!(null));
        result.action = zhuye_workflow::Action::Back {
            target: "loop".into(),
        };
        Ok(result)
    }
}

#[tokio::test]
async fn retry_limit_ends_the_workflow_with_a_failure_record() {
    let mut flow = Flow::new(json!({})).unwrap();
    let mut node = NodeSpec::new("loop", Arc::new(AlwaysBack));
    node.max_runs = 2;
    flow.push(node).unwrap();
    let report = flow.run(Control::default()).await;
    assert_eq!(report.status, Status::Failed);
    assert_eq!(report.history.len(), 3);
    assert!(
        report.history[2]
            .error
            .as_deref()
            .unwrap()
            .contains("次数已达上限")
    );
}

struct FencedAgent(&'static str);
#[async_trait]
impl zhuye_workflow::AgentBackend for FencedAgent {
    async fn run(
        &self,
        _: String,
        run_id: uuid::Uuid,
    ) -> Result<zhuye_workflow::AgentReply, String> {
        Ok(zhuye_workflow::AgentReply {
            run_id: run_id.to_string(),
            text: self.0.into(),
        })
    }
}

#[tokio::test]
async fn agent只接受完整的json代码围栏() {
    for (reply, valid) in [
        ("```json\n{\"status\":\"success\",\"output\":{}}\n```", true),
        ("{\"status\":\"ok\",\"output\":{\"ready\":true}}", true),
        (
            "说明\n```json\n{\"status\":\"success\",\"output\":{}}\n```",
            false,
        ),
        ("```json\n{\"status\":\"success\",\"output\":{}}", false),
    ] {
        let node = zhuye_workflow::AgentNode {
            backend: Arc::new(FencedAgent(reply)),
            instructions: "处理".into(),
            context_keys: vec![],
            output_keys: vec![],
        };
        let input = NodeInput {
            node_id: "a".into(),
            node_type: "agent".into(),
            attempt_id: uuid::Uuid::new_v4(),
            attempt: 1,
            config: json!({}),
            variables: json!({}),
            outputs: Default::default(),
        };
        let result = node.execute(input).await;
        assert_eq!(result.is_ok(), valid);
        if reply.contains("\"ok\"") {
            assert_eq!(result.unwrap().status, zhuye_workflow::Status::Success);
        }
    }
}
