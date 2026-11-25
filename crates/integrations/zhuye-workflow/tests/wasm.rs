//! 使用实际构建的组件验证工作流宿主契约。
use serde_json::json;
use std::{path::Path, time::Duration};
use uuid::Uuid;
use zhuye_workflow::{Node, NodeInput, Status, WasmTaskNode};

fn component() -> WasmTaskNode {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../plugins/plugin-demo/bin/zhuye-workflow-echo.wasm");
    let component = zhuye_wasm::WasmWorkflowNode::from_file(&path).unwrap();
    assert_eq!(component.name, "workflow_echo");
    WasmTaskNode(component.with_deadline(Duration::from_millis(100)))
}

fn input(mode: &str) -> NodeInput {
    NodeInput {
        node_id: "sample".into(),
        node_type: "wasm".into(),
        attempt_id: Uuid::new_v4(),
        attempt: 1,
        config: json!({"mode": mode}),
        variables: json!({"message": "你好"}),
        outputs: Default::default(),
    }
}

#[tokio::test]
async fn component_roundtrip_and_failure_paths() {
    let node = component();
    let result = node.execute(input("ok")).await.unwrap();
    assert_eq!(result.status, Status::Success);
    assert_eq!(result.output, json!({"echo": "你好"}));
    assert_eq!(result.updates["echoed"], "你好");
    assert!(
        node.execute(input("invalid"))
            .await
            .unwrap_err()
            .contains("节点结果非法")
    );
    assert!(
        node.execute(input("error"))
            .await
            .unwrap_err()
            .contains("示例组件拒绝请求")
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(3), node.execute(input("loop")))
            .await
            .unwrap()
            .is_err()
    );
}
