# ZhuYe

ZhuYe is a runtime engine for AI agents. Embed it in your own product and the agent runs inside that product: reading code, editing files, calling tools, calling models, spawning subagents.

It is not a coding assistant you sit in front of — for that, Claude Code or Codex are enough. ZhuYe is the chassis underneath an agent: those are finished houses, ZhuYe is the framing kit.

中文：[README.md](README.md).

## Getting started

The engine needs Rust 1.89, as required by the security-updated rmcp and rusqlite dependency set. Build the binary first:

```bash
cargo build -p zhuye-cli
ZHUYE_MODEL=mock ./target/debug/zhuye exec "hello"
```

`ZHUYE_MODEL=mock` is an explicit offline model. It will not silently swap you onto it when no API key is configured. For a real model, set `OPENAI_API_KEY` or `ANTHROPIC_API_KEY`. Events go to stdout, logs go to stderr.

Link it in-process (also keyless):

```rust
use zhuye::{Submission, Zhuye};

let agent = Zhuye::builder("mock").workspace(".").build()?;
let mut stream = agent.submit(Submission::user_input("hello")).await;
while let Some(event) = stream.next().await {
    let _ = event;
}
```

Tools written in your language stay in your language. The SDK starts `zhuye serve` (`ZHUYE_BIN` picks the binary), and the tools you register only run on the host side:

```python
from zhuye import ZhuyeAgent, ToolOutput, ContentBlock

agent = ZhuyeAgent.spawn()
agent.register_tool(
    "get_weather",
    "查询城市天气",
    {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
    lambda args: ToolOutput(
        content=[ContentBlock(type="text", text=f"{args['city']} 晴")],
        is_error=False,
    ),
)
for ev in agent.submit("北京天气如何?"):
    if ev["msg"]["type"] == "turn_complete":
        break
agent.close()
```

`pip install zhuye` for the Python package (standard library only), `npm install zhuye` for the TypeScript one. See [docs/sdk.md](docs/sdk.md) for the details.

## What it can do

- **Embed it in your own product** — the engine is a library, not a product: link it into your process, run a single turn and exit, or keep a session service alive. One engine, three ways in.
- **Keep your tools in your own process** — register a Python / TypeScript function by name and schema; no rewrite in Rust needed.
- **Multi-agent is a primitive** — subagents, role-based discussions, DAG pipelines, tasks and teams ship with the engine.
- **Risky actions have gates** — permission modes + hooks + OS sandbox (Seatbelt on macOS, Landlock on Linux).
- **The context tidies itself up** — local trimming → summarizer rewrite → active files re-read.
- **Switching models does not mean rewriting prompts** — prompts are stacked as "default → vendor → exact model", plus a provider fallback chain.
- **Every session leaves a record** — JSONL the whole way: look back, resume, branch, search across sessions.
- **Levers outside the session** — cross-session memory, cron jobs, side-channels, an audit trail per turn.
- **git is a first-class citizen** — checkpoint / rewind via tagged snapshots; plugins add branch / PR probes and per-turn auto snapshots.

How a turn flows through the engine: [docs/how-it-works.md](docs/how-it-works.md). The full capability tour and known limits: [docs/features.md](docs/features.md). The docs themselves are written in Chinese.

## Documentation

| Doc | Contents |
|-----|----------|
| [docs/how-it-works.md](docs/how-it-works.md) | What happens in a turn |
| [docs/features.md](docs/features.md) | Capability tour and limits |
| [docs/cli.md](docs/cli.md) | CLI reference: subcommands, zhuye-import |
| [docs/configuration.md](docs/configuration.md) | Config file and environment variables |
| [docs/architecture.md](docs/architecture.md) | How the engine is put together: layers, data flow, facade mapping |
| [docs/sdk.md](docs/sdk.md) | SDK integration: session service, remote tools, offline testing |
| [sdks/PROTOCOL.md](sdks/PROTOCOL.md) | The wire protocol between client and server |
| [docs/tools-and-hooks.md](docs/tools-and-hooks.md) | Built-in tools, hook events and decisions, permission modes, sandbox |
| [docs/prompts.md](docs/prompts.md) | The prompt library and its layered overrides |
| [docs/plugin-dev-guide.md](docs/plugin-dev-guide.md) | Plugin development guide (community-facing: from zero to published) |
| [docs/plugins.md](docs/plugins.md) | Plugin reference (manifest fields, security model, API index; incl. WASM component plugins) |
| [wit/README.md](wit/README.md) | WASM plugin contract (`zhuye:plugin` WIT worlds, versioning, language matrix) |
| [docs/plugin-marketplace.md](docs/plugin-marketplace.md) | Plugin marketplace: manifest and publishing |
| [docs/PLAN_MODE.md](docs/PLAN_MODE.md) | Read-only planning mode |
| [docs/sanitize.md](docs/sanitize.md) | Secret redaction |
| [docs/release.md](docs/release.md) | SDK releases (PyPI / npm) |
| [docs/development.md](docs/development.md) | Development guide: build, test, gates |
| [AGENTS.md](AGENTS.md) | Agent collaboration conventions and dev checklist |

## License

The engine and CLI are [Apache-2.0](LICENSE). The Python and TypeScript SDKs declare MIT in their own package manifests. Repository: <https://github.com/Asketisch/ZhuYe>.
