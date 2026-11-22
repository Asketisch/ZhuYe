# ZhuYe Plugin Development Guide (for the community)

<p align="center">  <a href="plugin-dev-guide.md">中文</a> · <b>English</b>  </p>

> This guide is for **every developer who wants to build plugins for
> ZhuYe**; familiarity with ZhuYe internals is not required. Start from
> zero, get a first plugin running, then dive into the individual
> capabilities and WASM components as needed. For the user perspective
> (install / enable / built-in plugins / troubleshooting) see
> [`plugins.md`](plugins.md); for publishing to a marketplace see
> [`plugin-marketplace.md`](plugin-marketplace.md). This doc focuses on
> "how to do it".

---

## 0. What a plugin is (30 seconds)

A ZhuYe plugin = one directory containing a `plugin.toml` manifest plus a
number of **capability declarations** (slash commands / sub-agents /
skills / hooks / MCP servers / WASM components). Once installed into
ZhuYe, these capabilities are mounted into the engine:

- slash commands become `/commands` the user can type;
- sub-agents become tools the model can call;
- skills enter the progressive-disclosure directory (the model
  `load_skill`s them on demand);
- hooks guard or inject at event points such as tool calls;
- MCP server tools merge into the model-visible tool set;
- WASM components provide the same hooks / tools **in-process** (zero
  subprocesses).

Enabling / disabling a plugin mid-run **requires no restart** (the
`[plugins]` config section is hot-reloaded).

---

## 1. Quick start: get your first plugin running in 5 minutes

### 1.1 Write the manifest and one command

```bash
mkdir -p my-first-plugin/commands
cd my-first-plugin

cat > plugin.toml <<'EOF'
name = "my-first"
version = "0.1.0"
description = "我的第一个烛夜插件"
commands = "./commands"
EOF

cat > commands/hello.md <<'EOF'
---
description: 打招呼
---
你好,$ARGUMENTS!这是来自插件的消息。
EOF
```

### 1.2 Install, enable, verify

```bash
# install (copied into ~/.zhuye/plugins/cache and registered; a local directory installs as my-first@inline)
zhuye plugin install /path/to/my-first-plugin

# enable (written into [plugins].enabled_plugins in ~/.zhuye/config.toml)
zhuye plugin enable my-first@inline

# verify
zhuye plugin list
zhuye exec "/my-first:hello 烛夜"     # → 你好,烛夜!这是来自插件的消息。
```

> **Important**: a local install **copies the entire plugin directory**
> into `~/.zhuye/plugins/cache`; after installation it is decoupled from
> the working directory. The full development-iteration workflow is in
> §5.3.

### 1.3 Hot reload

While a session is in progress, directly edit the
`[plugins].enabled_plugins` list in `~/.zhuye/config.toml` (add / remove
ids); the running session diff-syncs mounting / unmounting, no restart
needed. The CLI's `plugin enable` / `disable` only change the config and
take effect in already-running sessions through this same hot-reload
path.

---

## 2. Choosing a capability: decision table

| What you want to do | Which capability | Notes |
|---|---|---|
| Give users a `/command` shortcut (prompt template) | **commands** | simplest; the "user-level commands directory" also works without a plugin (§3.1) |
| Give the model a callable **sub-agent** (isolated context) | **agents** | one md defines the role; a `call_<role>` tool is generated automatically |
| Give the model **on-demand reference material / operating manuals** | **skills** | activated by the model via `load_skill`; `tools` annotations supported |
| **Guard / audit / inject around tool calls** | **hooks** | shell scripts or WASM components; fail-closed |
| Give the model **new tools** (needs external process / network service) | **mcp_servers** | any MCP server, stdio / HTTP |
| Give the model **new tools** (pure computation, any language, in-process) | **wasm_tools** | WASM components, deterministic timeout + sandbox (§5) |

Rule of thumb: **if a file can solve it, don't write code** (commands /
skills / agents are all pure declarations); when you need execution
logic, prefer a shell hook or an MCP server; reach for WASM when you want
simple distribution, cross-language, in-process performance.

---

## 3. Tutorials per capability

### 3.1 commands —— slash commands

**Directory convention**: any-depth `*.md` under `commands/`. The command
name = `<plugin-name>:<relative path segments>`, e.g. `my-first:hello`.

