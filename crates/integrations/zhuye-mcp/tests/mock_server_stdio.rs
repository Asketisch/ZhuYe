//! zhuye-mcp 集成测试 —— mock stdio MCP server 子进程 × 本 crate 的 client 全栈。
//!
//! 覆盖链路:子进程启动 → initialize 握手 → tools/list(echo + slow)
//! → tools/call(echo 原样回显 text)。
//!
//! ## 为什么这个文件必须存在(构建副作用,别删)
//!
//! cargo 在编译某包的**集成测试**时,会把该包的普通 bin 一并构建
//! (`CARGO_BIN_EXE_<name>` 指向它)。`zhuye-exec` 的
//! `tests/mcp_integration.rs` 需要把 `mock_mcp_server` 拉起成真子进程,
//! 而 zhuye-mcp 只是 zhuye-exec 的 **dependency** —— dependency 只构建
//! lib,不构建 bin。因此没有本文件时,干净 checkout 上
//! `cargo test --workspace --all-targets` 不会产出
//! `target/debug/mock_mcp_server`,zhuye-exec 的 MCP 子进程用例会直接
//! 失败("mock server should start")。
//!
//! 换句话说:本文件既是 mock server fixture 自身的回归测试,也是
//! 「让 mock_mcp_server 二进制随 `cargo test` 一起产出」的锚点。

use std::collections::HashMap;
use std::time::Duration;

use zhuye_mcp::{McpConnectionManager, McpServerConfig, McpToolAdapter, McpTransport};
use zhuye_tools::{Tool, ToolContext};

/// 服务端断开时，宿主必须及时观察到连接取消，不能留下永久等待的任务。
#[tokio::test]
async fn transport_reports_server_disconnect() {
    let env = HashMap::from([("MOCK_DISCONNECT_AFTER_INITIALIZED".into(), "1".into())]);
    let (lifecycle_tx, _lifecycle_rx) = tokio::sync::mpsc::channel(8);
    let tool_names =
        std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let handle = zhuye_mcp::transport::build_stdio(
        "mock",
        Some(env!("CARGO_BIN_EXE_mock_mcp_server")),
        &[],
        &env,
        lifecycle_tx,
        tool_names,
    )
    .await
    .expect("握手应成功");
    tokio::time::timeout(Duration::from_secs(5), handle.cancel.cancelled())
        .await
        .expect("服务端关闭后连接必须取消");
}

/// 握手和初始化工具列表不得无限等待无响应的本地服务。
#[tokio::test]
async fn startup_times_out_for_unresponsive_server() {
    for method in ["initialize", "tools/list"] {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let mut config = mock_server_config();
        config.timeout = Duration::from_millis(750);
        config
            .env
            .insert("MOCK_PENDING_METHOD".into(), method.into());
        config
            .env
            .insert("MOCK_PID_MARKER".into(), pid_file.display().to_string());
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let manager = McpConnectionManager::new(tx);
        let result = tokio::time::timeout(Duration::from_secs(5), manager.start_server(config))
            .await
            .expect("服务启动未按配置超时");
        assert!(result.is_err(), "无响应服务不能启动成功: {method}");
        match rx.recv().await.unwrap() {
            zhuye_mcp::McpLifecycleEvent::Failed {
                server,
                error,
                will_retry,
            } => {
                assert_eq!(server, "mock");
                assert!(error.contains("timeout"), "{method}: {error}");
                assert!(!will_retry);
            }
            _ => panic!("应报告启动失败"),
        }
        assert!(manager.server_names().await.is_empty());
        #[cfg(unix)]
        {
            let pid = std::fs::read_to_string(pid_file).expect("服务进程未启动");
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let alive = std::process::Command::new("/bin/kill")
                        .args(["-0", pid.as_str()])
                        .stderr(std::process::Stdio::null())
                        .status()
                        .unwrap()
                        .success();
                    if !alive {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("初始化超时后服务子进程未退出");
        }
    }
}

/// HTTP 启动失败不会自动重试，事件不能错误承诺重连。
#[tokio::test]
async fn http_start_failure_does_not_claim_retry() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut config = mock_server_config();
    config.transport = McpTransport::Http;
    config.command = None;
    config.url = Some(format!("http://{}/mcp", listener.local_addr().unwrap()));
    config.timeout = Duration::from_millis(250);
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let manager = McpConnectionManager::new(tx);
    assert!(manager.start_server(config).await.is_err());
    match rx.recv().await.unwrap() {
        zhuye_mcp::McpLifecycleEvent::Failed {
            server, will_retry, ..
        } => {
            assert_eq!(server, "mock");
            assert!(!will_retry);
        }
        _ => panic!("应报告启动失败"),
    }
}

