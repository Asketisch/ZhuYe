# Architecture

<p align="center">  <a href="architecture.md">中文</a> · <b>English</b>  </p>

> This document is the architecture reference for ZhuYe: framework
> boundaries and entry points, the six-layer workspace structure, protocol
> and session lifecycle, the state transitions of a single turn, node
> context and capability assembly, and the key design decisions. The
> README covers usage; this one is for readers who need to understand the
> internals or do secondary development. For finer wire semantics, see
> [sdks/PROTOCOL.md](../sdks/PROTOCOL.md); for field-level specs of tools
> and hooks, see [tools-and-hooks.md](tools-and-hooks.md).

## 1. Framework Boundaries and Entry Points

ZhuYe is an **AI agent runtime framework written in Rust**, not a coding assistant for the terminal. It
has several consumption patterns; the main interfaces and boundaries are as follows:

- **Library facade (`zhuye` crate)** — the main entry point for secondary
development. `Zhuye::builder(model)` is the only constructor; it chain-assembles workspace / approvals /
plugins / prompt library / telemetry / rollout and produces a handle that can drive one `AgentThread`
in-process. The `zhuye` library flattens and re-exports the key types of each layer crate (a hundred-plus
public names); a host process just depends on it directly.
- **Headless CLI (`zhuye exec`)** — a thin CLI entry, emitted by the
`zhuye-cli` crate as a single `zhuye` binary. It **does not include a `tui` subcommand** (the TUI has
been split into the standalone repository [Reflect-TUI](https://cnb.cool/Demon1019/Reflect-CLI)). It runs
one turn and exits: events are written as a JSONL stream to stdout, tracing logs go to stderr, so `zhuye
exec | jq` is not polluted by logs.
- **Serve protocol entry (`zhuye serve`)** — a resident stdio JSONL
session service; one process = one resident `AgentThread`, with in-memory state shared across multiple
turns. Both the Python / TypeScript SDKs are built on this protocol (see
[sdks/PROTOCOL.md](../sdks/PROTOCOL.md)).
- **PyO3 bindings (`zhuye-py` crate)** — a separate crate, not part of the
`zhuye` library facade, for Python hosts to link directly.
- **External session import (`zhuye-import`)** — a standalone binary
(produced by the `bin` of the `zhuye-import` crate) that converts session records from other harnesses
into rollout; see [plugins/harness-import](../plugins/harness-import).
- **WASM plugin host / git tools / computer-use / shell** — these are the
capability core of `plugins/*`, implemented in the integrations layer, but they do **not** enter the SDK
facade; they are mounted in plugin form.

In one sentence: the engine is a library; the binaries are just a thin shell around it.

## 2. The Six-Layer Workspace Structure

The workspace has **45 crates**, grouped into 6 layers under `crates/<layer>/<crate>/`, with dependencies
flowing **top-down** (upper layers depend on lower ones; `protocol` is the common contract of all layers
and is depended on by every layer). The complete member list in the root [Cargo.toml](../Cargo.toml)
`members` section is the single source of truth; the table below corresponds to it line by line.

| Layer | Crates | Responsibilities |
|----|-------|------|
| `protocol/` | `zhuye-protocol` | Base types `Submission` / `Op` / `Event` / `EventMsg` / `Item` + the `RolloutRecorder` trait. The common contract of all layers |
| `abilities/` | `llm`, `tools`, `bm25`, `hooks`, `skills`, `memory`, `agent-def`, `prompt`, `compact`, `recovery`, `notes`, `sanitize`, `kms`, `eval` | Capability primitives: `ModelClient` trait + providers, `Tool` trait + built-in tools, BM25 relevance ranking, `HookEngine`, skills directory, cross-session memory, agent definitions, prompt library, compaction escalation, checkpoint recovery, session notes, output sanitization, key management, evaluation system |
| `resources/` | `config`, `permissions`, `plugin`, `rollout`, `telemetry`, `sandbox`, `audit`, `secrets`, `fs`, `git`, `ast`, `wasm` | Configuration hot-reload, permission rules, plugin lifecycle, JSONL persistence, observability telemetry, OS sandbox (macOS Seatbelt / Linux Landlock), audit logging, encrypted key storage, shared file primitives (atomic writes), git capability family (checkpoint/branch/instruction parsing), tree-sitter AST search, WASM component plugin host |
| `orchestration/` | `subagent`, `discussion`, `task`, `pipeline`, `goal` | Multi-agent orchestration: sub-agent factory (concurrent in-flight ≤ 16), role discussions, task/team/squad delegation, DAG pipelines, goal mode |
| `integrations/` | `mcp`, `lsp`, `integration`, `stream`, `import`, `computer-use`, `shell`, `git-tools` | MCP client (stdio / streamable-http), LSP, external integrations, streaming session backend (including cron / autopilot / queue), external harness session import, Computer Use (screenshot/keyboard-mouse, gated by the `real` feature), resident shell session pool, git workflow MCP server |
| `runtime/` | `core`, `exec`, `cli`, `zhuye`, `py` | `AgentThread` + `StateGraph` (core), headless + serve (exec), CLI entry (cli), library facade + Builder (zhuye), PyO3 bindings (py) |

Layering rule: dependencies may only flow from top to bottom. `protocol` sits at the bottom; any layer
may depend on it. `runtime` sits at the top, aggregating all layers and exposing the entry points.
`orchestration` and `integrations` are both in the upper-middle; they do not depend on each other, only
sharing dependencies on `abilities` / `resources` / `protocol`.

## 3. Protocol and Session Lifecycle

There is exactly one contract between clients and the engine, defined in the `protocol` layer, running
through exec / serve / the library facade / SDKs:

- **`Submission` / `Op`** — client → engine. One `Submission` carries
one `Op` (user input, tool approval replies, compact, rewind, set_permission_mode, query_guard_stats,
etc.). In serve mode, each line of stdin is one `Submission` JSON.
- **`Event` / `EventMsg`** — engine → client. Each `Event` carries one
`msg: EventMsg` variant (turn_started / agent_message_delta / tool_call_begin / tool_execution_request /
approval_request / turn_complete / …). In serve mode, each line of stdout is one `Event` JSON.
- **`Item` base types** — shared data structures such as
`ContentBlock` / `ToolError` / `ChatMessage` (`item.rs`). `Item` is the module-level umbrella name for
these types; there is no public type by that name.
- **`RolloutRecorder` trait + `RolloutRecord`** — persistence. JSONL
on disk (`~/.zhuye/sessions/YYYY/MM/DD/<thread>.jsonl`), recording `SessionMeta` / `Message` /
`Compaction` / `Fork` / `Checkpoint` / `DiscussionTranscript` / `TokenCount`, etc.

### v0 Freeze and Additive Rule

**Protocol v0 is frozen**: adding `Op` / `EventMsg` variants is non-breaking; **existing variants must
not be changed** (changing a field is breaking and is forbidden). Wire stability is a shared precondition
of all clients (TUI / exec / serve / SDKs); discriminators are serde `snake_case`, and **tolerating
unknown `"type"` values is the client's obligation**.

Recent additive examples (additions only, no changes):

- `QueryGuardStats` / `GuardStats` (prompt injection guard ruling
statistics snapshot);
- `ToolCallEndEvent` gained an optional `args` field (carrying the
tool call arguments);
- `tool_call_args_delta` (streamed tool argument increments);
- `submission_closed` (deterministic close-out for non-turn
operations).

For the complete wire semantics, handshake, concurrency ordering, remote tool execution flow, exit and
error formats, see [sdks/PROTOCOL.md](../sdks/PROTOCOL.md); for the default hook behavior on serve
(built-in hooks are disabled by default), see §3.1 of that document.

## 4. A Single Turn and State Transitions

The engine's processing has no black magic. `AgentThread` (`zhuye-core`) holds the `mpsc` channel of
`Submission` and `tokio::spawn`s the `submission_loop`; `submission_loop` loops receiving `Submission`,
dispatching the `Op`, and the `UserInput` branch constructs and runs the 4-node `StateGraph`
(`crates/runtime/zhuye-core/src/agent_thread.rs:57-68`, `submission_loop.rs`).

`StateGraph` runs **4 nodes**: `PreLoop → ModelCall → (ToolExec → PreLoop)* → CheckStop`. Transitions are
hand-written as an enum + match via `GraphNode::next(has_tool_calls)`; no petgraph is introduced
(`graph/mod.rs:28-40`):

- `PreLoop` → `ModelCall` (unconditional)
- `ModelCall` with tool calls → `ToolExec`; without tool calls →
`CheckStop`
- `ToolExec` → `PreLoop` (tool results feed back into the model,
continuing multi-step reasoning)
- `CheckStop` allows → turn ends; Stop hook vetoes → after appending the
feedback to history, return to `PreLoop` (forced continuation, see `graph/nodes/check_stop.rs:20-43`)

```
PreLoop ──▶ ModelCall ──▶ ToolExec ─┐
   ▲            │  ▲               │
   │            ▼  │               │
   │         CheckStop◀────────────┘
   │            │
   │   (Stop hook vetoes → back to PreLoop)
   │            │
   └────────────┘   (allowed → turn ends)
```

Responsibilities of each node:

| Node | Responsibility |
|------|------|
| PreLoop | Compaction escalation decisions (upgrading step by step per input token threshold), pre-processing hooks, history assembly, harvesting the steering queue |
| ModelCall | After resolving the primary model via `RoutingPolicy`, call `ModelClient` (SSE streaming), producing text/thinking deltas and tool calls |
| ToolExec | Approval gate check → execute tools (built-in / MCP / remote) via `ToolExecutionQueue` → write results back into history; Plan mode switches here |
| CheckStop | Decides turn end when the model has no new tool calls; dispatches the Stop hook, which may veto and force continuation |

When the Stop hook vetoes, `check_stop` judges the veto by the decision leaf's `deny_reason`, appends the
hook-injected reminder (e.g., failing test output) as a `<system-reminder>` User message into history,
then returns to `PreLoop` so the model sees the veto reason and continues; `stop_hook_attempts` counts
them, and the max-iteration safety valve prevents infinite continuation as a backstop.

Errors and cancellation: when `model_call` / `tool_exec` return `None` due to an error or a triggered
`CancellationToken`, the turn is an **abnormal end** and `completed_normally` is not set; only a natural
`CheckStop` termination sets it. Cancellation comes from user interruption, timeout, or budget
exhaustion, after which the `TurnHandle` closes.

### Real consumption points of orchestration, routing, and recovery

- **Goal continuation.** After `EnterGoalMode`, each `TurnComplete`
calls `GoalController::on_turn_end`. When the goal is not complete, the continuation text advances the
`steering_queue`; the next `UserInput` `drain_all`s it inside `submission_loop` and merges it into that
user message. The engine does not submit another turn by itself.
- **Pipeline failure policy.** Neither `Abort` nor `ContinueCollect`
ends the loop early. Downstream of a failed node is uniformly `skipped`; independent branches keep
running. The overall status is `failed` and `partial` respectively.
- **Routing.** `model_call` consumes the primary, fallbacks, and
credential cooldown of `Role::Main`. When constructing the summarizer, the primary of `Role::Compact` is
copied into model_name; the summarization call only runs next_for on this single spec and does not read
compact's fallbacks. Sub-agents use the primary model at startup, or the independent registry of
`[subagent_providers]`.
- **Context recovery.** Active files are re-read only on
`compact_triggered` (any non-Noop compaction). Sub-agent registration and session notes are injected on
every `pre_loop`, then appended into the message by `model_call`.

## 5. Node Context and Capability Assembly

`NodeContext` is a **cheaply clonable** turn-level dependency bundle (`submission_loop.rs:65-155`); each
`StateGraph` node holds a clone of it. Fields are grouped by responsibility:

- **Identity and turn**: `turn_id`, `session_id` (M5, stable across
turns, so the rollout recorder can mark the same session).
- **Model and routing**: `model` (`Arc<RwLock<String>>`, hot-reloaded
model switches take effect on the very next request), `policy` (`RoutingPolicy`, role → spec slot),
`registry` (`SharedModelRegistry`, auto-advance to the next within the credential pool), `effort`
(session-level reasoning effort; the `/effort` switch affects the next turn).
- **Hooks and tools**: `hook_engine` (`HookEngine`), `tools_queue`
(`ToolExecutionQueue`, unified entry for built-in / MCP / remote, with approval and sanitization attached
inside).
- **Events and cancellation**: `event_tx` (`mpsc::Sender<Event>`),
`cancel` (`CancellationToken`), `approval_gate` (turn-level approval gate; None when headless runs
without confirmation).
- **Budget and usage**: `max_iterations`, `session_usage` (cumulative
session tokens), `token_budget` (hard cap; `model_call` returns None early once the cumulative total
exceeds the threshold), `quota_tracker` (token plan quota; exhaustion triggers credential cooldown).
- **Compaction and steering**: `force_compact_next` (the `/compact`
force flag), `steering_queue` (changing requirements mid-turn while it runs).
- **Observability and goal**: `telemetry` (`TelemetrySink`, None =
off), `goal` (goal mode controller, self-check at turn end + continuation), `recorder`
(`RolloutRecorder`).
- **Plan mode**: `plan_approval_gate`, `plan_session_subs`
(session-level fan-out), `cfg` (shared `AgentConfig`, letting approval decisions flip the session-level
permission_mode slot).
- **Sandbox**: `sandbox_override` (OS sandbox policy delivered per turn,
passed through `ToolContext.os_sandbox` to BashTool).

Capability assembly: `AgentThread::new` injects the sanitizer (output key redaction), the `HookEngine`
(including built-in + plugin hooks), the prompt injection guard, the session-level permission_mode
handle, and the permission resolver into `ToolExecutionQueue`; clones of `NodeContext` are then
distributed to the individual nodes. Tool surface trimming is covered in the next section.

### Tool Surface Trimming (the schema sent to the LLM)

The tool schema sent to the LLM is produced by `ToolRegistry::list_specs()` and then filtered by PreLoop
— unregistered or trimmed tools **never enter the prompt at all**. The core built-in tools number **27**
(`builtin_tools.rs`; the three paths exec / serve / lib facade share this one registration list): `echo`,
`bash`, `background_status`, `read`, `write`, `edit`, `delete_file`, `grep`, `glob`, `EnterPlanMode`,
`ExitPlanMode`, `PlanWrite`, `EnterWorktree`, `ExitWorktree`, `ast`, `web_fetch`, `web_search`,
`ask_user_question`, `ask_user`, `request_human_input`, `tool_search`, `image_view`, `notebook_edit`,
`brief`, `get_context_remaining`, `checkpoint`, `rewind`. Note: this is the core registration list, **not
the full set** — orchestration, cron, and plugins can register more tools.

Trimming has three entry points, divided by "who it faces":

1. **Agent definition layer (Markdown frontmatter, the main entry).**
Placed in `.zhuye/agents/` (workspace) or `~/.zhuye/agents/` (home; on name collision, home overrides
workspace), activated via `zhuye exec --agent <name>`. The semantics are **whitelist ∩, blacklist −,
`readonly` excludes mutating tools**; if all three fields are empty, no filtering happens (backward
compatible). Under Plan mode, `write` / `edit` are forcibly removed and the LLM can only use the
path-restricted `PlanWrite`.
2. **Sub-agent layer (config.toml / Markdown).** The `allowed_tools`
whitelist of `[[subagents]]` decides the sub-agent tool surface; at spawn time a filtered registry
containing **only the whitelisted tools** is built for the sub-agent. Source merge priority (later
overrides earlier on name collision): `~/.zhuye/subagents/*.md` → workspace `.zhuye/subagents/*.md` →
config.toml `[[subagents]]`.
3. **Code layer (ToolRegistry API).** Library users get the registry via
`agent.thread().tools()` and add or remove tools directly; `register_except`'s signature carries a source
leading parameter: `registry.register_except(source, tools, excluded)`.

## 6. Design Decisions

- **Protocol v0 is frozen**: adding `Op` / `EventMsg` variants is
non-breaking; existing variants must not be changed. Wire stability is a shared precondition of all
clients.
- **StateGraph transitions are hand-written as enum + match**: a fixed 4-node
structure, no petgraph — the control flow is explicit and auditable, one of the core differences between
this framework and black-box agent loops.
- **`ToolError` is defined in the `protocol` layer**
(`item.rs:515`): it breaks the `tools ↔ hooks` circular dependency (allowing
`HookEvent::PostToolUseFailure` to carry it), sinking the error type down into the common contract layer.
- **Error handling**: `thiserror` / `anyhow` are a convention, not an
exclusive rule — library crates use `thiserror` (explicit error types), the application layer (exec/cli)
uses `anyhow` (error chain propagation); zhuye-core uses both.
- **MSRV and edition**: Rust stable, **MSRV 1.89**,
`edition = "2024"` (security dependency surface requirement, see `workspace.package` in the root
Cargo.toml).
- **JSONL on stdout, logs on stderr**: `zhuye exec | jq` is not broken —
the event stream and diagnostic logs are strictly separated, so pipe consumers always get pure JSONL.
- **Serve is the embedding entry point, not a development assistant**:
built-in hooks are disabled by default on serve (exec / TUI keep "unconfigured = all enabled"), avoiding
Stop hooks like verification from running tests in the host cwd. The host of serve is the SDK user's
process, not the developer themselves.
- **Profile strategy**: `[profile.release]` uses thin LTO for release
distribution; `[profile.release-fast]` has no LTO and high parallelism (opt-level 2, incremental) for
fast local iteration; `[profile.dev]` deliberately disables incremental (see the comments in the root
Cargo.toml).
- **The facade export count is not pinned down**: the number of
re-exports in the `zhuye` library facade keeps changing across versions; this document only describes it
qualitatively. The cron / autopilot / queue types of zhuye-stream are already re-exported through the
facade (`lib.rs:125-130`).