```markdown
---
description: 按语言生成项目骨架
---
请为 $1 语言创建项目骨架,参数总览:$ARGUMENTS
```

- `$ARGUMENTS` = everything after the command name; `$1`..`$9` =
  positional arguments one by one.
- User input: `/my-first:skeleton rust 附加要求`.
- The frontmatter `description` is shown in the command list.

**Don't want to write a plugin?** The user-level commands directory is
simpler: drop `<stem>.md` into `<workspace>/.zhuye/commands/` or
`~/.zhuye/commands/`; the command name is the file name (`/review args`
works directly, no prefix). Mounted at startup; hot re-scanning
supported.

### 3.2 agents —— sub-agents

`agents/<name>.md` (frontmatter + body = system prompt):

```markdown
---
name: 代码评审员
description: 对当前 diff 做严格评审,输出阻塞项与建议
model: anthropic/claude-sonnet-4-5
tools: [read, grep]
---

你是资深评审。输出分两节:## 阻塞项、## 建议。
```

Once mounted the model gets a `call_<role>` tool (role is derived from
the plugin name, snake_case). `tools` is a **whitelist**: the sub-agent
can only use the listed parent-level tools; empty = a pure-text
sub-agent.

### 3.3 skills —— skills

`skills/<name>/SKILL.md`:

```markdown
---
name: zhuye-release
description: 烛夜 SDK 发版流程(双端版本联动)
tools: [bash]
---

发版步骤:…(正文在模型 load_skill 后进入其上下文)
```

- The frontmatter must have at least `description`; `triggers` (trigger
  keywords), `tools` (tools appended on activation), `when_paths`
  (associated files) are optional.
- **Non-conformant plugin SKILL.md files are degraded** (only manifest
  metadata used, no tools/triggers) with a warning — check the format
  yourself before publishing.
- User-level skills directories `<workspace>/.zhuye/skills/` and
  `~/.zhuye/skills/` need no plugin; just drop the directory there.

### 3.4 hooks —— event guards

`hooks.json` (external-agent style):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "bash",
        "hooks": [
          { "type": "command", "command": "./guard.sh", "timeout": 10,
            "env": ["MY_POLICY_URL"] }
        ]
      }
    ]
  }
}
```

**Execution protocol** (authoritative definition:
`crates/abilities/zhuye-hooks/src/shell.rs`):

- the event JSON (containing `kind` / tool name / args / result) is
  written to the subprocess's **stdin**;
- the subprocess returns a decision JSON on **stdout**, with a
  `decision` field:

| decision | Effect |
|---|---|
| `allow` / empty output / non-JSON (exit 0) | pass through (informational output does not block) |
| `deny` (alias `block`) | reject, `reason` is shown to the user |
| `ask` | escalate to human confirmation, `reason` is the question |
| `inject` (alias `message`) | inject a system reminder (`reason` is the content) |

- **fail-closed**: timeout / non-zero exit → Deny (if the guard is
  unavailable, rather reject).
- `matcher` only applies to `PreToolUse` / `PostToolUse`; supports `*` /
  `?` wildcards.
- **Environment-variable hygiene**: env vars whose names match secret
  patterns are stripped by default (`*_TOKEN` / `*_API_KEY` etc.); if one
  must pass through, list it explicitly in `env`.

### 3.5 mcp_servers —— MCP servers

`.mcp.json` (stdio) or an inline declaration (HTTP):

```json
{
  "my-tools": {
    "command": "${PLUGIN_ROOT}/bin/my-server",
    "args": ["--mode", "strict"],
    "timeout": 60
  }
}
```

- tools are injected into the model-visible set named
  `mcp__plugin:<plugin-id>:<server>__<tool>`.
- **Permission floor**: MCP tools are raised to `Prompt` even if
  declared `Auto` (approval required before a call).
- **Process isolation**: the subprocess only gets HOME / PATH and the
  explicitly declared env; it does not inherit secrets.
- Editing the `[mcp_servers]` config section at runtime supports
  field-level hot reload (only servers that truly changed are
  restarted).

---

## 4. WASM components (multi-language plugins)

WASM is the community-facing **in-process plugin form**: any language
produces a single `.wasm` file against one contract, and the host
instantiates it in-process — zero subprocesses, zero JSON-RPC boilerplate,
deterministic timeouts, a pure-computation sandbox with no filesystem /
no network / no environment variables.

### 4.1 Two worlds

`wit/plugin.wit` (`package zhuye:plugin@0.1.0`) defines two worlds; a
component implements whichever one it needs:

- **`zhuye-hook`**: `handle(event) -> result<string, string>` — event
  JSON in, decision JSON out (the `decision` protocol is identical to
  shell hooks, §3.4);
- **`zhuye-tool`**: `describe() -> string` (returns the tool
  declaration JSON) + `invoke(args) -> result<string, string>` (returns
  the full protocol-layer ToolOutput wire shape: `content` / `is_error` /
  `metadata` / `elapsed_ms`, none may be missing).

The ABI deliberately picks **JSON strings**: dynamic-language authors
need not generate strongly typed bindings.

### 4.2 Minimal Rust component

```
my-wasm-plugin/
├── Cargo.toml          # standalone project (add an empty [workspace] table to keep out of the main workspace by accident)
└── src/lib.rs
```

```toml
[package]
name = "my-hook"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = "0.40"

