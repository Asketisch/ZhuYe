# Capabilities

<p align="center">  <a href="features.md">中文</a> · <b>English</b>  </p>

> This document is ZhuYe's capability overview and known boundaries, with
> every item annotated by "enabled by default / requires configuration /
> requires a plugin". For quick start, see the root
> [README](../README.md); for how the engine is built internally, see
> [architecture.md](architecture.md); for field-level specs of tools and
> hooks, see [tools-and-hooks.md](tools-and-hooks.md).

## Three Ways to Embed (default)

The same Rust engine has three usage modes, all **available by default**; which one you pick depends on
your host:

| Mode | Entry | Notes |
|------|-------|------|
| In-process linking | `zhuye` library facade | `Zhuye::builder(model).workspace(".").build()?` + `Submission::user_input`, driving it directly inside the host process; suitable for embedding the agent as a module |
| Headless single turn | `zhuye exec` | Runs one turn and exits; events as JSONL on stdout, tracing on stderr, so `zhuye exec | jq` stays free of log noise |
| Long-running session | `zhuye serve` | stdio JSONL protocol entry; the Python / TypeScript SDKs are both built on it, supporting multi-turn shared state, remote tool replies, approval interactions, and more |

For offline development, the examples and e2e use `ZHUYE_MODEL=mock` to skip real LLM network calls, so
it runs without an API key.

## Tools Stay in Your Own Process (default)

Functions you write in Python or TypeScript do not need to be rewritten in Rust. Register a name and an
argument schema, and when the model wants to use it, the engine sends the request back to your process
via the serve protocol; after the function runs, the result is sent back. Built-in / MCP / remote tools
all go through the same `ToolExecutionQueue`.

## Core Built-in Tools (default)

The engine ships with **25 unconditional core built-in tools plus 2 gated plugin tools** (a single
registration list in `zhuye-tools`, shared by the exec / serve / lib facade paths):

Unconditional (25):

`echo`, `bash`, `background_status`, `read`, `write`, `edit`, `delete_file`, `grep`, `glob`,
`EnterPlanMode`, `ExitPlanMode`, `PlanWrite`, `EnterWorktree`, `ExitWorktree`, `ast`, `web_fetch`,
`ask_user_question`, `ask_user`, `request_human_input`, `tool_search`, `image_view`,
`brief`, `get_context_remaining`, `checkpoint`, `rewind`.

Gated plugin tools (on by default; can be disabled wholesale — see "Three-Layer Tool Control" below):

- `web_search` — the `web-search` plugin (Brave Search API; key via the `[web_search]` section,
  Auto approval);
- `notebook_edit` — the `notebook` plugin (Jupyter cell-level editing, integrated with the
  read_before_edit hook).

Note: this is the **core registration list, not the full set** — orchestration, cron, and plugins can
register more tools. The tool schema sent to the LLM is trimmed by PreLoop (see architecture.md §5);
unregistered or trimmed tools never enter the prompt. `bash` runs in the OS sandbox by default (see
"Security Gates").

## Three-Layer Tool Control

There are three orthogonal "off" entries, from coarse to fine:

| Layer | Entry | Effect |
|---|---|---|
| Assembly · registration gate | `[plugins].disabled_plugins` (plugin-level) | The tool is **not registered**: `tool_search` cannot find it, calls get tool not found; this is how the default-on gated plugins (web-search / notebook) are turned off |
| Definition · visibility filter | `[active] tools` / `[active].disallowed_tools` (tool-name-level) | The tool stays registered but is **invisible to the LLM** (allowlist ∩ / blocklist ∪, composable with agent-file frontmatter) |
| Execution · approval | permission rules / floors (operation-level) | Decides whether a call may run; `bash` defaults to Prompt, `web_search` to Auto |

The gated-plugin enable judgment and default table live in `zhuye-plugin`
(`BUNDLED_PLUGIN_DEFAULTS` / `bundled_plugin_enabled`; explicit disable > explicit
enable > default). The judgment is **not hot-reloaded** — restart the session
after changing it.

