# ZhuYe Examples

<p align="center">  <b>中文</b> · <a href="README.en.md">English</a>  </p>

`zhuye` crate 提供 6 个可运行示例,演示公共库门面。所有示例都基于
`Zhuye::builder()` + 事件流 API。

| 示例 | 说明 | 运行 |
|---------|---------------|-----|
| `headless_run.rs`     | 最小可用配置:一条 prompt 跑到 `TurnComplete`。 | `cargo run -p zhuye --example headless_run -- "what is 2+2?"` |
| `custom_tool.rs`      | 实现 `Tool` trait(`EchoTool`)并注册到 thread 的 `ToolRegistry`。 | `cargo run -p zhuye --example custom_tool` |
| `multi_turn.rs`       | 同一 agent 上的 3 条顺序 turn;历史在 turn 间保持。 | `cargo run -p zhuye --example multi_turn` |
| `custom_provider.rs`  | 注册第三个 `ModelClient`(`MockLlmClient`)让 agent 离线运行。 | `cargo run -p zhuye --example custom_provider` |
| `hook_listener.rs`    | 注册两个 hook:`TokenUsageHook`(计数器)与 `DangerousCommandHook`(拒绝 `rm -rf /`)。 | `cargo run -p zhuye --example hook_listener` |
| `discussion_demo.rs`  | 3-agent 讨论,用 stub `ModelClient` 离线跑通完整 wiring。 | `cargo run -p zhuye --example discussion_demo` |

## 环境变量

- `OPENAI_API_KEY` —— 启用 OpenAI provider。
- `ANTHROPIC_API_KEY` —— 启用 Anthropic provider。
- `ZHUYE_MODEL` —— 覆盖默认模型规格(`openai/gpt-4o`)。

多数示例设置 `ZHUYE_MODEL=mock`(或默认就是它)以在无 API key 时离线运行。
`headless_run` 需要真实 key。

## 通用模式

每个示例都遵循同样的形状:

```rust
use zhuye::{Submission, Zhuye};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let agent = Zhuye::builder("openai/gpt-4o").build()?;
    let mut stream = agent.submit(Submission::user_input("...")).await;
    while let Some(event) = stream.next().await {
        match event.msg {
            zhuye::EventMsg::AgentMessageDelta(d) => print!("{}", d.delta),
            zhuye::EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    Ok(())
}
```

门面完整映射见 `docs/architecture.md`。
