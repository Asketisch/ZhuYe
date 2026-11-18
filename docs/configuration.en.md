# Configuration & Environment

<p align="center">  <a href="configuration.md">中文</a> · <b>English</b>  </p>

> Configuration files, hot reload, and environment variables. Field
> definitions for the full config schema live in
> [`zhuye-config`](../crates/resources/zhuye-config/src/schema/mod.rs).

## Configuration File

Config lives in `~/.zhuye/config.toml`; `ZHUYE_HOME` relocates the whole
directory:

```toml
[active]
provider = "openai"

[openai]
api_key = ""                                  # endpoints without auth can stay empty
base_url = "https://api.kilo.ai/api/gateway"
model = "free"                                # free model: one is picked from the catalog at runtime
temperature = 0.3
max_tokens = 4096

[model]
context_window = 128000                       # max context length of the model (tokens); the
                                              # adaptive output budget and compaction
                                              # thresholds follow it. Private endpoints /
                                              # new models can also be configured per-model
                                              # via the [context_windows] table:
                                              # [context_windows]
                                              # "MiniMax-M3" = 1000000
reasoning_effort = "high"                     # thinking effort: low|medium|high|max|ultra.
                                              # It is clamped per model family before hitting
                                              # the wire — qwen up to high, deepseek up to
                                              # max, OpenAI (gpt-5 / o series) up to ultra
```

The anthropic section also supports keyless local gateways
(`api_key = ""` plus an explicit `base_url`, e.g. LM Studio).

No process restart after edits — the config watcher hot-reloads.

## Environment Variables

| Variable | Purpose |
|------|--------|
| `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` | provider credentials; pick one to start |
| `ZHUYE_MODEL` | overrides the model spec; `=mock` for offline runs |
| `ZHUYE_HOME` | data directory, defaults to `~/.zhuye` |
| `ZHUYE_BIN` | where the SDKs find the `zhuye` binary (resolution order: `options.bin` → `ZHUYE_BIN` → bundled binary → PATH, see [sdk.md](sdk.md)) |
| `ZHUYE_MEMORY_BACKEND` | memory backend: default `markdown` (MEMORY.md file + BM25 section search); `=sqlite` enables the entry version store (`memory.db` + FTS5, MEMORY.md degrades to an editable projection; first open auto-imports existing content idempotently). In sqlite mode, compaction solidification and background file tidy are disabled for now (automatic writes pending M5b via the persistent queue); new notes carry workspace ownership, and legacy unowned notes stop participating in cross-project recall |
| `RUST_LOG` | log filter, default `warn,zhuye=info` |

Other environment variables are scattered across crates; search the
source for the `ZHUYE_` prefix.
