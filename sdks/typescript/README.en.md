# zhuye (TypeScript SDK)

<p align="center">  <a href="README.md">中文</a> · <b>English</b>  </p>

The official SDK for driving [ZhuYe Agent](https://github.com/Asketisch/ZhuYe)
directly from Node.js / TypeScript projects; the API mirrors the
[Python version](../python/README.md). Under the hood it spawns a
`zhuye serve` subprocess and speaks the JSONL stdio protocol:
**works out of the box** (platform sub-packages with embedded
binaries are installed automatically per platform), **zero runtime
dependencies** (dev dependencies are build/test tooling only),
**protocol v0 frozen** (new variants from the Rust core degrade
automatically to `UnknownEventMsg` and never break old clients).

- Node ≥ 18, pure ESM (`import`), ships `.d.ts` types out of the
  box
- Persistent session: one process = one AgentThread, multiple turns
  share context
- Host functions can be registered directly as tools the LLM can
  call (remote tool protocol)
- Streaming output, approvals, interruption, global events,
  permission modes, plan / goal mode, and runtime rescanning of the
  skills directory (`rescan_extensions`)

## Installation

```bash
npm install zhuye
```

Works out of the box: the main package auto-installs the current
platform's sub-package (`zhuye-darwin-arm64` /
`zhuye-darwin-x64` / `zhuye-linux-x64` / `zhuye-linux-arm64` /
`zhuye-win32-x64`) via optional dependencies, which embeds the
`zhuye` binary for the corresponding platform; the SDK resolves it
first — no binary to install yourself, no environment variables to
configure. You can confirm the sub-package is in place with
`npm ls zhuye-darwin-arm64` (per platform).

**Binary resolution priority** (identical in both official SDKs):

```
options.bin  →  ZHUYE_BIN  →  platform sub-package embedded binary  →  zhuye on PATH
```