## Form-Judgment Conclusions (not pluginized)

Tool-batch ordering, execution journaling, turn scheduling, task-tree budgets
and context provenance are general execution guarantees that must coordinate
with the runtime queue, persistence and cancellation. Domain tasks and business
acceptance remain with hosts, hooks or plugins. Serial turns, strict journaling,
task budgets and dynamic context can be disabled through configuration;
provenance can be set to `off`. Explicitly configured isolation fails closed;
removing isolation configuration restores the shared workspace.

Sub-agent continuation is a built-in orchestration primitive: pass a completed
session's `session_id` and a new `prompt` to the same `call_<role>` tool to
retain its history and workspace. Concurrent follow-ups and role mismatches are
rejected. Sessions live in the parent factory's process, with at most 64 retained;
evicted sessions and sessions from an earlier process cannot be continued. Each
follow-up is charged to the current task-tree budget.

Per the root AGENTS.md "form judgment (main loop vs plugin)" iron rule, the
capabilities **kept built-in** and why:

- File family / bash family / Plan-mode trio / ask_user family / `tool_search` /
  `get_context_remaining` — main-loop essentials (StateGraph flow, approval
  side-channel, context governance depend on them directly);
- `checkpoint` / `rewind` / worktree tools — depend on the engine recorder and
  rollout truncation (engine-internal state via ToolContext); the workspace-side
  snapshot already ships in the git-tools plugin — the builtin pair carries the
  conversation-rewind semantics, and the two do not mix;
- `brief` / `image_view` — pure-local generic conveniences (`image_view` returns
  a multimodal ContentBlock, which plugin tool output cannot carry);
- `ast` — heavy tree-sitter dependency; evolves via a feature gate rather than
  pluginization;
- vendor quota providers (Kimi / Zhipu / Minimax / Zenmux) — in-process
  ModelRegistry machinery re-exported by the SDK facade;
- the `langfuse_tracker` hook — the plugin hook slots (shell / WASM) cannot
  carry OTLP/HTTP export; requires extending the plugin hook form first
  (on the roadmap);
- `zhuye-eval` — a standalone evaluation tool (the `zhuye-eval-score` binary),
  not an agent-runtime capability; not pluginized.

## Multiple LLM Providers (default)

Native implementations for the **Anthropic, OpenAI, and Ollama** providers, plus an offline `mock` (runs
without a key) and an OpenAI Responses implementation. The OpenAI-compatible interface can serve other
vendors (point `base_url` at any compatible gateway), so you do not have to write a separate native
client for each one. Each model's prompts are layered as "default → vendor → specific model"; switching
models automatically switches to the copy suited to it; you can also configure a fallback candidate
chain, so if one provider goes down it automatically switches to the next.

## Five Orchestration Primitives (default)

Multi-agent orchestration is a built-in capability of the engine, not a plugin. Each of the five
primitives is its own crate:

| Primitive | Crate | Notes |
|------|-------|------|
| Sub-agent | `zhuye-subagent` | Sub-agent factory, concurrent in-flight ≤ 16; can use an independent model registry |
| Role discussion | `zhuye-discussion` | Multi-role discussion orchestration, message bus + convergence decision |
| Task/team | `zhuye-task` | Declarative tasks, team / squad delegation |
| Pipeline | `zhuye-pipeline` | DAG pipeline; `Abort` / `ContinueCollect` failure policies |
| Goal mode | `zhuye-goal` | After `EnterGoalMode`, each turn's `on_turn_end` self-check + continuation |

