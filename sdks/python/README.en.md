# zhuye (Python SDK)

<p align="center">  [中文](README.md) · **English**  </p>

The official SDK for driving [ZhuYe Agent](https://github.com/Asketisch/ZhuYe)
directly from Python projects. Under the hood it spawns a
`zhuye serve` subprocess and speaks the JSONL stdio protocol:
**works out of the box** (the platform wheel embeds the
`zhuye` binary for the corresponding platform), **zero
runtime third-party dependencies** (stdlib only), **protocol v0
frozen** (new variants from the Rust core degrade automatically and
never break old clients).

- Python ≥ 3.10, zero dependencies, ships `py.typed` type
  annotations
- Persistent session: one process = one AgentThread, multiple turns
  share context
- Host-language functions can be registered directly as tools the
  LLM can call (remote tool protocol)
- Supports streaming, approvals, interruption, global events,
  permission modes, plan / goal mode, and runtime rescanning of the
  skills directory (`rescan_extensions`)

## Installation

```bash
pip install zhuye
```

Works out of the box: PyPI provides platform wheels (macOS arm64 /
x86_64, Linux x86_64 / aarch64 glibc, Windows x86_64) with the
`zhuye` binary for the corresponding platform embedded; the SDK
resolves it first — no binary to install yourself, no environment
variables to configure.

**Binary resolution priority** (identical in both official SDKs):

```
options.bin  →  ZHUYE_BIN  →  wheel-embedded binary  →  zhuye on PATH
```

When the current platform is not in the support matrix (e.g.
musl/Alpine), it falls back to PATH: download an installer from
[GitHub Releases](https://github.com/Asketisch/ZhuYe/releases), or
build from source with `make build` to produce
`target/release/zhuye` and point `ZHUYE_BIN` at it.

## Quick Start

```python
from zhuye import ZhuyeAgent

agent = ZhuyeAgent.spawn()          # returns only after the session_configured handshake completes
print(agent.prompt("用一句话介绍你自己"))
agent.close()                        # idempotent: Shutdown → wait for exit → force-kill fallback
```

By default it uses the model configuration from environment
variables; for an offline experience use the mock model:
`ZHUYE_MODEL=mock`.

## Core Concepts

```
Python process                        zhuye serve subprocess
┌─────────────────────┐    JSONL stdio    ┌──────────────────────┐
│ ZhuyeAgent          │ ── Submission ──▶ │ AgentThread          │
│  reader thread      │ ◀──── Event ───── │  (StateGraph turn)   │
│  ├ id="" global q   │                   │                      │
│  └ routed per ──────│                   │                      │
│    submission id    │                   │                      │
└─────────────────────┘                   └──────────────────────┘
```

- **Persistent session**: the serve process started by `spawn()`
  stays alive until `close()`, and multiple `submit()` calls share
  the full conversation context.
- **Event dual-channel**: events come in two flavors —
  - *submission events*: keyed on the submission id and consumed
    exclusively by the iterator returned from `submit()` (streaming
    deltas, turn_complete, tool_call_end, etc.);
  - *global events* (`id=""`): not belonging to any submission
    (token_count, permission_mode_changed, the handshake
    session_configured, etc.), read via `next_global_event()`.
  The two channels never mix. Remote tool requests
  (`tool_execution_request`) are global events on the wire, but the
  SDK intercepts them internally and dispatches to the local handler;
  they never appear in either queue.
- **Iterator termination**: `turn_complete` / `turn_aborted` /
  `shutdown_complete` / `submission_closed` are terminal events
  and the iterator ends naturally after them; a connection drop (EOF)
  also wakes and ends all iterators — nothing hangs forever.

## Usage Guide

### Streaming output (submit + delta)

```python
from zhuye import ZhuyeAgent

agent = ZhuyeAgent.spawn()
for ev in agent.submit("讲个笑话"):
    msg = ev["msg"]
    if msg["type"] == "agent_message_delta":
        print(msg["delta"], end="", flush=True)
    elif msg["type"] == "turn_complete":
        print(f"\n[turn ended] usage {msg['usage']}")
agent.close()
```

`submit(text)` wraps the text into a `user_input` Op internally
and returns the event iterator **exclusive to that submission**; when
the iterator ends, this turn is over.

### Get the full text in one step (prompt)

```python
answer = agent.prompt("北京天气如何?")
```

Aggregates all deltas; `turn_complete` returns normally, while
`turn_aborted` / `error` events raise `RuntimeError` (partial
text is never silently returned as a complete answer).
This synchronous interface currently has no `prompt` timeout
parameter; TypeScript's `timeoutMs` only stops the local wait and
does not automatically interrupt the server-side turn.
When a model stream errors out, if text, thinking or tool-call events
have already been emitted, this turn is not automatically retried;
the emitted text is attempted to be written to the session record
before the error event, and you can continue in the next turn.

### Custom tools (host function → LLM tool)

```python
from zhuye import ZhuyeAgent, ContentBlock, ToolOutput

def get_weather(args: dict) -> ToolOutput:
    return ToolOutput(
        content=[ContentBlock(type="text", text=f"{args['city']} 晴 26℃")],
        is_error=False,
        metadata={},
        elapsed_ms=0,
    )

agent = ZhuyeAgent.spawn()
agent.register_tool(
    name="get_weather",
    description="查询指定城市的天气",
    parameters={                       # JSON Schema
        "type": "object",
        "properties": {"city": {"type": "string"}},
        "required": ["city"],
    },
    handler=get_weather,
)
print(agent.prompt("上海天气怎么样?"))
```

Behavioral contract (protocol §5):

- When the model calls the tool, serve emits
  `tool_execution_request` and the SDK runs the handler on a
  **separate thread** and sends the receipt back; the reader thread is
  not blocked, and you can call `prompt()` / `submit()` again
  inside the handler.
- Handler throws → the SDK automatically converts it into a text
  receipt with `is_error=True`; the LLM sees the error message and
  the turn does not hang.
- An unregistered tool gets called → an error text receipt is sent
  back (this also does not hang).
- The receipt wait limit is controlled by the environment variable
  `ZHUYE_REMOTE_TOOL_TIMEOUT_SECS` (default 120s); on timeout it
  ends with an error.

### Multiple turns & concurrency

```python
# Multiple turns sharing context
agent.prompt("我叫小明")
agent.prompt("我叫什么?")            # the model can answer "小明"

# Concurrent submissions: each submit has its own id and iterator; thread-safe
import threading
t1 = threading.Thread(target=lambda: [print(e) for e in agent.submit("问题一")])
t2 = threading.Thread(target=lambda: [print(e) for e in agent.submit("问题二")])
t1.start(); t2.start(); t1.join(); t2.join()
```

### Interruption & approval

Interrupting a parent turn cancels its in-flight subagents and the
server-side wait for remote tools, but does not force-stop a Python
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

```python
# Interrupt the current turn (that turn subsequently ends with turn_aborted)
agent.interrupt()

# Approval: after receiving the global event approval_request, return a decision
import json
event = agent.next_global_event(timeout_secs=30)
if event and event["msg"]["type"] == "approval_request":
    req = event["msg"]
    agent.approve(req["request_id"], "approve")            # or "approve_for_session"
    # Deny: agent.approve(req["request_id"], {"deny": {"reason": "太危险"}})
```

`decision` passes the wire form through verbatim, with no shape
conversion: `"approve"` / `"approve_for_session"`, or
`{"deny": {"reason": "..."}}` (the legacy
`{"type":"approve"}` cannot be deserialized by the server — do not
use it). Hook approval uses the same decision form (serve dispatches
it by event kind).

### Global events

```python
event = agent.next_global_event(timeout_secs=5)   # blocking read of one; raises queue.Empty on timeout
```

- The handshake `session_configured` itself is in the global queue
  — it's usually the first one;
- After stdout EOF / `close()` it returns `None`; repeated reads
  are safe;
- Completely independent of submission iterators; unknown variants
  are returned as-is.

### Session control Ops (submit_op)

`submit_op(op)` sends any protocol Op and returns an event iterator
(non-turn Ops finish with `submission_closed`). Full Op set (26
kinds, one-to-one with the Rust protocol; types in `zhuye.Op`):

| Category | Op | Key fields |
|---|---|---|
| conversation | `user_input` | `items` (text/image), optional `thread_settings{model, approval_policy, sandbox_policy, max_tool_concurrency}` |
| conversation | `compact` / `rewind` | rewind optionally with `to_turn_id` |
| control | `interrupt` / `shutdown` | interrupt optionally with `child_id` |
| approval | `tool_approval` / `hook_approval` | `id` + `decision` |
| plan | `enter_plan_mode` / `exit_plan_mode` / `plan_approval` | `plan_approval.choice ∈ auto_mode/manual_approve/revise` |
| interaction | `ask_user_question_response` / `ask_user_input_response` | respond to the global events ask_user_question / ask_user_input |
| state | `set_effort` / `set_permission_mode` / `cycle_permission_mode` | mode ∈ auto/prompt/plan/deny/bubble |
| prompt | `prompt_command` | `action{type: list\|switch}`; query / rescan the prompt layer, answered with `prompt_list` / `prompt_switched` |
| steering | `steer` | `items` (optional `priority ∈ now/attachment`, default attachment); feed mid-turn without interrupting the in-flight turn |
| subagent | `query_subagents` | optional `child_id`; answered with a `subagent_status` snapshot |
| goal | `enter_goal_mode` / `exit_goal_mode` | goal + optional verify_command / token_budget |
| side-channel | `start_side_channel` / `cancel_side_channel` / `query_side_channels` | user-driven concurrent agent: start (`agent`+`prompt`) / cancel (`id`) / query (optional `id`, returns `side_channel_status`) |
| tools | `register_tools` / `tool_execution_response` | usually use the `register_tool()` wrapper; no need to hand-write |
| extensions | `rescan_extensions` | **new in v1.x**: runtime rescan of the skills directory (`.zhuye/skills`, workspace + user-level; plugin injection unaffected); returns `extensions_rescanned` (skills count, commands always false) |
| security | `query_guard_stats` | **new in F01**: query the prompt_guard verdict statistics (four counters: definition-side / output-side block and review); returns `guard_stats` |

Examples: switching to plan mode, submitting with thread_settings

```python
evs = list(agent.submit_op({"type": "set_permission_mode", "mode": "plan"}))
# side-channel query (WP9): a real serve returns a status snapshot; the iterator finishes with submission_closed
evs = list(agent.submit_op({"type": "query_side_channels"}))
evs = list(agent.submit_op({
    "type": "user_input",
    "items": [{"type": "text", "text": "调研一下"}],
    "thread_settings": {"approval_policy": "deny"},
}))
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

| Method | Description |
|---|---|
| `ZhuyeAgent.spawn(options=None) -> ZhuyeAgent` | Starts `zhuye serve` and blocks until the handshake completes; raises `RuntimeError` if the binary is not found / the handshake times out |
| `ZhuyeAgent.from_popen(proc, handshake_timeout_secs=30)` | Test injection: use an existing `Popen` (stdin/stdout must be pipes) instead of spawning; this path does not forward stderr |
| `register_tool(name, description, parameters, handler)` | Registers a remote tool, callable at any time; silently accepted, errors reported asynchronously via error events |
| `submit(text) -> Iterator[dict]` | Submits user text; returns the event iterator for that submission |
| `submit_op(op, *, submission_id=None) -> Iterator[dict]` | Submits any Op; the submission id can be specified explicitly |
| `prompt(text) -> str` | Aggregates deltas and returns the full text; raises `RuntimeError` on `turn_aborted` / `error` or a disconnection before the terminal state |
| `interrupt(child_id=None)` | Interrupts an in-flight turn |
| `approve(approval_id, decision)` | Returns an approval decision (wire form passed through) |
| `next_global_event(timeout_secs=None) -> dict | None` | Reads a global event; raises `queue.Empty` on timeout, returns `None` after EOF |
| `close()` | Idempotent close: Shutdown → wait 2s → force-kill; afterwards all writes are silently dropped and iterators finish |

### `ZhuyeAgentOptions`

| Field | Default | Description |
|---|---|---|
| `bin` | `None` | Binary path; omitted follows the resolution priority (see Installation) |
| `cwd` | `None` | Working directory of the serve process |
| `env` | `None` | Extra environment variables (merged on top of the parent environment, e.g. `ZHUYE_MODEL=mock`) |
| `handshake_timeout_secs` | `30.0` | `session_configured` handshake timeout |

### Data types (`from zhuye import ...`)

- `ToolOutput`: tool receipt
  `{content: [ContentBlock], is_error, metadata, elapsed_ms}`
  (all fields optional; idiomatically you pass all of them).
- `ContentBlock`: `{type: "text", text: str}`.
- `RemoteToolSpec`: `{name, description, parameters (JSON Schema)}`.
- `Event`: `{id: str, msg: dict}`; `id=""` denotes a global
  event.
- `Op` / `EventMsg` / `UnknownEventMsg`: protocol TypedDicts
  (see below).
- `Submission` / `EVENT_ID_NONE`.

## Event Reference

`msg["type"]` values in iterators and the global queue (full types in
`zhuye/protocol.py`, one-to-one with the Rust `EventMsg`):

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
| `ask_user_question` / `ask_user_input` | global | User answer needed → respond with the matching response Op via `submit_op` |
| `permission_mode_changed` | global | Result of a permission mode switch |
| `context_compacted` | submission | Compaction finished |
| `plan_request` / `plan_ready` / `plan_approved` / `plan_rejected` / `plan_draft_updated` / `plan_step` | submission / global | Plan mode events |
| `error` / `stream_error` | submission | Errors (`error` is turned into an exception by `prompt()`) |
| `shutdown_complete` | submission | Shutdown finished |
| `side_channel_started` / `side_channel_finished` | submission / global | side-channel agent (WP9): user-initiated concurrent agent; `finished` has the three states done / cancelled / error |
| `side_channel_status` | submission | The response to `submit_op({"type":"query_side_channels"})` (channels snapshot array) |
| `submission_closed` | submission | That submission's channel is drained (termination marker for non-turn Ops) |
| `turn_rewound` / `config_reloaded` / `collab_*` / `mcp_server_*` / `mcp_tool_invoked` / `lsp_server_*` / `plugin_loaded` / `quota_exhausted` / `routing` | — | Remaining variants: see protocol.py |
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
| Binary not found | `spawn()` raises `RuntimeError` (resolution failed by priority) |
| Handshake timeout / instant process exit | `spawn()` raises `RuntimeError` (it calls `close()` first on timeout) |
| Model returns an error event | `prompt()` raises `RuntimeError: zhuye error: <code>: <message>`; handle it yourself in a `submit()` stream |
| Turn interrupted | `prompt()` raises `RuntimeError: zhuye turn aborted: ...`; a `submit()` stream sees `turn_aborted` |
| Disconnection before turn completion | `prompt()` raises `RuntimeError: zhuye stream ended before turn_complete`; no incomplete text is returned |
| Global event read timeout | `next_global_event` raises `queue.Empty` |
| Handler throws | Automatically converted into an `is_error=True` receipt; the turn is unaffected; the SDK is unaffected |
| Writes after `close()` | Silently dropped (the receipt has nowhere to go; the host is not crashed) |

## Environment Variables

| Variable | Purpose |
|----------|---------|
| `ZHUYE_BIN` | Path to the `zhuye` binary. Lower priority than `ZhuyeAgentOptions.bin` (an explicit pass wins), higher than the wheel-embedded binary and PATH. |
| `ZHUYE_MODEL` | Model spec (`mock/mock-1` for offline testing). |
| `ZHUYE_REMOTE_TOOL_TIMEOUT_SECS` | The limit for waiting on a client receipt for remote tools, default 120s. |

## Testing

```bash
python3 -m pip install pytest
python3 -m pytest -ra
```

The mock serve process runs on Node (Node ≥ 18) and does not depend on
a real `zhuye` binary — the tests are fully offline and need no
network / API key. When developing in the repo, run directly from the
source tree (`python -m pytest` puts this directory on
`sys.path`); no build or install required. The packaging backend is
maturin (platform wheels are produced by CI with the binary
embedded); a local build needs the Rust toolchain.

**Two-layer test structure**:

- This directory (`sdks/python/tests/`) is the **mock layer** —
  protocol round-trips and degradation semantics, covering:
  handshake, multi-turn, remote tools, approval, unknown-variant
  degradation, pass-through of the new `side_channel_*` variants
  and the query Op (`query_side_channels` → status snapshot +
  `submission_closed` termination);
- The repo-level `tests/sdk-python/test_sdk_real_bin.py` is the
  **real binary binding layer** (K1–K10), driving a real
  `zhuye serve` and asserting the SDK client's own behavior — K10
  is the real-serve response to the side-channel query Op. Run with:
  `./tests/run_all.sh sdk-python` (the binary must be built first;
  `ZHUYE_BIN` can override it).

End-to-end tests (integration against a real `zhuye serve`
subprocess) run in the repo CI: set `ZHUYE_BIN` and execute
`./tests/run_all.sh sdk-python`.

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
are absolute GitHub links — PyPI pages do not resolve in-repo
relative paths):

| Document | Content |
|---|---|
| [Capability overview & boundaries](https://github.com/Asketisch/ZhuYe/blob/main/docs/features.md) | What the engine can / cannot do |
| [Configuration & environment variables](https://github.com/Asketisch/ZhuYe/blob/main/docs/configuration.md) | Every `config.toml` entry, hot reload, env list |
| [SDK integration in depth](https://github.com/Asketisch/ZhuYe/blob/main/docs/sdk.md) | Session server, remote tool protocol, offline debugging |
| [Tools & hooks](https://github.com/Asketisch/ZhuYe/blob/main/docs/tools-and-hooks.md) | Built-in tools, hook events, permission modes, sandbox |
| [Wire protocol](https://github.com/Asketisch/ZhuYe/blob/main/sdks/PROTOCOL.md) | The serve JSONL protocol definition (frozen v0) |

## License

MIT. Repository: <https://github.com/Asketisch/ZhuYe>.
