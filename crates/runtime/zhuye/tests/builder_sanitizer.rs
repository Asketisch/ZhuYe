//! 验证同步与异步门面均使用用户配置的工具输出脱敏规则。

use zhuye::Zhuye;
use zhuye_protocol::{ContentBlock, EventMsg, Op, Submission, UserInputItem};

#[tokio::test]
async fn both_builders_apply_configured_sanitizer_to_tool_output() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        "[hooks]\nenabled = []\n[sanitize]\nmarker = '<MASKED>'\nextra_patterns = ['CANDLESECRET123']\n",
    ).unwrap();
    let script = dir.path().join("mock.jsonl");
    std::fs::write(
        &script,
        concat!(
            "{\"type\":\"tool_call\",\"name\":\"echo\",\"args\":{\"text\":\"CANDLESECRET123\"}}\n",
            "{\"type\":\"text\",\"text\":\"done\"}\n"
        ),
    )
    .unwrap();
    // 该集成测试二进制只有本用例读取这些环境变量。
    unsafe {
        std::env::set_var("ZHUYE_HOME", &home);
        std::env::set_var("ZHUYE_MODEL", "mock");
        std::env::set_var("ZHUYE_PROVIDER", "mock");
        std::env::set_var("ZHUYE_MOCK_SCRIPT", &script);
    }
    let sync = Zhuye::builder("mock")
        .workspace(dir.path())
        .without_rollout()
        .build()
        .unwrap();
    let asynchronous = Zhuye::builder("mock")
        .workspace(dir.path())
        .without_rollout()
        .build_async()
        .await
        .unwrap();
    for agent in [&sync, &asynchronous] {
        let sub = Submission {
            id: "sanitizer-probe".into(),
            op: Op::UserInput {
                items: vec![UserInputItem::Text {
                    text: "运行工具".into(),
                }],
                thread_settings: Default::default(),
            },
            client_user_message_id: None,
            trace: None,
            workspace: None,
            source_command: None,
        };
        let mut events = agent.submit(sub).await;
        let mut output = None;
        while let Some(event) =
            tokio::time::timeout(std::time::Duration::from_secs(20), events.next())
                .await
                .unwrap()
        {
            if let EventMsg::ToolCallEnd(end) = event.msg {
                assert!(!end.is_error);
                output = end
                    .output
                    .content
                    .into_iter()
                    .find_map(|block| match block {
                        ContentBlock::Text { text } => Some(text),
                        _ => None,
                    });
            }
        }
        assert_eq!(output.as_deref(), Some("<MASKED>"));
    }
}
