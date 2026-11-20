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
group = "openai"                   # active model group id (the id in the model group array)

[[model_group]]                    # model groups — the only model entry point
id = "openai"                       # group id = model spec prefix (<id>/<model>) and
                                   # the credential pool name. Required, unique, must not
                                   # contain '/', and must not collide with the reserved
                                   # name `mock` — violating groups are skipped with a
                                   # warning instead of breaking the rest of the config
label = "OpenAI"                    # display name (defaults to the id)
endpoint = "openai"                 # wire format: openai | anthropic | ollama
api_key = ""                        # endpoints without auth can stay empty
base_url = "https://api.kilo.ai/api/gateway"
model = "free"                      # free model: one is picked from the catalog at runtime
temperature = 0.3
max_tokens = 4096
timeout_secs = 300                  # HTTP timeout: connect + **read-idle** two-layer
                                    # (reset on every byte). Not the total request
                                    # duration — a legitimate long stream is not cut
                                    # short; only a connection with no bytes for that
                                    # long is dropped

[model]
context_window = 128000             # max context length of the model (tokens); the
                                    # adaptive output budget and compaction
                                    # thresholds follow it. Private endpoints /
                                    # new models can also be configured per-model
                                    # via the [context_windows] table:
                                    # [context_windows]
                                    # "MiniMax-M3" = 1000000
reasoning_effort = "high"           # thinking effort: low|medium|high|max|ultra.
                                    # It is clamped per model family before hitting
                                    # the wire — qwen up to high, deepseek up to
                                    # max, OpenAI (gpt-5 / o series) up to ultra
```

Groups with `endpoint = "anthropic"` also support keyless local gateways
(`api_key = ""` plus an explicit `base_url`, e.g. LM Studio). Groups with
`endpoint = "ollama"` additionally accept the local-inference-only fields
`keep_alive_secs` / `num_ctx` / `num_gpu`.

## Migrating from v1 configs

The v1 fixed sections `[anthropic]` / `[openai]` / `[ollama]` are **still
readable** — loading migrates them into equivalent model groups, and writing
back emits only the new format:

| Old config | New config |
|---|---|
| `[anthropic]` | model group `id = "anthropic"` / `endpoint = "anthropic"` |
| `[openai]` | model group `id = "openai"` / `endpoint = "openai"` |
| `[ollama]` | model group `id = "ollama"` / `endpoint = "ollama"` |
| `[active].provider` | `[active].group` |
| `[[<section>.credentials]]` | in-group `[[model_group.credentials]]` |

`api_key` / `base_url` / `model` / `temperature` / `max_tokens` /
`timeout_secs` / `credentials` carry over unchanged; `[openai].responses_api`
and `[ollama].keep_alive_secs` / `num_ctx` / `num_gpu` land on the group with
the same names.

The sub-agent endpoint section is renamed in step: `[subagent_providers]` →
`[subagent_models]` (the old section name is still readable; writing emits
only the new name).

The migration is **idempotent**: a config already in the new format loads
without any change (no duplicate groups are appended, and an existing group
with the same id is never overwritten — the group wins). Legacy section data
never shows up in the written output.

## How the active group is resolved

1. env `ZHUYE_PROVIDER` (explicit override, kept for compatibility);
2. env `ZHUYE_MODEL` shaped like `mock` / `mock/...` (offline mock special case);
3. TOML `[active].group`;
4. the prefix of env `ZHUYE_MODEL`, when it is a **known group id**;
5. the first group that has usable credentials;
6. none → no model configured.

Model-name prefixes are stripped only for group ids declared in the config
plus `mock` — real model names (e.g. `spark-2/qwen3.8-27b-nvfp4`) are kept
verbatim.

Env fallbacks are kept for the three fixed group ids only: `ANTHROPIC_API_KEY`
for `anthropic`, `OPENAI_API_KEY` for `openai`, and `OLLAMA_HOST` +
`OLLAMA_API_KEY` for `ollama`. Other groups never read env — a third-party
gateway swallowing `OPENAI_API_KEY` is harder to diagnose than a missing key.

Hot reload, cooldown failover, `model = "free"` catalog discovery and
`zhuye models list --provider <group-id>` all work on model groups.
Anthropic-format endpoints (including gateways) support catalog fetching
(`GET {base}/v1/models`).

No process restart after edits — the config watcher hot-reloads.

## Tool Control (three layers)

There are three orthogonal "off" entries, from coarse to fine (see
[features.md](features.md) "Three-Layer Tool Control" for details):

```toml
[plugins]
enabled_plugins  = ["git-tools@inline"]   # authoritative enable list (name@marketplace)
disabled_plugins = ["web-search@inline"]  # explicit disable of gated plugins (see below)

[active]
tools             = []                    # allowlist: non-empty intersects the LLM-visible set
disallowed_tools  = ["web_search"]        # blocklist: excludes from the LLM-visible set
```

| Layer | Entry | Effect |
|---|---|---|
| Assembly · registration gate | `[plugins].disabled_plugins` | **Plugin-level**: gated-plugin tools (web-search / notebook, on by default) are **not registered** — `tool_search` cannot find them, calls get tool not found. Equivalent command: `zhuye plugin disable <id>` |
| Definition · visibility filter | `[active] tools` / `disallowed_tools` | **Tool-name-level**: the tool stays registered but is invisible to the LLM (composable with `.zhuye/agents/*.md` frontmatter) |
| Execution · approval | permission rules / floors | **Operation-level**: decides whether a call may run |

The gated-plugin enable judgment is **not hot-reloaded** (unlike plugin
mounting); restart the session after changing it.

## Environment Variables

| Variable | Purpose |
|------|--------|
| `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` | credential fallback (only for the `openai` / `anthropic` groups) |
| `OLLAMA_HOST` / `OLLAMA_API_KEY` | credential fallback (only for the `ollama` group) |
| `ZHUYE_MODEL` | overrides the model spec; `=mock` for offline runs |
| `ZHUYE_PROVIDER` | overrides the active model group id |
| `ZHUYE_HOME` | data directory, defaults to `~/.zhuye` |
| `ZHUYE_BIN` | where the SDKs find the `zhuye` binary (resolution order: `options.bin` → `ZHUYE_BIN` → bundled binary → PATH, see [sdk.md](sdk.md)) |
| `ZHUYE_MEMORY_BACKEND` | memory backend: default `markdown` (MEMORY.md file + BM25 section search); `=sqlite` enables the entry version store (`memory.db` + FTS5, MEMORY.md degrades to an editable projection; first open auto-imports existing content idempotently). In sqlite mode, compaction solidification and background file tidy are disabled for now (automatic writes pending M5b via the persistent queue); new notes carry workspace ownership, and legacy unowned notes stop participating in cross-project recall |
| `RUST_LOG` | log filter, default `warn,zhuye=info` |

Other environment variables are scattered across crates; search the
source for the `ZHUYE_` prefix.
