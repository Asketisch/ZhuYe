# Plan Mode Design

<p align="center">  <a href="PLAN_MODE.md">中文</a> · <b>English</b>  </p>

> This document describes the Plan mode protocol and implementation
> contract of ZhuYe. Related source:
> `zhuye-tools/src/builtins/plan/enter_plan_mode.rs` /
> `exit_plan_mode.rs` / `plan_write.rs`,
> `zhuye-hooks/src/builtins/plan_mode_gate.rs`,
> `zhuye-protocol/src/event_msg/plan.rs`,
> `zhuye-core/src/submission_loop.rs` (Op::PlanApproval dispatch).

## 1. Purpose

When a task is complex or wide-impact, the agent first does
**read-only research** and produces a structured plan; only after user
approval does it enter the execution phase. Plan mode is the runtime
presentation of `PermissionMode::Plan`: in this mode write tools are
blanket-denied, and the agent can only read, search, and write the plan
itself.

## 2. Entry Paths (Three, Converging to the Same State)

| Path | Trigger | Behavior |
|---|---|---|
| Tool `EnterPlanMode` | agent's own judgment | only **requests**: emits `EventMsg::PlanRequest` without switching modes; after the user presses 1 (approve) in the modal, `submission_loop` switches the permission mode to `Plan` |
| `zhuye exec --plan-mode` | CLI flag | enters Plan mode at startup (headless single-turn scenario) |
| TUI `/plan` slash | user | equivalent to the exec flag, for interactive sessions |

Defensive constraint: while already in Plan mode, `PlanModeGate` denies
`EnterPlanMode` (a repeat call would only produce a pointless modal);
the `tool_exec` node has a fallback too — if it detects Plan mode is
already active it skips dispatching the PlanRequest.

## 3. Tool Surface in Plan Mode

`PlanModeGate` (a PreToolUse hook) uses a **conservative allowlist**:

- allowed by default: `read` / `grep` / `glob` / `echo` + the Plan
  control tools (`ExitPlanMode` / `PlanWrite`);
- **`EnterPlanMode` is deliberately NOT on the allowlist** — calling it
  while in Plan mode is denied (the reason notes you are already in
  Plan mode);
- everything else is denied, with `Deny.reason` carrying the tool name
  (observable, so the agent can adjust itself);
- the allowlist can be injected with a custom one (tests / advanced
  scenarios, `PlanModeGate::with_allowlist`).

The allowlist is defined by `default_read_only_allowlist()` in
`plan_mode_gate.rs`.

## 4. The Plan Body and the Approval Protocol

- `PlanWrite`: the agent writes the structured plan (steps + status),
  producing a `PlanStep` event stream (the TUI renders progress);
- `PlanCompletionHook` is a **Stop hook**: before allowing the turn to
  end with the `AgentDecision` reason it requires explicit verification
  (under strict it injects a reminder and Denies, at most 3 times) — it
  is **not** a plan-step-completion tracker;
- user decisions go through `Op::PlanApproval { id, choice }`, paired
  with `PlanRequestEvent.id` and sent back to the waiter. Options:
  1 = approve, exit Plan mode and enter execution; 2 = reject
  (`PlanRejected`, the agent revises the plan); 3 = abort;
- the `ExitPlanMode` tool is the agent-side closer: it declares the
  plan ready and waits for approval.

## 5. Headless Semantics (`zhuye exec --plan-mode`)

A single turn has no interactive modal: the agent runs the read-only
research straight through in Plan mode, and the turn ends after it
produces the plan text. The convention is **exit code 122 = plan
generation failed, retry** (see the doc-comment of
`ExecArgs::plan_mode`; for scripted callers to key off).

## 6. Plan Mode vs `PermissionMode::Plan`

The force of `--plan-mode` comes from `PlanModeGate` (a PreToolUse hook
that blanket-denies non-allowlisted tools), **not** from
`PermissionMode::Plan` — the latter only tells the approval layer which
mode is active; standing alone it would still let write tools through.
All three entry points must do both things simultaneously:

| Entry point | permission mode | PlanModeGate |
|---|---|---|
| `zhuye exec` (normal) | `bootstrap_normal` in `headless.rs` | same as above |
| `zhuye exec --resume` | `bootstrap_resumed` in `headless.rs` | same as above |
| `ZhuyeBuilder::plan_mode(true)` / zhuye-py | `builder.rs` | same as above |

## 7. Design Principles (Why It Is Cut This Way)

- **request and switch are separated**: `EnterPlanMode` only emits the
  event; the switch is driven by user approval — tightening permissions
  must be confirmed by a human, and the agent cannot grant itself
  privileges (true in both directions: entering Plan is tightening,
  exiting Plan is loosening, and both need the user's involvement);
- **hook rather than hard-coding**: the tool-surface trimming lives in
  the `PlanModeGate` hook layer; the permission mode itself stays pure
  data — other modes (e.g. a future AcceptEdits variant) can reuse the
  same mechanism;
- **protocol pairing**: the waiter mechanism of
  `PlanRequest.id` ↔ `Op::PlanApproval.id` is isomorphic to
  `ask_user_question` / the approval gates — client implementations
  add zero new concepts.
