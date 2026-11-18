# `zhuye-discussion`

<p align="center">  <a href="README.md">中文</a> · <b>English</b>  </p>

> Shipped in M9 (2026-06-22); v0.2.3 finished the real-LLM wiring (round
> marking + AgentTurn emit + lib re-export + docs de-hedging). The
> ZhuYe multi-agent discussion subsystem.

## Responsibility

Wraps the multi-agent discussion state machine into a
`DiscussionOrchestrator`: given a `DiscussionConfig` (`mode` /
`participants` / `topic` / `consensus_window` / `max_rounds`) plus a
shared `MessageBus`, it runs the entire discussion, emits
`OrchestratorEvent` observation points, and returns
`DiscussionResult { Consensus | NoConsensus | Finished }`.

Since v0.2.3: every agent in every round calls the real LLM through
`prompt_for_closure` (via `SubAgentFactory::spawn` + draining the
`TurnHandle`); `run_noop` is kept for tests / offline fallback.

## Quickstart (v0.2.3 LLM Path)

```rust
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use parking_lot::Mutex;
use zhuye::{DiscussionConfig, DiscussionId, DiscussionResult, SubAgentFactory, SubAgentSpec, DataTransferConfig};
use zhuye_discussion::llm::{build_context, prompt_for_closure};
use zhuye_discussion::message_bus::MessageBus;
use zhuye_discussion::models::{AgentId, DiscussionMode};
use zhuye_discussion::tool::DiscussionToolSet;
use zhuye_discussion::{AgentSection, DiscussionOrchestrator, OrchestratorEvent};
use zhuye_llm::{ModelRegistry, SharedModelRegistry};
use zhuye_protocol::ThreadId;
use zhuye_tools::ToolRegistry;
use tokio_util::sync::CancellationToken;

# async fn run() {
let config = DiscussionConfig {
    mode: DiscussionMode::Sequential,
    participants: vec![AgentId("advocate".into()), AgentId("skeptic".into())],
    topic: "Async vs sync?".into(),
    consensus_window: 1,
    max_rounds: 5,
    mailbox_capacity: 32,
};
let bus = MessageBus::new(DiscussionId::new(), config.participants.clone(), 32);

// build the LLM registry + parent tool registry + SubAgentFactory
let registry: SharedModelRegistry = Arc::new(ModelRegistry::new());
// registry.register("openai", Arc::new(OpenAIClient::new(...)));  // or anthropic
let finished = Arc::new(Mutex::new(false));
// since v0.2.3: the shared round_counter feeds every DiscussionToolSet + the orchestrator;
// the runtime stores round() each round, and comm_tools load it at execute() time
// to write DiscussionMessage.round.
let round_counter = Arc::new(AtomicU32::new(0));
let parent_tools = Arc::new(ToolRegistry::default());
for p in &config.participants {
    let set = DiscussionToolSet::new(p.clone(), bus.clone(), finished.clone(), round_counter.clone());
    set.verify()?;
    for name in set.tool_names() {
        if let Some(t) = set.registry.get(&name) { parent_tools.register(t); }
    }
}
let factory = Arc::new(SubAgentFactory::new(
    ThreadId::new(), "openai/gpt-4o", registry, parent_tools,
    CancellationToken::new(), None));

// build the LlmContext + prompt_for_closure
let agents: Vec<AgentSection> = config.participants.iter().map(|p| AgentSection {
    role: p.0.clone(),
    system_prompt: format!("You are {p} of the discussion."),
    allowed_tools: vec!["send_message".into(), "read_messages".into(), "finish_discussion".into()],
}).collect();
let ctx = build_context(factory, config.topic.clone(), &config.participants, &agents)?;

// run it (passing the same round_counter)
let orch = DiscussionOrchestrator::with_round_counter(
    config, bus.clone(), Some(factory), CancellationToken::new(), None, round_counter)?;
let result: DiscussionResult = orch.run(
    prompt_for_closure(ctx, bus),
    |event| { let _ = event; },
).await?;
# Ok::<(), Box<dyn std::error::Error>>(())
# }
```

CLI equivalent (`try_build_llm_orchestrator` takes over the wiring
automatically):

```bash
zhuye discussion run -c crates/zhuye-discussion/examples/discussion.toml
```

