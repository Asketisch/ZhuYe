# Prompt Library (v1.6): Externalized + Per-Model Matching

<p align="center">  <a href="prompts.md">中文</a> · <b>English</b>  </p>

All of ZhuYe's LLM prompts have been externalized to Markdown since
v1.6, and are matched dynamically against the **current model** — when
the same agent switches LLMs (e.g. claude / gpt / gemini / deepseek),
each model can load its own tuned prompt version so it performs best.

## Directory Layout

```text
~/.zhuye/prompts/          # user global layer (applies to all workspaces)
<workspace>/.zhuye/prompts/  # workspace layer (same-named files override the global layer)
├── default.md               # fallback layer: wording used when no model file matches
├── claude.md                # vendor layer: overlaid when the spec prefix is claude/
├── gpt.md
├── gemini.md
├── deepseek.md
├── kimi.md                  # vendor layers consolidated in v1.6.1 below
├── qwen.md
├── grok.md
├── glm.md
├── minimax.md
└── claude-sonnet-4-5.md     # exact layer: overlaid when the model segment matches exactly
```

Each file uses `## key` sections, one prompt per section. **You only
need to write the keys you want to override**; missing keys fall back
to the compile-time embedded defaults (works out of the box, tune
progressively).

### Built-in Vendor Layers (consolidated in v1.6.1)

Nine vendor layers with real override wording are embedded at compile
time, distilled from each vendor's official agent system prompts
(sources and licenses in the header comments of each
`prompts/<vendor>.md` file):

| Vendor layer | Source | Notes |
|--------|------|------|
| claude | Claude Sonnet 5.5 (leaks) | Harness / context management / subagents (Explore/Plan) wording |
| gpt | GPT-5.6 (leaks) | edit constraints / autonomy boundaries / destructive-action rules |
| gemini | Gemini CLI (leaks) | context efficiency / tool parallelism & confirmation protocol |
| kimi | Kimi K3 (leaks) | communication / search & time-sensitive info rules |
| qwen | Qwen3.8 Max (leaks) | function-calling format / time reference baseline |
| grok | Grok 4.7 CLI (leaks) | dangerous actions / work strategy / communication rules |
| glm | Z.ai Code (tools repo) | working rules / UI & code style; the GLM model itself has no system prompt |
| minimax | MiniMax M2.5 (leaks) | plan before acting / web project verification / delivery checklist |
| deepseek | (no body source) | source only has the tool JSON skeleton, the layer is empty, everything falls back to default |

The embedded vendor layers take effect automatically via the matching
chain: both `anthropic/claude-*` and bare `claude-*` hit
`claude.md`; both `moonshot/kimi-*` and bare `kimi-*` hit `kimi.md`;
and so on. A same-named file in the user directory overrides any vendor
layer wholesale.

## Matching Chain

Before each request, `model_call` does a three-level fallback using the
current model spec (`provider/model`):

1. **exact model name**: `claude-sonnet-4-5.md` — the model segment
   matches exactly;
2. **vendor**: `claude.md` — two hit paths coexist: family inference
   from model-name keywords (e.g. `claude-*` hits `claude.md`, whether
   or not the spec carries the `anthropic/` prefix) and an explicit
   spec-prefix self-made layer (e.g. `stub/any-model` hits
   `stub.md`); when both exist, the explicit provider layer wins;
3. **default**: `default.md` — the fallback.

The three levels overlay; for the same key the deeper level wins. After
a runtime `/model` hot switch, **the next request immediately uses the
new model's prompts** (consistent with model hot-reload semantics).

The legacy single file `~/.zhuye/prompts.md` remains compatible (treated
as a default-layer override, with a deprecation log at startup);
migrating to `~/.zhuye/prompts/default.md` is recommended.

## `/prompt` Slash: Inspect and Refresh Prompts at Runtime

TUI/GUI (the `/prompt` command, protocol-level `Op::PromptCommand`)
manages prompt layers while the session is running, no restart needed:

```text
/prompt            # list available layers + the currently effective one (answered by EventMsg::PromptList)
/prompt list       # same as above
/prompt reload     # rescan the prompt library so edits to .zhuye/prompts/*.md take effect immediately
                   # (answered by EventMsg::PromptSwitched; the next request uses the new wording)
```

- available layers = the 9 built-in vendor layers + the user-directory
  layers (default / self-made), deduplicated in dictionary order;
- the "effective layer" is resolved from the current model spec via the
  matching chain; embedded layers are not recorded, only user layers
  are reported;
- headless supports it too: `zhuye exec "/prompt reload"` (the JSONL
  output carries a `prompt_switched` event);
- programming interface: TUI/GUI submit
  `Op::PromptCommand { action }` directly (Rust: `zhuye::Op`; the
  Python binding constructs it via Submission JSONL); events arrive on
  the per-submission channel, closed by `SubmissionClosed`.

## Prompt Key Reference

