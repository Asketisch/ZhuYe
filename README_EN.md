# ZhuYe

ZhuYe is a runtime engine for AI agents. Embed it in your own product and the agent runs inside that product: reading code, editing files, calling tools, calling models, spawning subagents.

It is not a coding assistant you sit in front of — for that, Claude Code or Codex are enough. ZhuYe is the chassis underneath an agent: those are finished houses, ZhuYe is the framing kit.

中文：[README.md](README.md).

## What happens in a turn

You hand the engine a sentence. It sends that, plus the current tool list, to the model. The model either replies or asks for a tool to run. The engine runs it, feeds the result back, and the loop continues. When the model stops asking for tools, the turn is over.

There is no magic in the middle. The whole loop is `PreLoop → ModelCall → ToolExec → PreLoop → CheckStop`, and every step emits an event, so your interface just renders the events.

## What you can do with it

**Embed it in your own product.** One Rust engine, three ways to reach it: link it into your process, run a single turn and exit, or keep a session service alive for clients to connect to. It is a library, not a product.

**Keep your tools in your own process.** Functions you write in Python or TypeScript do not have to be rewritten in Rust. Register a name and a parameter schema; when the model wants that tool, the engine sends the request back to your process, the function runs there, and the result comes back as a receipt.

**Multi-agent is a primitive, not a plugin.** Subagents, role-based discussions, declarative task pipelines, tasks and teams all ship with the engine. You do not install an extension first.

**Switching models does not mean rewriting prompts.** Every model has its own temperament, so the built-in prompts are stacked as "default → vendor → exact model" and the right ones get picked for you. You can also set up a chain of candidates, so when one provider is down the engine moves to the next.

**The context tidies itself up.** As the conversation approaches the limit, it trims locally first and only calls a summarizer when that is not enough; afterwards it re-reads the files that are still active.

**Risky actions have gates.** Permission modes decide what asks you, what runs straight away, and what is refused outright. Hooks can rewrite arguments, block calls, or speak up before and after a tool runs. `bash` goes through an OS sandbox by default (Seatbelt on macOS, Landlock on Linux), and if the sandbox cannot start, the command fails rather than quietly falling back to a raw shell.

**Every session leaves a record.** The whole run is written as JSONL. Look back at it, pick it up where it stopped, branch a new line from it, or search across sessions.

## Getting started

The engine needs Rust 1.85. Build the binary first:

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

`build()` installs compaction, memory, skills, and session recording.

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

## Command line

`zhuye` has 19 subcommands. The ones you will reach for:

| Command | What it does |
|---------|--------------|
| `exec` | One turn and out; events stream to stdout as JSONL |
| `serve` | The resident session service the SDK talks to |
| `config` | Read / edit `~/.zhuye/config.toml`, hot-reloaded on write |
| `session` | List sessions, show them, delete, fork, search across them |
| `plugin` / `mcp` / `lsp` | Install extensions, connect MCP servers, connect language servers |
| `models` | Show what the current provider offers; the list is fetched live |
| `discussion` / `pipeline` / `task` | Run a multi-agent discussion, a DAG pipeline, tasks and teams |

For the full list, see `zhuye --help`; for flags, `zhuye <command> --help`.

There is also a standalone binary, `zhuye-import`, which imports history from Claude Code / Codex / ZCode as ZhuYe sessions. After the import you can pick the session back up with `zhuye exec --resume`.

## Configuration

Configuration lives in `~/.zhuye/config.toml`; `ZHUYE_HOME` moves the whole directory elsewhere:

```toml
[active]
provider = "openai"

[openai]
api_key = ""                                  # leave empty for keyless endpoints
base_url = "https://api.kilo.ai/api/gateway"
model = "free"                                # free model: picked from the live catalog at runtime
temperature = 0.3
max_tokens = 4096
```

No restart is needed after an edit — a watcher reloads it. The full set of options lives in [`zhuye-config`](crates/resources/zhuye-config/src/schema/mod.rs).

## Environment variables

| Variable | What it does |
|----------|--------------|
| `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` | Provider credentials; one is enough to start |
| `ZHUYE_MODEL` | Override the model spec; `=mock` runs offline |
| `ZHUYE_HOME` | Data directory, default `~/.zhuye` |
| `ZHUYE_BIN` | Where the SDK looks for the `zhuye` binary |
| `RUST_LOG` | Log filter, default `warn,zhuye=info` |

The rest are scattered across the crates; grep for the `ZHUYE_` prefix in the source.

## Documentation

| Doc | Contents |
|-----|----------|
| [docs/architecture.md](docs/architecture.md) | How the engine is put together: layers, data flow, facade mapping |
| [docs/sdk.md](docs/sdk.md) | SDK integration: session service, remote tools, offline testing |
| [sdks/PROTOCOL.md](sdks/PROTOCOL.md) | The wire protocol between client and server |
| [docs/tools-and-hooks.md](docs/tools-and-hooks.md) | Built-in tools, hook events and decisions, permission modes, sandbox |
| [docs/prompts.md](docs/prompts.md) | The prompt library and its layered overrides |
| [docs/plugins.md](docs/plugins.md) | Developing and using plugins |
| [docs/plugin-marketplace.md](docs/plugin-marketplace.md) | Plugin marketplace: manifest and publishing |
| [docs/PLAN_MODE.md](docs/PLAN_MODE.md) | Read-only planning mode |
| [docs/sanitize.md](docs/sanitize.md) | Secret redaction |
| [docs/release.md](docs/release.md) | SDK releases (PyPI / npm) |
| [AGENTS.md](AGENTS.md) | Development guide and project conventions |

## Development

```bash
make check                                            # cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./tests/run_all.sh functional                         # capability coverage layer
./scripts/e2e.sh                                      # end-to-end smoke
```

Examples and e2e run on the offline `ZHUYE_MODEL=mock` provider, so no API key is needed.

## Limits

- The binary in this repository has no TUI subcommand. The interactive interface lives in the [Reflect-TUI](https://cnb.cool/Demon1019/Reflect-CLI) repository.
- The sandbox denies network by default on macOS. On Linux it restricts writes; it does not cut the network.
- A session can have at most 16 subagents in flight at once. A finished child frees its slot.

## License

The engine and CLI are [Apache-2.0](LICENSE). The Python and TypeScript SDKs declare MIT in their own package manifests. Repository: <https://github.com/Asketisch/ZhuYe>.