/// 资源请求在服务端无响应时仍可取消，并向服务端发送对应请求的取消通知。
#[tokio::test]
async fn cancelling_resource_read_notifies_server() {
    let dir = tempfile::tempdir().unwrap();
    let pending = dir.path().join("pending");
    let cancelled = dir.path().join("cancelled");
    let mut config = mock_server_config();
    config.env.insert(
        "MOCK_RESOURCE_PENDING_MARKER".into(),
        pending.display().to_string(),
    );
    config
        .env
        .insert("MOCK_CANCEL_MARKER".into(), cancelled.display().to_string());
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let manager = std::sync::Arc::new(McpConnectionManager::new(tx));
    manager.start_server(config).await.unwrap();
    let tool = zhuye_mcp::ReadMcpResourceTool::new(manager.clone());
    let ctx = ToolContext::default();
    let cancel = ctx.cancel.clone();
    let task = tokio::spawn(async move {
        tool.execute(
            ctx,
            serde_json::json!({"server":"mock", "uri":"file:///slow"}),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !pending.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("服务端未收到资源读取");
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("取消未及时返回")
        .unwrap();
    assert!(
        matches!(result, Err(zhuye_tools::ToolError::Cancelled)),
        "{result:?}"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !cancelled.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("服务端未收到取消通知");
    let notification: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cancelled).unwrap()).unwrap();
    let request_id: serde_json::Value =
        serde_json::from_slice(&std::fs::read(pending).unwrap()).unwrap();
    assert_eq!(notification["params"]["requestId"], request_id);
    manager.shutdown().await;
}

/// server→client elicitation 全链路:服务端请求转成宿主事件,宿主回执驱动工具结果。
#[tokio::test]
async fn elicitation_reaches_host_and_drives_tool_result() {
    let mut config = mock_server_config();
    config.env.insert(
        "MOCK_ELICITATION".to_string(),
        serde_json::json!({
            "mode": "form",
            "message": "请补充",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "answer": { "type": "string", "title": "回答", "description": "测试输入" },
                    "colour": { "type": "string", "enum": ["red", "blue"] },
                    "auto": { "type": "boolean" },
                },
                "required": ["answer"],
            },
        })
        .to_string(),
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let manager = McpConnectionManager::new(tx);
    let handle = manager
        .start_server(config)
        .await
        .expect("mock server 应启动");
    let desc = handle
        .tools
        .iter()
        .find(|d| d.original_name == "echo")
        .expect("echo 描述符");
    let adapter = McpToolAdapter::from_descriptor(
        handle.inner.clone(),
        desc,
        "mock",
        Duration::from_secs(10),
        None,
    );
    let call = tokio::spawn(async move {
        adapter
            .execute(
                ToolContext::for_workspace(std::env::temp_dir()),
                serde_json::json!({"text": "hi"}),
            )
            .await
    });

    // 握手事件先到,之后是 elicitation 请求。
    let (event, respond) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let zhuye_mcp::McpLifecycleEvent::Elicitation { event, respond } =
                rx.recv().await.expect("生命周期通道不应关闭")
            {
                return (event, respond);
            }
        }
    })
    .await
    .expect("服务端应发起 elicitation 请求");

    assert_eq!(event.server, "mock");
    assert_eq!(event.message, "请补充");
    let zhuye_protocol::McpElicitationKind::Form { fields } = event.kind else {
        panic!("form 形态应映射为 Form: {:?}", event.kind)
    };
    // 字段顺序沿用 rmcp 交付的 schema 键序(JSON 对象可能被中间层重排),
    // 因此按名字取字段断言,不锁顺序。
    let field = |name: &str| {
        fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("缺少字段 {name}: {fields:?}"))
    };
    let mut names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["answer", "auto", "colour"]);
    let answer = field("answer");
    assert!(answer.required, "required 应落到字段上");
    assert_eq!(answer.title.as_deref(), Some("回答"));
    assert_eq!(answer.description.as_deref(), Some("测试输入"));
    assert_eq!(
        answer.kind,
        zhuye_protocol::McpElicitationFieldKind::Text { secret: false }
    );
    assert_eq!(
        field("colour").kind,
        zhuye_protocol::McpElicitationFieldKind::Enum {
            options: vec!["red".into(), "blue".into()],
            multi: false,
        }
    );
    assert!(!field("colour").required);
    assert_eq!(
        field("auto").kind,
        zhuye_protocol::McpElicitationFieldKind::Boolean
    );

    respond
        .send(zhuye_protocol::McpElicitationResponse {
            id: event.request_id.clone(),
            action: "accept".into(),
            content: Some(serde_json::json!({"answer": "42"})),
        })
        .expect("服务端请求应仍在等待");

    let out = tokio::time::timeout(Duration::from_secs(5), call)
        .await
        .expect("tools/call 应随回执结束")
        .expect("任务不应 panic")
        .expect("echo 调用应成功");
    let text = match &out.content[0] {
        zhuye_protocol::ContentBlock::Text { text } => text.clone(),
        other => panic!("应为文本: {other:?}"),
    };
    assert!(text.starts_with("hi|elicit:"), "{text}");
    assert!(text.contains(r#""action":"accept""#), "{text}");
    assert!(text.contains(r#""answer":"42""#), "{text}");
    manager.shutdown().await;
}

/// URL 形态 + 宿主 decline:回执按协议原样上送,不伪造 content。
#[tokio::test]
async fn elicitation_url_mode_decline_passes_through() {
    let mut config = mock_server_config();
    config.env.insert(
        "MOCK_ELICITATION".to_string(),
        serde_json::json!({
            "mode": "url",
            "message": "请到链接授权",
            "url": "https://example.com/consent",
            "elicitationId": "e-1",
        })
        .to_string(),
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let manager = McpConnectionManager::new(tx);
    let handle = manager.start_server(config).await.expect("启动");
    let desc = handle
        .tools
        .iter()
        .find(|d| d.original_name == "echo")
        .expect("echo 描述符");
    let adapter = McpToolAdapter::from_descriptor(
        handle.inner.clone(),
        desc,
        "mock",
        Duration::from_secs(10),
        None,
    );
    let call = tokio::spawn(async move {
        adapter
            .execute(
                ToolContext::for_workspace(std::env::temp_dir()),
                serde_json::json!({"text": "u"}),
            )
            .await
    });
    let (event, respond) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let zhuye_mcp::McpLifecycleEvent::Elicitation { event, respond } =
                rx.recv().await.expect("通道不应关闭")
            {
                return (event, respond);
            }
        }
    })
    .await
    .expect("应有 elicitation 请求");
    assert_eq!(
        event.kind,
        zhuye_protocol::McpElicitationKind::Link {
            url: "https://example.com/consent".into()
        }
    );
    respond
        .send(zhuye_protocol::McpElicitationResponse {
            id: event.request_id.clone(),
            action: "decline".into(),
            content: None,
        })
        .expect("请求应待决");
    let out = tokio::time::timeout(Duration::from_secs(5), call)
        .await
        .expect("应结束")
        .expect("任务不应 panic")
        .expect("调用应成功");
    let text = match &out.content[0] {
        zhuye_protocol::ContentBlock::Text { text } => text.clone(),
        other => panic!("应为文本: {other:?}"),
    };
    assert!(text.contains(r#""action":"decline""#), "{text}");
    assert!(
        !text.contains("content"),
        "decline 不应携带 content: {text}"
    );
    manager.shutdown().await;
}

/// mock stdio server 配置:命令 = 本包的 mock_mcp_server bin。
fn mock_server_config() -> McpServerConfig {
    McpServerConfig {
        name: "mock".to_string(),
        transport: McpTransport::Stdio,
        command: Some(env!("CARGO_BIN_EXE_mock_mcp_server").to_string()),
        args: vec![],
        env: HashMap::new(),
        url: None,
        headers: HashMap::new(),
        timeout: Duration::from_secs(10),
        oauth: None,
    }
}

/// 真子进程:握手成功 + 列出 echo/slow 两个工具 + echo 回显。
#[tokio::test]
async fn mock_server_lists_tools_and_echo_roundtrips() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let manager = McpConnectionManager::new(tx);
    let handle = manager
        .start_server(mock_server_config())
        .await
        .expect("mock server 应该启动成功");

    // 1) 工具列表:mock server 暴露 echo + slow。
    let names: Vec<&str> = handle.tools.iter().map(|d| d.full_name.as_str()).collect();
    assert_eq!(handle.tools.len(), 2, "expected echo + slow, got {names:?}");
    assert!(names.contains(&"mcp__mock__echo"), "got {names:?}");
    assert!(names.contains(&"mcp__mock__slow"), "got {names:?}");

    // 2) 生命周期事件:Started。
    let evt = rx.try_recv().expect("Started event");
    match evt {
        zhuye_mcp::McpLifecycleEvent::Started { server, tools, .. } => {
            assert_eq!(server, "mock");
            assert_eq!(tools, 2);
        }
        other => panic!("expected Started, got {other:?}"),
    }

    // 3) tools/call:echo 原样回显 text。
    let desc = handle
        .tools
        .iter()
        .find(|d| d.original_name == "echo")
        .expect("echo 描述符");
    let adapter = McpToolAdapter::from_descriptor(
        handle.inner.clone(),
        desc,
        "mock",
        Duration::from_secs(10),
        None,
    );
    let out = adapter
        .execute(
            ToolContext::for_workspace(std::env::temp_dir()),
            serde_json::json!({"text": "ping"}),
        )
        .await
        .expect("echo 调用应成功");
    assert!(!out.is_error, "echo 不应报错: {out:?}");
    let text = match &out.content[0] {
        zhuye_protocol::ContentBlock::Text { text } => text.clone(),
        other => panic!("expected text, got {other:?}"),
    };
    assert!(text.contains("ping"), "echo 应回显 text: {text:?}");

    manager.shutdown().await;
}

/// 服务端通知工具清单变化后,宿主必须拿到**新清单**并知道哪些工具消失了。
#[tokio::test]
async fn tool_list_change_refreshes_snapshot() {
    let mut config = mock_server_config();
    config
        .env
        .insert("MOCK_LIST_CHANGED".to_string(), "1".to_string());
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let manager = McpConnectionManager::new(tx);
    let handle = manager
        .start_server(config)
        .await
        .expect("mock server 应启动");
    // 初始清单:echo + slow。
    let initial: Vec<String> = handle
        .tools
        .iter()
        .map(|descriptor| descriptor.full_name.clone())
        .collect();
    assert!(
        initial.contains(&"mcp__mock__echo".to_string()),
        "{initial:?}"
    );
    assert!(
        initial.contains(&"mcp__mock__slow".to_string()),
        "{initial:?}"
    );

    // 通知到达后:新清单 echo + extra,消失的是 slow。
    let changed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await.expect("生命周期通道不应关闭") {
                zhuye_mcp::McpLifecycleEvent::ToolsChanged { server, tools } => {
                    return (server, tools);
                }
                _ => continue,
            }
        }
    })
    .await
    .expect("应收到工具清单变化事件");
    let (server, tools) = changed;
    assert_eq!(server, "mock");
    let names: Vec<&str> = tools
        .iter()
        .map(|descriptor| descriptor.full_name.as_str())
        .collect();
    assert!(names.contains(&"mcp__mock__extra"), "{names:?}");
    assert!(!names.contains(&"mcp__mock__slow"), "{names:?}");
    // 快照应与事件一致(调用方按它注册)。
    let snapshot = manager
        .server_tool_snapshot("mock")
        .await
        .expect("快照应存在");
    let snapshot_names: Vec<&str> = snapshot
        .iter()
        .map(|descriptor| descriptor.full_name.as_str())
        .collect();
    assert!(
        snapshot_names.contains(&"mcp__mock__extra"),
        "{snapshot_names:?}"
    );
    assert!(
        !snapshot_names.contains(&"mcp__mock__slow"),
        "{snapshot_names:?}"
    );
    manager.shutdown().await;
}