[workspace]
```

```rust
wit_bindgen::generate!({
    // path is relative from this file to the repo-root wit/ directory; for distribution, copy the same structure along
    path: "path/to/wit",
    world: "zhuye:plugin/zhuye-hook@0.1.0",
});

struct MyHook;

impl Guest for MyHook {
    fn handle(event: String) -> Result<String, String> {
        Ok("{\"decision\":\"deny\",\"reason\":\"周末禁止执行写操作\"}".to_string())
    }
}

export!(MyHook);
```

Build and declare:

```bash
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
# artifact target/wasm32-wasip2/release/my-hook.wasm → drop it into the plugin's bin/
```

```toml
# plugin.toml
[[wasm_tools]]
path = "bin/my-tool.wasm"
name = "my_tool"
```

```json
// hooks.json entry (in a command object, use the wasm field instead of command)
{ "type": "wasm", "wasm": "${PLUGIN_ROOT}/bin/my-hook.wasm", "timeout": 10 }
```

### 4.3 Other languages

| Language | Toolchain | Status |
|---|---|---|
| Rust | wit-bindgen 0.40 (pinned, see below) | production-ready (samples in this repo) |
| TypeScript / JS | jco (componentize-js) | on the roadmap |
| Python | componentize-py | on the roadmap |
| Go | tinygo + wasm-tools | experimental |

The contract is language-agnostic — any language that can produce a
component-model `.wasm` can participate; `wit/README.md` has the version
policy details.

### 4.4 Security and timeouts (must understand)

- **Deterministic timeouts**: each call defaults to 30s (epoch interrupt
  force-truncates), overridable via `timeout` in hooks.json; timeout →
  hook **Deny** / tool `is_error` (fail-closed).
- **Sandbox**: components get no filesystem / network / environment
  variables (pure computation; for IO use an MCP server or a shell
  hook).
- **Permissions**: WASM tools share the MCP baseline — declaring `Auto`
  has no effect; everything is `Prompt` (user approval required before a
  call).
- **Decision fail-closed**: component returns `Err` / invalid JSON /
  unknown `decision` → Deny.
- **Pinned toolchain matrix**: host wasmtime 30.0.1 ↔ guest wit-bindgen
  0.40. The component model's WASI interface minor versions must match
  exactly; upgrades require host and component together (see
  `wit/README.md`).

---

## 5. Debugging

### 5.1 Plugin not taking effect?

Check in order:

1. `zhuye plugin list` — is the id listed, is `enabled` true;
2. does `[plugins].enabled_plugins` in `~/.zhuye/config.toml` contain the
   full `name@marketplace` name;
3. search stderr for `plugin`-related warnings (a single capability
   failure does not block the others);
4. `name` in `plugin.toml` must equal the directory name; declared paths
   must exist (reserved paths warn without blocking);
5. WASM: the artifact of `scripts/build_plugin_demo_wasm.sh` lives in
   `plugins/plugin-demo/bin/`; a load failure reports an explicit
   RegisterError.

### 5.2 Common errors quick reference

| Symptom | Cause |
|---|---|
| Typing `/demo:hello` expands nothing | the command name is missing the plugin prefix; or the plugin is not enabled |
| a hook Denies every time but the script looks fine | non-JSON / non-zero exit is fail-closed; run the script by hand first and check output and exit code |
| MCP tools don't appear | the server failed to start (stdio init timeout / command not found); check stderr warnings |
| a WASM tool fails to load | not a zhuye world / toolchain matrix mismatch / corrupt file; explicit error at mount time |
| a SKILL.md is degraded | the frontmatter is missing `name` / `description` |

### 5.3 Local development workflow (install is a cache copy)

A local install **copies** the plugin directory into
`~/.zhuye/plugins/cache/inline/<name>/<version>/`; after installation it
is decoupled from the working directory — editing files in the source
directory does **not** take effect automatically. Two iteration styles:

```bash
# Style A (recommended): reinstall after every change, uninstall the old one first, then install the new
zhuye plugin uninstall my-first@inline --yes
zhuye plugin install ./my-first-plugin