A sub-agent's `call_<role>` supports two dispatch modes. `mode="sync"` (default) blocks the
parent turn until the sub-agent finishes and returns its result directly. `mode="async"` returns
a placeholder receipt immediately while the sub-agent keeps running in the background: the parent
agent can make progress on other work, and once it has nothing else to do the turn **suspends**
waiting for the sub-agent. When the sub-agent finishes, its result is injected back into the
current turn's context (under the same context budget as tool results, and written to the session
record so later turns can still see it) and the main agent is resumed **within the same turn**.
An async sub-agent never outlives the turn that dispatched it: interrupting the parent turn also
cancels its in-flight async sub-agents, exactly as in sync mode.

Without worktree isolation, a sub-agent uses the parent session's current workspace.
Enabling the coordinator in the CLI or async Rust builder requires Git worktree
isolation. Dispatch reports an error if Git root discovery or worktree creation
fails. Plugin sub-agents inherit their parent session’s tool journaling policy and
write separate session records under the same root when rollout is enabled.

The side-channel lets user-initiated concurrent agents run in parallel with the main turn without
interfering with each other (carried by `zhuye-core`).

## Context Management (default)

Approaching the context limit, it first trims locally (step-by-step escalation); only when it really
cannot be squeezed does it call the summarization model to rewrite; after compaction, still-active files
are read back in. Related primitives:

- **Compaction escalation** (`zhuye-compact`) — upgrades the strategy
by the level calibrated from the same model report; trimming that stays at or above the trigger is not a successful compaction;
- **Cross-session memory** (`zhuye-memory`) — the `memory` tool lets
the model write experience into cross-session memory, isolated by scope;
- **Skills directory** (`zhuye-skills`) — reusable skill packs;
- **Session notes** (`zhuye-notes`) — injected on every
`pre_loop`, appended into the message by `model_call`;
- **BM25 relevance ranking** (`zhuye-bm25`) — local retrieval ranking,
no model dependency;
- **External prompt library** (`zhuye-prompt`) — prompts externalized
and matched per model, see [docs/prompts.md](prompts.md).

## Security Gates (default)

Three layers of gates stacked, all active by default:

- **Permission modes** (`zhuye-permissions`) — decide which operations
to ask about, which to let through, and which to forbid outright; `bash` defaults to Prompt permission,
`web_search` defaults to Auto;
- **Hooks** (`zhuye-hooks`) — 12 event types, 7 decision kinds; can
modify arguments before/after tool execution, block calls, interject, and veto turn completion;
- **MCP client** (`zhuye-mcp`) — stdio and streamable-http transports;
  the http transport supports client-side OAuth
  (`[mcp_servers.<name>.oauth]`: static token / `token_env` / client
  credentials / refresh token, with the token endpoint discovered from the
  protected-resource metadata when unset), reports "authorization required"
  explicitly when a server demands auth without usable credentials, and
  keeps the tool registry in sync when a server changes its tool list
  (`notifications/tools/list_changed`).
- **OS sandbox** (`zhuye-sandbox`) — Seatbelt on macOS; Landlock for
filesystem writes and seccomp for networking on Linux. When Linux sandboxing is
enabled, installation failures fail closed. macOS
still allows local and Unix sockets; Linux blocks socket creation and connection by default.

In addition, there is a **prompt injection guard** (enabled by default since F01) and **output
sanitization** (`zhuye-sanitize`, see [docs/sanitize.md](sanitize.md)), which redact keys in tool output.
**Audit** (`zhuye-audit`) and **encrypted key storage** (`zhuye-secrets`, AES-256-GCM + machine key)
provide un-bypassable audit records and at-rest key protection. Note: auditing can be turned off via the
`[telemetry].activity` setting; it is not an unconditional per-turn record. Field-level details are in
[tools-and-hooks.md](tools-and-hooks.md).

## Integrations (requires configuration)

- **MCP client** (`zhuye-mcp`) — stdio / streamable-http transports;
- **LSP** (`zhuye-lsp`) — language server integration; activation is driven by
  `[lsp_servers]` config (semantics unchanged), with the `lsp` gated plugin
  carrying a **coarse switch** (explicit disable turns it fully off — see
  [plugins/lsp/](../plugins/lsp/));