When the CLI detects an available provider
(`OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `~/.zhuye/config.toml`) it
runs the real LLM; otherwise it falls back to `run_noop` (friendly for
offline / CI).

## Module Graph

```
models        — core data types (DiscussionId / MessageId / AgentId / DiscussionMessage / DiscussionConfig / DiscussionResult)
  ↓
message_bus   — in-process mpsc routing + transcript persistence (MessageBus / AgentMailbox / BusError)
  ↓
comm_tools    — the 3 communication tools visible to the LLM (SendMessageTool / ReadMessagesTool / FinishDiscussionTool)
  ↓
tool          — DiscussionToolSet::new registers the 3 tools under ToolSource::Builtin
  ↓
runtime       — sequential / concurrent scheduling loop (DiscussionRuntime / StepOutcome / RuntimeError)
  ↓
orchestrator  — the main state machine (DiscussionOrchestrator / OrchestratorEvent / OrchestratorError)
  ↓
llm           — real-LLM integration layer (LlmContext / build_context / prompt_for_closure / LlmError)
  ↓
cli           — the `zhuye discussion run` CLI entry (DiscussionToml + try_build_llm_orchestrator auto-wiring)
```

## Key Design

- `MessageBus` shares an internal `Arc<Inner>`; borrowing `&MessageBus`
  across `await` is `Send` (avoids the `parking_lot::MutexGuard`
  `!Send`-across-await problem).
- sequential vs concurrent: `run_sequential` awaits serially /
  `run_concurrent` runs in parallel with `tokio::task::JoinSet`.
- consensus detection: scans the transcript; when every participant has
  sent a `MessageKind::Consensus` within the recent
  `consensus_window` rounds, the whole group reaches consensus.
- early finish: any `MessageKind::Finish` message (triggered by
  `FinishDiscussionTool`) exits immediately; in concurrent mode
  `set.abort_all()` cancels the remaining tasks.
- **v0.2.3 LLM wiring**: `prompt_for_closure` hooks into
  `SubAgentFactory::spawn` + `SpawnedChild::collect_result`; the LLM
  expresses intent through `send_message` / `finish_discussion` tool
  calls, and the assistant text obtained from `collect_result` is
  discarded (avoids double-writing the transcript).
- **v0.2.3 round marking**: the runtime `store(round, SeqCst)`s into
  the shared `Arc<AtomicU32>` each round; `SendMessageTool` /
  `FinishDiscussionTool` `load` it at `execute()` and write it into
  `DiscussionMessage.round`, so transcript messages carry the correct
  round (cross-round consensus detection with `consensus_window > 1`
  depends on this).
- **v0.2.3 AgentTurn emit**: `OrchestratorEvent::AgentTurn { agent,
  round }` is emitted after every spawn+drain; sequential mode forwards
  directly, concurrent mode aggregates via an `Arc<Mutex<Vec>>` bridge.
- the protocol does not expose discussion state (`EventMsg::Collab*` —
  12 variants, not implemented in v0); state is fully consumed inside
  the crate, and the only externally visible artifact is the
  `RolloutRecord::DiscussionTranscript` persistence.

## Known Limitations (deferred to v0.2.4 / v0.3.x / v1)

- **`SubAgentFactory::MAX_DEPTH = 3`** bounds the **cumulative spawn
  count** (not concurrent in-flight):
  - `Sequential` mode: any round is fine (depth is always = 1).
  - `Concurrent` mode: N participants → depth jumps to N in round 1;
    the 3-agent `discussion.toml` example works for ~1 round.
  - v0.3.x plan: `fetch_sub(1)` at the end of
    `SpawnedChild::collect_result` so depth becomes the "concurrent
    in-flight cap".
- **`DiscussionMessage::token_usage` is currently always
  `Default::default()`**: left as a billing reservation in M9; v0.2.4
  plans `collect_result_with_usage()` on `SpawnedChild` + injecting
  tokens into the `send_message` tool-call context via
  `build_context`.
- `zhuye discussion ls` is a stub for now; v0.2.4 will hook it to
  `zhuye-rollout::list_sessions`.
- invisible to the TUI: not exposed by the protocol; v0.2.4 adds the
  three `EventMsg::Collab*` variants.
- `RolloutRecord::DiscussionTranscript` does not write per-agent
  separate transcripts; v0.2.4 adds a `Vec<ThreadId> agents` field.

## Public Entry Points

Top-level `zhuye` lib re-exports (expanded to 12 since v0.2.3):
- core facade (M9): `DiscussionConfig` / `DiscussionId` /
  `DiscussionOrchestrator` / `DiscussionResult`
- advanced API (v0.2.3): `AgentId` / `AgentSection` / `DiscussionMode` /
  `MessageBus` / `MessageId` / `MessageKind` / `OrchestratorEvent` /
  `build_context` / `prompt_for_closure`
- related: `SubAgentFactory` / `SubAgentSpec` / `DataTransferConfig`
  (M5 re-exports)

`comm_tools` / `runtime` / `tool` are left for advanced users to take
directly from `zhuye_discussion::*`.