# Style B (fast): edit files in the cache directly; command bodies are read from disk on every expansion
vi ~/.zhuye/plugins/cache/inline/my-first/0.1.0/commands/hello.md
```

Note: repeating `install` for the same `id@scope` reports "already
installed"; uninstall first, or bump the `version` / change the scope and
install again. Changes to `plugin.toml`, `hooks.json`, and capability
directory structure must go through Style A to reinstall (editing files
inside the cache directly only works for pure body content).

---

## 6. Publishing to a Marketplace

A marketplace = a `.claude-plugin/marketplace.json` (JSON) in one repo,
listing multiple plugin entries and their sources:

```json
{
  "name": "my-tools",
  "owner": { "name": "me", "url": "https://example.com" },
  "plugins": [
    {
      "name": "my-formatter",
      "description": "格式化插件",
      "source": { "source": "github", "repo": "me/my-formatter", "ref": "v1" },
      "category": "devtools"
    }
  ]
}
```

On the user side:

```bash
zhuye plugin marketplace add my-tools --from github --repo me/my-tools
zhuye plugin install my-formatter@my-tools
zhuye plugin enable my-formatter@my-tools
```

Sources support five kinds: `github` / `git` / `url` / `file` /
`directory`; ⚠️ `marketplace remove` on a `directory` source deletes the
source directory you provided — see the dangerous-behavior warning in
[`plugin-marketplace.md`](plugin-marketplace.md).

---

## 7. Testing advice (for plugin authors)

For the Rust part of a plugin repo:

- use **property-based tests** (`proptest`) for pure functions (argument
  substitution, state machines, parsers) to explore the blind spots of
  hand-written cases — `zhuye-protocol` / `zhuye-tools` /
  `zhuye-compact` in this repo each have ready-made examples
  (`mod proptests`);
- coverage via `cargo llvm-cov --summary-only` (baseline 80%+);
- write **positive and negative cases** per the §3.4 decision-field
  table for hook decision JSON (missing `decision` → fail-closed is
  protocol behavior, not a bug).

## 8. Version compatibility notes

- **Wire protocol v0 is frozen**: only additive evolution between the
  engine and SDKs / clients; old plugins and old clients are not broken
  by new capabilities.
- **Plugin manifests are backward compatible**: older ZhuYe silently
  ignores new fields (e.g. `[[wasm_tools]]`); your plugin is safe on
  both old and new versions.
- **Pinned WASM toolchain matrix** (wasmtime ↔ wit-bindgen, see
  `wit/README.md`): host upgrades are declared in sync; build your
  components against the pinned versions.
- **`user_config`**: the field is defined but not consumed at runtime —
  declaring it does not change behavior; do not depend on it.

---

## 9. References and samples

- Reference manual (field level): [`plugins.md`](plugins.md)
- Marketplace publishing: [`plugin-marketplace.md`](plugin-marketplace.md)
- Complete sample plugin: `plugins/plugin-demo/` (five directory-declared
  capabilities + a WASM component, one-click build via
  `scripts/build_plugin_demo_wasm.sh`)
- WASM contract: `wit/plugin.wit` + [`wit/README.md`](../wit/README.md)
- Authoritative hook decision protocol: `crates/abilities/zhuye-hooks/src/shell.rs`
