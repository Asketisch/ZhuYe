# goal-queue plugin

<p align="center">  <a href="README.md">中文</a> · <b>English</b>  </p>

The delivery plugin for goal mode + queue mode (v1.10 feature 2).

## Features

- **goal (goal mode)**: the `goal` tool (`enter` / `exit` / `status`).
  Submits `Op::EnterGoalMode` / `Op::ExitGoalMode` over the submission
  channel, reusing all engine semantics: per-turn self-verification by
  an independent LLM, an optional verify command with AND conditions,
  the 3-strike block determination, and the token budget.
- **queue (queue mode)**: the `queue` tool (`add` / `list` / `remove` /
  `clear` / `start` / `pause` / `status`). A FIFO task queue consumed
  serially by a driver: after the current task's turn ends
  (TurnComplete), the next task is automatically injected as a new
  turn. Persisted to `~/.zhuye/queue/<session>.json`; unfinished tasks
  are automatically re-queued after a restart.

## Install / enable

```bash
zhuye plugin install plugins/goal-queue --scope user
zhuye plugin enable goal-queue@inline
```

Once enabled, headless (`zhuye exec`) / serve / SDK sessions register
the `goal` / `queue` tools and start the queue driver; the slash
commands `/goal-queue:goal` / `/goal-queue:queue` guide the model to
use these two tools.

## Design notes

- The engine path for goal (the TUI's `/goal` slash → Op) is unaffected
  by the plugin; the plugin simply gives the headless / SDK side's
  model a tool entry point.
- The queue completion signal is event-driven: the driver subscribes to
  the thread event stream, and on receiving `TurnComplete` settles the
  current active task (`Success` → done, otherwise failed with a note)
  and immediately tries to dequeue the next one.