| Category | key | Purpose |
|------|-----|------|
| main agent | `system.main` | default agent system prompt (exec entry) |
| main agent | `system.facade-main` | lib facade (Builder) default agent prompt |
| mode | `system.plan-mode` | Plan mode rules handbook (injected by compose_core; overriding to `off` turns the section off entirely, see below) |
| mode | `runtime.active-mode` | Plan mode "current state" notice segment (overriding to `off` turns it off) |
| tools | `tool.<name>` | builtin tool descriptions, e.g. `tool.read` / `tool.bash` (23 total) |
| runtime | `runtime.nudge-loop` | loop reminder when a tool is called repeatedly |
| runtime | `runtime.nudge-final` | closing reminder when the iteration budget is exhausted |
| runtime | `runtime.nudge-web-fetched` | web_fetch progress summary title |
| runtime | `runtime.nudge-web-review` | web_fetch progress summary closer |
| runtime | `runtime.nudge-domain-repeat` | reminder for repeated fetches of the same domain |
| compaction | `compact.full` | full conversation summary prompt (`{{ conversation }}`) |
| compaction | `compact.recent` | incremental summary prompt (`{{ previous_summary }}` / `{{ recent_conversation }}`) |
| compaction | `compact.memory-extract` | memory extraction prompt (`{{ conversation }}`) |
| orchestration | `coordinator.system` | task coordinator default prompt (an explicit `[coordinator].system_prompt_path` file still takes top priority) |
| orchestration | `team.lead` | team lead member default prompt |
| orchestration | `goal.verifier` | goal-mode verifier system prompt |
| orchestration | `discussion.judge` | discussion judge system prompt |
| orchestration | `discussion.participant` | discussion participant user prompt format (`{{ topic }}` and other variables) |
| subagent | `subagent.explorer` | explorer fallback spec prompt |

Keys with `{{ var }}` are rendered by minijinja (see
`zhuye_prompt::template`). The `FINAL ANSWER: <answer>` closing
template is a **formatting contract** (tests and the presentation layer
depend on stripping the marker); it stays in the constant
`zhuye_prompt::FINAL_ANSWER_TEMPLATE`, with nudge wording injected via
variables.

## Turning Mode Sections Off

Non-coding scenarios (finance analyst, writing assistant, and other
custom personas) don't need the Plan Mode rules handbook or tool
nudges. Override the corresponding section with the reserved word
**`off`** (also accepts `none` / `disabled` / `false`, case-insensitive)
to turn it off entirely:

```markdown
<!-- ~/.zhuye/prompts/default.md -->
## system.main
You are a financial forecasting analyst...

## system.plan-mode
off

## runtime.active-mode
off
```

- `system.plan-mode` = `off` → the core system prompt no longer
  appends the `## Plan Mode` rules section (saves ~2.3KB of coding-agent
  persona per request);
- `runtime.active-mode` = `off` → in Plan mode the `## Active Mode`
  "current state" section is no longer injected (normal execution mode
  never injects it anyway);
- overriding to **empty is invalid**: an empty `## key` section body is
  dropped by the parser; you must write `off` or another keyword;
- the other keys (`system.main` / `tool.*` etc.) are unchanged — this
  only adds an explicit off switch for "mode"-style sections.

## Per-Model Override Example

The minimal file to give only Claude a different main prompt
(`.zhuye/prompts/claude.md`):

```markdown
## system.main
You are Claude, an expert pair programmer. ...(tuned in the style of
Anthropic's official agent prompts)

## tool.bash
(optional) Rewrite the bash tool description to match Claude's tool-use
habits.
```

Besides `claude/gpt/gemini/deepseek`, you can create a same-named exact
layer file for the model segment of any spec, e.g. `deepseek-chat.md`:

```markdown
## system.main
You are a DeepSeek coding assistant... (DeepSeek-specific wording)
```

## Programming Interface (facade / TUI / GUI)

```rust
use zhuye::{ZhuyeBuilder, PromptLibrary, PromptSet, init_library, library, split_spec};

// one Builder line mounts a bundled prompt package (directory has the highest priority, overriding same-named layers):
let agent = ZhuyeBuilder::new("claude/claude-sonnet-4-5")
    .workspace("/path/to/ws")
    .with_prompts_dir("/path/to/bundled-prompts")
    .build()?;

// or bypass the Builder and operate the process-level library directly (default → vendor → exact three-level fallback):
init_library(&[home_prompts_dir, ws_prompts_dir], None);
let set: PromptSet = library().resolve("anthropic/claude-sonnet-4-5");
let main_prompt = set.get_or("system.main", "fallback");
let (provider, model) = split_spec("anthropic/claude-sonnet-4-5");
```

The Python binding (`zhuye-py`) exposes `ZhuyeBuilder.prompts_dir(dir)`
in sync.

Choosing prompts at runtime in TUI/GUI: submit `Op::PromptCommand`
(via `thread.submit(...)`, the same Op channel as `/plan` / `/goal`)
and consume `EventMsg::PromptList` / `EventMsg::PromptSwitched` to
render the selection UI:

```rust
use zhuye::{Op, PromptAction, Submission};

// list selectable layers (for the popup selector):
let sub = Submission::with_id("ui-1", Op::PromptCommand { action: PromptAction::List });
// refresh after the user edited prompt files (no arg = full rescan):
let sub = Submission::with_id("ui-2", Op::PromptCommand {
    action: PromptAction::Switch { layer: None },
});
```

## Runtime Sequence

1. the entry point
   (`zhuye-exec::bootstrap_m4` / the lib facade's
   `zhuye::m4_bootstrap::build_default_m4_with_prompts`) scans
   `<ws>/.zhuye/prompts` + `~/.zhuye/prompts` + Builder-added
   directories and calls `init_library` to assemble the process-level
   `PromptLibrary` (repeatable for a full rescan);
2. `pre_loop` assembles the system prompt: when the agent-definition
   source is `BuiltinDefault`, `system.main` is resolved per the
   current spec; user customs (`.zhuye/agents/*.md`) are left
   untouched (`SystemPromptSource::Custom`);
3. tool description overrides happen at **spec assembly time**
   (`apply_model_overrides`) and do not touch the `Tool` trait —
   external tool implementers (MCP / LSP / plugin) are unaffected;
4. nudge / active-mode / compact / orchestration prompts are resolved
   from the library at their respective call sites; with no model layer
   configured the behavior is exactly the same as before.

## Cache Safety

Same model, same prompts → the prompt cache hits as usual; hot-switching
models already breaks the cache, so switching prompts together with the
model causes no extra invalidation.
