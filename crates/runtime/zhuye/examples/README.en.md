# ZhuYe Examples

<p align="center">  <a href="README.md">中文</a> · <b>English</b>  </p>

The `zhuye` crate provides 6 runnable examples demonstrating the
public library facade. All examples are based on the
`Zhuye::builder()` + event-stream API.

| Example | Description | Run |
|---------|---------------|-----|
| `headless_run.rs`     | minimal working config: one prompt run until `TurnComplete`. | `cargo run -p zhuye --example headless_run -- "what is 2+2?"` |
| `custom_tool.rs`      | implements the `Tool` trait (`EchoTool`) and registers it into the thread's `ToolRegistry`. | `cargo run -p zhuye --example custom_tool` |
| `multi_turn.rs`       | 3 sequential turns on the same agent; history is kept across turns. | `cargo run -p zhuye --example multi_turn` |
| `custom_provider.rs`  | registers a third `ModelClient` (`MockLlmClient`) so the agent runs offline. | `cargo run -p zhuye --example custom_provider` |
| `hook_listener.rs`    | registers two hooks: `TokenUsageHook` (counter) and `DangerousCommandHook` (denies `rm -rf /`). | `cargo run -p zhuye --example hook_listener` |
| `discussion_demo.rs`  | a 3-agent discussion, run offline through a stub `ModelClient` with the full wiring. | `cargo run -p zhuye --example discussion_demo` |

## Environment Variables

- `OPENAI_API_KEY` — enables the OpenAI provider.
- `ANTHROPIC_API_KEY` — enables the Anthropic provider.
- `ZHUYE_MODEL` — overrides the default model spec (`openai/gpt-4o`).

Most examples set `ZHUYE_MODEL=mock` (or default to it) to run offline
without an API key. `headless_run` requires a real key.

## Common Pattern

Every example follows the same shape:

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

The full facade mapping lives in `docs/architecture.md`.
