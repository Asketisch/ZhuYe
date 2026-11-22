<p align="center">
  <a href="README.md">中文</a> · <b>English</b>
</p>

# ZhuYe

ZhuYe is an AI agent runtime framework written in Rust. You embed it into your own product, and the agent runs inside your product: reading code, editing files, calling tools, calling models, spawning sub-agents.

It is not a coding assistant for the terminal — for that kind of product, off-the-shelf options are plenty. ZhuYe is the chassis for building agents: a finished product is the house, and ZhuYe is the framework it was built with.

## Quick Start

The engine requires Rust 1.89 (edition 2024). First build the binary:

```bash
cargo build -p zhuye-cli
ZHUYE_MODEL=mock ./target/debug/zhuye exec "hello"
```

`ZHUYE_MODEL=mock` is an explicit offline model — it runs without an API key, and will not be silently swapped for another provider. To use a real model, set `OPENAI_API_KEY` or `ANTHROPIC_API_KEY`. Events flow as JSONL on stdout, tracing logs go to stderr (`zhuye exec | jq` stays free of log noise).

## Three Ways to Use It

**In-process linking (Rust)** — the engine is a library, link it directly into your process:

```rust
use zhuye::{Submission, Zhuye};

let agent = Zhuye::builder("mock").workspace(".").build()?;
let mut stream = agent.submit(Submission::user_input("hello")).await;
while let Some(event) = stream.next().await {
    let _ = event;
}
```

**Headless single turn** — run one turn and exit; the JSONL event stream is ideal for scripts and CI:

```bash
ZHUYE_MODEL=mock ./target/debug/zhuye exec "refactor this module"
```

**Long-running session (Python / TypeScript SDK)** — `zhuye serve` provides a stdio JSONL session service, and the official SDKs wrap it; a function in the host language becomes callable by the model just by registering a name, no rewriting to Rust (inference lives in the Rust engine, business logic stays in your process):

```python
from zhuye import ZhuyeAgent, ToolOutput, ContentBlock

agent = ZhuyeAgent.spawn()
agent.register_tool(
    "get_weather", "查询城市天气",
    {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
    lambda args: ToolOutput(content=[ContentBlock(type="text", text=f"{args['city']} 晴")]),
)
for ev in agent.submit("北京天气如何?"):
    if ev["msg"]["type"] == "turn_complete":
        break
agent.close()
```

`pip install zhuye` installs the Python package (pure standard library), `npm install zhuye` installs the TypeScript package (isomorphic API). Integration details: [docs/sdk.md](docs/sdk.md).

## Core Features

- **Embedded in a product, not a product itself** — library facade, headless CLI, long-running serve: three usage modes, one engine.
- **Host-language tools** — Python / TS functions become LLM-callable tools on registration, executed back through the serve protocol.
- **Multi-agent as a primitive** — sub-agents, role discussions, tasks/teams, DAG pipelines, goal mode, each in its own crate.
- **Gates on dangerous operations** — permission modes + hooks (12 event types / 7 decision kinds) + OS-level sandbox (macOS Seatbelt / Linux Landlock).
- **Frozen protocol, evolvable SDKs** — Protocol v0 extends additively; both SDKs degrade gracefully on unknown variants instead of crashing.
- **Sessions archived and resumable** — JSONL rollout; `--resume` continues by ID / most recent session / ordinal.

## Documentation

| Document | Contents |
|------|------|
| [docs/architecture.md](docs/architecture.md) | Architecture: layering, protocol, state transitions, design decisions |
| [docs/features.md](docs/features.md) | Feature list and boundaries |
| [docs/cli.md](docs/cli.md) | CLI reference: subcommands |
| [docs/sdk.md](docs/sdk.md) | SDK and usage guide (Python / TS) |
| [docs/plugins.md](docs/plugins.md) | Plugin usage (manifest, security model, WASM) |
| [docs/plugin-dev-guide.md](docs/plugin-dev-guide.md) | Plugin development guide |
| [docs/configuration.md](docs/configuration.md) | Configuration & environment variables |
| [docs/development.md](docs/development.md) | Development and release process |
| [SECURITY.md](SECURITY.md) | Security policy |

## License

The engine and CLI are [Apache-2.0](LICENSE); the Python SDK and the TypeScript SDK each carry MIT in their respective package manifests. Repository: <https://github.com/Asketisch/ZhuYe>.