When the current platform is not in the support matrix (e.g.
musl/Alpine), it falls back to PATH: download an installer from
[GitHub Releases](https://github.com/Asketisch/ZhuYe/releases), or
build from source with `make build` to produce
`target/release/zhuye` and point `ZHUYE_BIN` at it.

## Quick Start

```ts
import { ZhuyeAgent } from 'zhuye';

const agent = await ZhuyeAgent.spawn();   // waits for the session_configured handshake
console.log(await agent.prompt('用一句话介绍你自己'));
await agent.close();                      // idempotent: Shutdown → wait for exit → force-kill fallback
```

For an offline experience use the mock model:
`ZhuyeAgent.spawn({ env: { ZHUYE_MODEL: 'mock' } })`.

## Core Concepts

```
Node process                                  zhuye serve subprocess
┌─────────────────────┐    JSONL stdio    ┌──────────────────────┐
│ ZhuyeAgent          │ ── Submission ──▶ │ AgentThread          │
│  background stdout  │ ◀──── Event ───── │  (StateGraph turn)   │
│  consumer           │                   │                      │
│  ├ id="" global q   │                   │                      │
│  └ routed per ──────│                   │                      │
│    submission id    │                   │                      │
└─────────────────────┘                   └──────────────────────┘
```

- **Persistent session**: the serve process started by `spawn()`
  stays alive until `close()`, and multiple `submit()` calls share
  the full conversation context.
- **Event dual-channel**:
  - *submission events*: keyed on the submission id and consumed
    exclusively by the `AsyncIterable` returned from `submit()`
    (streaming deltas, turn_complete, tool_call_end, etc.);
  - *global events* (`id=""`): token_count, permission_mode_changed,
    the handshake session_configured, etc., read via
    `nextGlobalEvent()`.
  The two channels never mix. Remote tool requests
  (`tool_execution_request`) are global events on the wire, but the
  SDK intercepts them internally and calls the local handler; they
  never enter either queue.
- **Iterator termination**: `turn_complete` / `turn_aborted` /
  `shutdown_complete` / `submission_closed` are terminal events
  and `for await` ends naturally after them; a connection drop
  - the new `side_channel_started` / `side_channel_finished` /
    `side_channel_status` in v1.x (side-channel agent; under the
    frozen protocol v0 these pass through as additive variants, and
    old clients degrade automatically)
  (EOF) also wakes and ends all iterators — nothing hangs forever.

## Usage Guide

### Streaming output (submit + for await)

```ts
import { ZhuyeAgent } from 'zhuye';

const agent = await ZhuyeAgent.spawn();
for await (const ev of await agent.submit('讲个笑话')) {
  const msg = ev.msg;
  if (msg.type === 'agent_message_delta') process.stdout.write(msg.delta);
  else if (msg.type === 'turn_complete') console.log('\n[turn ended]', msg.usage);
}
await agent.close();
```

`submit(text)` wraps the text into a `user_input` Op internally
and returns the event `AsyncIterable` **exclusive to that
submission**; you can also `submit(op)` to send any protocol Op
(see below).

### Get the full text in one step (prompt)

```ts
const answer = await agent.prompt('北京天气如何?', { timeoutMs: 30_000 });
```

Aggregates all deltas; `turn_complete` returns normally, while
`turn_aborted` / `error` events throw `Error` (partial text is
never silently returned as a complete answer).
The optional `timeoutMs` counts from the call, including handshake,
write and reply wait; on timeout it throws and cleans up local
listeners, but a turn the server has already accepted may still run.
When you need to interrupt, call `interrupt()` explicitly; its scope
is defined in the protocol. The timeout value must be a finite number
between 0 and 2,147,483,647 milliseconds.
When a model stream errors out, if text, thinking or tool-call events
have already been emitted, this turn is not automatically retried;
the emitted text is attempted to be written to the session record
before the error event, and you can continue in the next turn.

### Custom tools (host function → LLM tool)

```ts
import { ZhuyeAgent } from 'zhuye';

const agent = await ZhuyeAgent.spawn();
await agent.registerTool(
  'get_weather',
  '查询指定城市的天气',
  {                                  // JSON Schema
    type: 'object',
    properties: { city: { type: 'string' } },
    required: ['city'],
  },
  async ({ city }) => ({
    content: [{ type: 'text', text: `${city} 晴 26℃` }],
    is_error: false,
    metadata: {},
    elapsed_ms: 0,
  }),
);
console.log(await agent.prompt('上海天气怎么样?'));
```

Behavioral contract (protocol §5):

- When the model calls the tool, serve emits
  `tool_execution_request` and the SDK runs the handler
  asynchronously and sends the receipt back; stdout consumption is
  not blocked, and you can call `prompt()` / `submit()` again
  inside the handler.
- Handler throws (including synchronous throws) → the SDK
  automatically converts it into a text receipt with
  `is_error=true`; the LLM sees the error message and the turn does
  not hang.
- An unregistered tool gets called → an error text receipt is sent
  back (this also does not hang).
- The receipt wait limit is controlled by the environment variable
  `ZHUYE_REMOTE_TOOL_TIMEOUT_SECS` (default 120s); on timeout it
  ends with an error.

### Multiple turns & concurrency

```ts
// Multiple turns sharing context
await agent.prompt('我叫小明');
await agent.prompt('我叫什么?');       // the model can answer "小明"

// Concurrent submissions: each submit has its own id and AsyncIterable; can be consumed simultaneously
await Promise.all([
  (async () => { for await (const e of await agent.submit('问题一')) console.log(e.msg.type); })(),
  (async () => { for await (const e of await agent.submit('问题二')) console.log(e.msg.type); })(),
]);
```

### Interruption & approval

Interrupting a parent turn cancels its in-flight subagents and the
server-side wait for remote tools, but does not force-stop a host
handler already running; late receipts are ignored, and the handler's
side effects must be managed by the client itself.
Text already streamed from the model is attempted to be written to
the session record before the interruption terminal state, so later
turns can recover it.
A subagent failing mid-way is returned to the parent as a tool error;
status queries show `failed`; interruption shows `cancelled`.
The `context_tail` of `call_<role>` does not pass parent history
by default; when set, it passes tail messages and pads pairings at
tool-call boundaries.
Tools inside a subagent that require approval send
`approval_request` to the parent session; a `tool_approval`
receipt from the same session wakes the subagent's tool call, and
both approval and denial are returned per the actual outcome.
A subagent's free-text questions and structured multiple-choice
questions also enter the parent session's event stream, using the
standard `ask_user_input_response` /
`ask_user_question_response` receipts.
Subagents inherit the parent session's permission rules, permission
mode, YOLO classification and sandbox configuration; explicitly
denied tools do not become approval-executable just by crossing into
a subagent.
Process text before a subagent's tool steps can be observed via
`subagent_progress`; the final result contains only the assistant
reply after the last tool step.

```ts
// Interrupt the current turn (that turn subsequently ends with turn_aborted)
await agent.interrupt();

// Approval: after receiving the global event approval_request, return a decision
const event = await agent.nextGlobalEvent({ timeoutMs: 30_000 });
if (event?.msg.type === 'approval_request') {
  await agent.approve(event.msg.request_id, 'approve');          // or 'approve_for_session'
  // Deny: await agent.approve(event.msg.request_id, { deny: { reason: '太危险' } });
}
```

`decision` passes the wire form through verbatim, with no shape
conversion: `'approve'` / `'approve_for_session'`, or
`{ deny: { reason: '...' } }` (the legacy
`{type:'approve'}` cannot be deserialized by the server — do not
use it). Hook approval uses the same decision form (serve dispatches
it by event kind).

### Global events

```ts
const event = await agent.nextGlobalEvent({ timeoutMs: 5_000 });  // throws a timeout error when no event
```

- The handshake `session_configured` itself is in the global queue
  — it's usually the first one;
- After stdout EOF / `close()` it returns `null`; repeated calls
  are safe;
- Completely independent of submission iterators; unknown variants
  are returned as-is.

### Session control Ops (submit(op))

`submit(op)` sends any protocol Op and returns an event
`AsyncIterable` (non-turn Ops finish with `submission_closed`;
you can specify the id explicitly with `{ submissionId }`). Full Op
set (27 kinds, one-to-one with the Rust protocol; types in the
`Op` of the `zhuye` package):

| Category | Op | Key fields |
|---|---|---|
| conversation | `user_input` | `items` (text/image), optional `thread_settings{model, approval_policy, sandbox_policy, max_tool_concurrency}` |
| conversation | `compact` / `rewind` | rewind optionally with `to_turn_id` |
| control | `interrupt` / `shutdown` | interrupt optionally with `child_id` |
| approval | `tool_approval` / `hook_approval` | `id` + `decision` |
| plan | `enter_plan_mode` / `exit_plan_mode` / `plan_approval` | `plan_approval.choice ∈ auto_mode/manual_approve/revise` |
| interaction | `ask_user_question_response` / `ask_user_input_response` | respond to the global events ask_user_question / ask_user_input |
| MCP interaction | `mcp_elicitation_response` | respond to the global event `mcp_elicitation` (an MCP server asking the user for input); `action ∈ accept/decline/cancel`, `content` holds the form values (keys taken from `kind.fields[].name`) |
| state | `set_effort` / `set_permission_mode` / `cycle_permission_mode` | mode ∈ auto/prompt/plan/deny/bubble |
| prompt | `prompt_command` | `action{type: 'list'\|'switch'}`; query / rescan the prompt layer, answered with `prompt_list` / `prompt_switched` |
| steering | `steer` | `items` (optional `priority ∈ 'now'\|'attachment'`, default attachment); feed mid-turn without interrupting the in-flight turn |
| subagent | `query_subagents` | optional `child_id`; answered with a `subagent_status` snapshot |
| side-channel | `start_side_channel` / `cancel_side_channel` / `query_side_channels` | user-driven concurrent agent: start (`agent`+`prompt`) / cancel (`id`) / query (optional `id`, returns a `side_channel_status` snapshot) |
| goal | `enter_goal_mode` / `exit_goal_mode` | goal + optional verify_command / token_budget |
| tools | `register_tools` / `tool_execution_response` | usually use the `registerTool()` wrapper; no need to hand-write |
| extensions | `rescan_extensions` | **new in v1.x**: runtime rescan of the skills directory (`.zhuye/skills`, workspace + user-level; plugin injection unaffected); returns `extensions_rescanned` (skills count, commands always false) |
| security | `query_guard_stats` | **new in F01**: query the prompt_guard verdict statistics (four counters: definition-side / output-side block and review); returns `guard_stats` |

Examples: switching to plan mode, submitting with thread_settings

```ts
// side-channel query (a non-turn Op; finishes with submission_closed)
for await (const e of await agent.submit({ type: 'query_side_channels' })) {
  if (e.msg.type === 'side_channel_status') console.log(e.msg.channels);
}
for await (const e of await agent.submit({ type: 'set_permission_mode', mode: 'plan' })) {}
for await (const e of await agent.submit({
  type: 'user_input',
  items: [{ type: 'text', text: '调研一下' }],
  thread_settings: { approval_policy: 'deny' },
})) {}
```

The approval policy `approval_policy` supports `prompt` (ask for
approval), `auto_approve` (auto-approve ordinary operations, still
ask for high-risk ones) and `full_access` (auto-approve tool
calls); it is independent of Plan mode, and explicit deny rules still
take effect.

The legacy fields `effort` and `permission_mode` inside
`thread_settings` are kept for type compatibility, but the server
does not read them; use the `set_effort` and
`set_permission_mode` Ops respectively.

## API Reference

### `ZhuyeAgent`

| Member | Description |
|---|---|
| `ZhuyeAgent.spawn(options?): Promise<ZhuyeAgent>` | Starts `zhuye serve` and waits for the handshake to complete; throws `Error` if the binary is not found (surfaced at the first call after `await`) / the handshake times out |
| `ZhuyeAgent.fromStdio(stdin, stdout, handshakeTimeoutMs?)` | Test injection: use an existing duplex stream instead of spawning |
| `registerTool(name, description, parameters, handler): Promise<void>` | Registers a remote tool, callable at any time; silently accepted, errors reported asynchronously via error events |
| `submit(text): Promise<AsyncIterable<Event>>` | Submits user text; returns the event stream for that submission; throws after close or disconnection |
| `submit(op, opts?): Promise<AsyncIterable<Event>>` | Submits any Op; `opts.submissionId` specifies the id explicitly |
| `prompt(text, opts?): Promise<string>` | Aggregates deltas and returns the full text; `timeoutMs` can limit the local wait; throws `Error` on interruption, error, or disconnection before the terminal state |
| `interrupt(childId?): Promise<void>` | Interrupts an in-flight turn |
| `approve(approvalId, decision): Promise<void>` | Returns an approval decision (wire form passed through) |
| `nextGlobalEvent(opts?): Promise<Event | null>` | Reads a global event; throws when `timeoutMs` elapses, returns `null` after EOF |
| `close(): Promise<void>` | Idempotent close: Shutdown → wait 2s → force-kill; subsequent public writes throw, iterators finish |
| `childProcess` (getter) | Exposes the serve subprocess handle (for tests / diagnostics) |

### `ZhuyeAgentOptions`

| Field | Default | Description |
|---|---|---|
| `bin` | `undefined` | Binary path; omitted follows the resolution priority (see Installation) |
| `cwd` | `undefined` | Working directory of the serve process |
| `env` | `undefined` | Extra environment variables (**merged on top of `process.env`**, e.g. `ZHUYE_MODEL=mock`) |
| `handshakeTimeoutMs` | `30000` | `session_configured` handshake timeout |

### Utility functions (also exported from the package root)

- `resolveBundledBinary(): string | null` — resolves the
  platform sub-package's embedded binary path; returns `null` when
  not installed.
- `platformPackageName(platform?, arch?): string | null` — the
  sub-package name for the current platform (e.g.
  `zhuye-darwin-arm64`); returns `null` when not in the support
  matrix.

### Data types (all exported from the package root)

- `ToolOutput`: tool receipt `{content: ContentBlock[], is_error, metadata, elapsed_ms}`.
  For a successfully started foreground `bash`, `metadata.network_restriction`
  is `blocked_sockets`, `local_only`, or `unrestricted`; a failed launch does
  not claim installation.
- `ReadMcpResource` preserves resource order. Binary resources are JSON text
  blocks with `uri`, base64 `blob`, and optional `mimeType`; `metadata.content_descriptors`
  describes each item.
- MCP initialization and initial tool listing use `timeout_ms`; startup failure
  events currently report `will_retry: false`.
- `ContentBlock`: `{type: 'text', text: string}`.
- `RemoteToolSpec`: `{name, description, parameters (JSON Schema)}`.
- `Event`: `{id: string, msg: EventMsg}`; `id === ''` denotes a
  global event.
- `Op` / `EventMsg` / `UnknownEventMsg` / `Submission` /
  `ReviewDecision` / `EVENT_ID_NONE`: protocol types (see below),
  all with precise literal types.

## Event Reference

The full set of `ev.msg.type` variants (type definitions in
`src/protocol.ts`, one-to-one with the Rust `EventMsg`):

| Variant | Channel | Description |
|---|---|---|
| `session_configured` | global | Handshake (session_id / model / provider / policies) |
| `turn_started` / `turn_complete` | submission | Turn start/end; complete carries `status` and `usage` |
| `turn_aborted` | submission | Turn was interrupted, `reason ∈ user_interrupt/shutdown/error` |
| `agent_message` / `agent_message_delta` | submission | Final text / streaming delta |
| `thinking_delta` | submission | Thinking-stream delta |
| `token_count` | global | Usage statistics |
| `tool_call_begin` / `tool_call_args_delta` / `tool_call_output_delta` / `tool_call_end` | submission | The built-in tool call quartet; end carries `is_error` and the final output |
| `tool_execution_request` | (intercepted inside the SDK) | Remote tool request → local handler |
| `approval_request` / `permission_bubble` | global | Approval needed → respond with `approve()` |
| `ask_user_question` / `ask_user_input` | global | User answer needed → respond with the matching response Op via `submit()` |
| `mcp_elicitation` | global | an MCP server asks the user for input → reply with `mcp_elicitation_response` via `submit()` (no reply: cancelled after the TTL) |
| `permission_mode_changed` | global | Result of a permission mode switch |
| `context_compacted` | submission | Compaction finished |
| `plan_request` / `plan_ready` / `plan_approved` / `plan_rejected` / `plan_draft_updated` / `plan_step` | submission / global | Plan mode events |
| `error` / `stream_error` | submission | Errors (`error` is turned into an exception by `prompt()`) |
| `shutdown_complete` | submission | Shutdown finished |
| `submission_closed` | submission | That submission's channel is drained (termination marker for non-turn Ops) |
| `turn_rewound` / `config_reloaded` / `collab_*` / `mcp_server_*` / `mcp_tool_invoked` / `lsp_server_*` / `plugin_loaded` / `quota_exhausted` / `routing` | — | Remaining variants: see protocol.ts |
| `UnknownEventMsg` form | any | Unrecognized variants degrade (`type` keeps its original value), guaranteeing forward compatibility |

Parallel tool arguments from OpenAI-compatible streams, Responses and
Anthropic are attributed by `call_id`; the first tool's deltas are
sent in real time, and the remaining tools' deltas are sent in call
order when that model output ends.
Anthropic in-stream errors and Responses failed terminal states enter
the error or credential-switch path; when Responses hit the output
limit, continuation is attempted, and unfinished tool calls are not
executed.
When the tool arguments are not valid JSON, the SDK's host handler is
not called; the model receives a tool error and can continue.

## Error Handling Quick Reference

| Scenario | Behavior |
|---|---|
| Binary not found (spawn ENOENT) | The first `await` (submit/registerTool) throws `zhuye serve failed to start: ...`; the host is not crashed with an uncaught exception |
| Handshake timeout | Throws `zhuye serve handshake timed out after ...ms` |
| Model returns an error event | `prompt()` throws `Error: zhuye error: <code>: <message>`; handle it yourself in a `submit()` stream |
| Turn interrupted | `prompt()` throws `Error: zhuye turn aborted: ...`; a `submit()` stream sees `turn_aborted` |
| Disconnection before turn completion | `prompt()` throws `Error: zhuye stream ended before turn_complete`; no incomplete text is returned |
| Global event read timeout | `nextGlobalEvent` throws `zhuye nextGlobalEvent timed out ...` |
| Handler throws | Automatically converted into an `is_error=true` receipt; the turn is unaffected; the SDK is unaffected |
| Writes after `close()` | Silently dropped (the receipt has nowhere to go; the host is not crashed) |

## Environment Variables

| Variable | Purpose |
|----------|---------|
| `ZHUYE_BIN` | Path to the `zhuye` binary. Lower priority than `ZhuyeAgentOptions.bin` (an explicit pass wins), higher than the platform sub-package embedded binary and PATH. |
| `ZHUYE_MODEL` | Model spec (`mock/mock-1` for offline testing). |
| `ZHUYE_REMOTE_TOOL_TIMEOUT_SECS` | The limit for waiting on a client receipt for remote tools, default 120s. |

## Testing

```bash
npm install
npm run typecheck    # tsc --noEmit
npm test             # protocol-layer vitest (mock serve, offline)
```

The mock serve process is run directly on Node by an embedded test
script and does not depend on a real `zhuye` binary — fully
offline, no network / API key needed. Coverage: handshake, prompt
aggregation, remote tools, unknown-event degradation, pass-through of
the new `side_channel_*` variants and the query Op
(`query_side_channels` → status snapshot + `submission_closed`
termination).

End-to-end tests (`tests/sdk-ts/real-bin.test.mjs`, the K1–K10 real
binary binding layer — K10 is the real-serve response to the
side-channel query Op) run in the repo CI: after building the binary,
execute `./tests/run_all.sh sdk-ts` (locally you can also drive it
directly with `ZHUYE_BIN=... node --test
tests/sdk-ts/real-bin.test.mjs`).

## Protocol & Version Compatibility

- The wire protocol between the SDK and `zhuye serve` is in
  [`../PROTOCOL.md`](../PROTOCOL.md) (online:
  [GitHub](https://github.com/Asketisch/ZhuYe/blob/main/sdks/PROTOCOL.md)).
- Protocol v0 is frozen: new Op / EventMsg variants from the Rust
  core are additive; the SDK degrades unrecognized variants to
  `UnknownEventMsg` and passes them through verbatim without
  crashing.
- The SDK version is managed together with the Rust workspace
  version (the release pipeline keeps four places consistent).

## More Documentation

Beyond SDK usage, the repo has a full documentation set (the following
are absolute GitHub links — npm pages do not resolve in-repo relative
paths):

| Document | Content |
|---|---|
| [Capability overview & boundaries](https://github.com/Asketisch/ZhuYe/blob/main/docs/features.md) | What the engine can / cannot do |
| [Configuration & environment variables](https://github.com/Asketisch/ZhuYe/blob/main/docs/configuration.md) | Every `config.toml` entry, hot reload, env list |
| [SDK integration in depth](https://github.com/Asketisch/ZhuYe/blob/main/docs/sdk.md) | Session server, remote tool protocol, offline debugging |
| [Tools & hooks](https://github.com/Asketisch/ZhuYe/blob/main/docs/tools-and-hooks.md) | Built-in tools, hook events, permission modes, sandbox |
| [Wire protocol](https://github.com/Asketisch/ZhuYe/blob/main/sdks/PROTOCOL.md) | The serve JSONL protocol definition (frozen v0) |

## License

MIT. Repository: <https://github.com/Asketisch/ZhuYe>.