- **Computer-use** (`zhuye-computer-use`) — screenshot / keyboard-mouse,
gated by the `real` feature (default is mock; real operations require explicitly enabling the feature);
- **Git capability family** (`zhuye-git` / `zhuye-git-tools`) —
checkpoint / rewind (tag-based snapshots, still reachable after rewinding) + branch / PR probing;
- **WASM component plugin host** (`zhuye-wasm`) — Wasmtime component
model, multi-language plugin runtime; 30s deadline, no fs / network / env inheritance;
- **Verification hook pair** (`verification` / `test_runner`) — off by default;
  enable via either equivalent path: list them explicitly in config `[hooks]
  enabled`, or install and enable the `verify-hooks` gated plugin (see
  [plugins/verify-hooks/](../plugins/verify-hooks/)).

## Persistence and Recovery (default)

The whole process is written as a JSONL rollout (`~/.zhuye/sessions/YYYY/MM/DD/<thread>.jsonl`),
supporting recovery by ID, most recent session, or ordinal: `--resume <uuid>` / `-c` (continue the most
recent) / `-r N` (by ordinal, 1-indexed, newest first); the three are mutually exclusive. You can look
back, keep running, fork a new branch, search across sessions, and export to Markdown with usage
statistics.

## Resident Shell Session Pool (requires configuration)

`zhuye-shell` maintains a pool of resident shell sessions, avoiding creating a new process for every
`bash` call; suitable for frequent short-command scenarios.

## cron / autopilot (requires configuration)

`zhuye-stream` provides real scheduled cron (5-field expression parsing) and autopilot (automatic
advancement); tasks persist across restarts. Only takes effect after a cron expression / autopilot policy
is configured on serve.

## Plan Mode (default)

Once Plan mode is entered, `write` / `edit` are forcibly removed and the LLM can only use the
path-restricted `PlanWrite`; approval goes through the `PlanApprovalGate` and session-level fan-out.
Design in [docs/PLAN_MODE.md](PLAN_MODE.md).

## Boundaries and Limitations

- This repository's binary has no TUI subcommand. The interactive
interface lives in the [Zhuye-TUI](https://cnb.cool/Demon1019/Zhuye-CLI) repository.
- Linux Landlock does not protect sensitive reads, and it cannot force nested
  sensitive paths inside a writable root to be read-only — Landlock merges
  rules bitwise, so write rights granted on a parent propagate to the nested
  paths, and the kernel rejects an empty access mask (`ENOMSG`); both were
  verified in a Linux container. Such paths are therefore handled as
  "report + fail closed on demand" on Linux: the bash tool's
  `metadata.path_protection` lists every path as enforced or unsupported, and
  when a host requires a path to be read-only
  (`OsSandbox::with_protected_paths` + `strict_paths`, or
  `ZHUYE_SANDBOX_STRICT_PATHS=1`) Linux refuses to execute instead of
  pretending the protection is active. macOS Seatbelt supports deny rules, so
  `.git` / `.agents` / `.aws` inside a writable root really are read-only.
- The default seccomp network filter supports x86_64 and
  aarch64, allows local `socketpair`, requires Landlock ABI v3, and fails closed
  if unavailable.
- **Windows is an unverified platform**: the Job Object (process-tree
  kill-on-close) + AppContainer backend is implemented but is not wired into
  the bash spawn path, so commands on Windows are currently **not** confined
  for files or networking, and the behaviour has **not** been verified on a
  real Windows machine. `status_line()` and the tool metadata report this
  honestly; use macOS / Linux when you need hard isolation.
- At most 16 sub-agents can be in flight in the same session at once;
slots are released as sub-tasks finish.
- Protocol v0 is frozen: adding `Op` / `EventMsg` variants is
non-breaking, but **existing variants must not be changed**; clients must tolerate unknown `"type"`
values (see [sdks/PROTOCOL.md](../sdks/PROTOCOL.md) for details).
