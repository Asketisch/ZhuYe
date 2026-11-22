# Using Plugins

<p align="center">  <a href="plugins.md">中文</a> · <b>English</b>  </p>

> This page is aimed at **users**: which plugins ship with the repo, how to
> install and enable them, how marketplaces are managed, where the state files
> live, and how to troubleshoot. Author-oriented content — manifest fields,
> capability declarations, the security model — is in
> [plugin-dev-guide.md](plugin-dev-guide.md); the publishing side of
> marketplaces is in [plugin-marketplace.md](plugin-marketplace.md).

## What a plugin is (30 seconds)

A plugin is a **directory** whose entry point is a `plugin.toml` manifest
(the Claude Code `.claude-plugin/plugin.json` format is also compatible).
The manifest declares which category (or categories) of capability the
plugin mounts into the engine; there are six:

| Capability | Declaration field | Form |
|---|---|---|
| slash commands | `commands` | `commands/*.md` prompt templates |
| sub-agents | `agents` | `agents/*.md`, registering `call_<role>` tools |
| skills | `skills` | `skills/<name>/SKILL.md`, activated on demand via `load_skill` |
| hooks | `hooks` | External-agent-style `hooks.json` (shell subprocesses or WASM components) |
| MCP server | `mcp_servers` | `.mcp.json`, stdio / streamable-http tool injection |
| WASM component tools | `wasm_tools` | `.wasm` files implementing the `zhuye:plugin` WIT world |

A plugin may mount just one of these categories, or all of them (the
sample plugin [`plugins/plugin-demo/`](../plugins/plugin-demo/) has all
six).

Installing and enabling are two independent actions: **install copies the
plugin directory into `~/.zhuye/plugins/cache` and registers it, while
enable writes the plugin id into the enabled list**. The engine (or the
runtime hot-reload) mounts plugins according to the enabled list —
installed does not mean active; you must enable.

## Plugins bundled in the repo

The repo ships 11 plugins, all under the [`plugins/`](../plugins/)
directory (conventions in its README). The "id" column below is the full
`name@marketplace` name used for enabling / uninstalling; for local
installs the marketplace is always `inline`:

| Directory | Manifest name | id | Purpose |
|---|---|---|---|
| [`plugin-demo/`](../plugins/plugin-demo/) | `demo` | `demo@inline` | Full-capability demo: one minimal runnable sample for each of the six capability types |
| [`computer-use/`](../plugins/computer-use/) | `computer-use` | `computer-use@inline` | Screen / mouse / keyboard control (xcap + enigo; build the binary first; on macOS grant Screen Recording and Accessibility permissions) |
| [`git-tools/`](../plugins/git-tools/) | `git-tools` | `git-tools@inline` | git workflow: branch / PR / diff probing + checkpoint snapshots + Stop auto-checkpoint (build the binary first) |
| [`shell/`](../plugins/shell/) | `shell` | `shell@inline` | Resident shell session pool: launch background processes, read output incrementally, write to stdin, kill — good for REPL / dev servers (build the binary first) |
| [`goal-queue/`](../plugins/goal-queue/) | `goal-queue` | `goal-queue@inline` | Goal mode + FIFO queue mode: enabling registers the `goal` / `queue` tools and starts the queue driver (gated form, off by default) |
| [`kms/`](../plugins/kms/) | `kms` | `kms@inline` | Local Markdown knowledge base: multi-wiki pagination + grep search + insight consolidation (dream) (build the binary first) |
| [`web-search/`](../plugins/web-search/) | `web-search` | `web-search@inline` | The `web_search` tool (Brave API). **Gated, on by default**; disable removes the tool from the registry (unlike `[active].disallowed_tools`, which only hides it) |
| [`notebook/`](../plugins/notebook/) | `notebook` | `notebook@inline` | The `notebook_edit` tool (Jupyter cell editing). **Gated, on by default** |
| [`verify-hooks/`](../plugins/verify-hooks/) | `verify-hooks` | `verify-hooks@inline` | Plugin enable path for the `verification` / `test_runner` hook pair. **Gated, off by default** (dual path: equivalent to listing them in config `[hooks] enabled`) |
| [`lsp/`](../plugins/lsp/) | `lsp` | `lsp@inline` | **Coarse switch** and docs for the LSP integration: activation is always driven by `[lsp_servers]` config; disable turns it fully off. Gated |

### Gated plugins

goal-queue / web-search / notebook / verify-hooks / lsp are **gated**
plugins: the capability body stays in an engine crate (keeping the original
tool name / approval / behavior — zero external-usage impact), and whether
it registers is decided by the plugin's enable state. The single source of
truth is `zhuye-plugin`'s `bundled_plugin_enabled` (default table
`BUNDLED_PLUGIN_DEFAULTS`).

- **On by default** (web-search / notebook): with empty config, behavior is
  identical to before pluginization; turn off via `zhuye plugin disable
  <id>@inline` or config `[plugins].disabled_plugins` (the tool leaves the
  registry and `tool_search` cannot find it);
- **Off by default** (goal-queue / verify-hooks / lsp): enabling is
  equivalent to the original explicit config path (lsp additionally needs
  `[lsp_servers]` definitions);
- the judgment is not hot-reloaded; restart the session after changing it.

⚠️ Note that `plugin-demo`'s directory name is `plugin-demo`, but its
manifest says `name = "demo"` — **the directory name and the manifest name
may differ; the plugin id uses the manifest `name`** — so enable it with
`zhuye plugin enable demo@inline`, not `plugin-demo@inline`.

## Installing and enabling

The full command surface (`zhuye plugin` has 7 subcommands + `marketplace`
has 5; definitions in `crates/runtime/zhuye-cli/src/main.rs`):

| Command | Arguments / flags | Effect |
|---|---|---|
| `install` | `<path or name@marketplace>` `--scope <managed\|user\|project\|local>` | Install. A local path is copied into the cache and registered (default `user` scope; locally installed ids are `name@inline`); ids containing `@` go through a registered marketplace path (including the dependency closure) |
| `enable` | `<id>` | Add the id to the enabled list (requires it to be **installed** first; otherwise `NotInstalled` is reported); also removes it from `disabled_plugins` if previously disabled |
| `disable` | `<id>` | Remove from the enabled list and write into `disabled_plugins` (installed files are not deleted) — for default-on gated plugins (web-search / notebook) this is the only effective way to turn them off |
| `uninstall` | `<id>` `--scope` `--yes` | Double delete: `installed_plugins.json` and the enabled list; default y/N interactive confirmation, `--yes` skips it; `--scope` must match the install scope; the managed scope refuses uninstall (enterprise pin) |
| `list` | — | Table of installed plugins, joined with enabled state (`enabled` / `installed`) |
| `info` | `<id>` | Single-plugin metadata: scope / version / install_path / timestamps / enabled state |
| `show` | `<id>` | Print the raw `plugin.toml` from the plugin's install directory (to verify which manifest got installed) |

```bash
# 1. Install: local directory (or install name@marketplace from a registered marketplace)
zhuye plugin install plugins/plugin-demo          # local install, id is name@inline
zhuye plugin install plugins/plugin-demo --scope local   # pick the scope (default user)
zhuye plugin install formatter@my-tools           # install from a registered marketplace (with dependency closure)

# 2. Enable: written into [plugins].enabled_plugins in ~/.zhuye/config.toml
zhuye plugin enable demo@inline

# 3. Inspect
zhuye plugin list                                  # table: id / scope / version / status
zhuye plugin info demo@inline                      # single-plugin metadata
zhuye plugin show demo@inline                      # raw manifest

# 4. Disable / uninstall
zhuye plugin disable demo@inline                   # only remove from enabled, keep the install
zhuye plugin uninstall demo@inline --yes           # full removal (--scope must match the install scope)
```

The **authoritative source** of the enabled state is
`[plugins].enabled_plugins` in `~/.zhuye/config.toml` (entries are full
`name@marketplace` names): both the startup assembly and the runtime
hot-reload only read it. `enable` / `disable` are just convenience
wrappers around rewriting that list; editing the file directly is
equally effective.

### scope (install scope)

| scope | Semantics |
|---|---|
| `managed` | Enterprise-distributed pin; `disable` / `uninstall` are refused (`ManagedLocked`) |
| `user` | User-level, the **default** (`~/.zhuye/plugins/cache`) |
| `project` | Project-level (⚠️ not yet supported in v0; the CLI accepts it but the semantics are not closed) |
| `local` | Local machine, temporary |

The same id can coexist under multiple scopes; lookup / mount priority
is `local > project > user > managed` (the highest-priority install wins).
If a non-empty `ZHUYE_HOME` is set, all of the paths above fall under
`ZHUYE_HOME` instead.

## Marketplaces

`zhuye plugin marketplace` has 5 subcommands:

```bash
# Register a marketplace source (five source types, all fully wired)
zhuye plugin marketplace add my-tools --from github --repo me/zhuye-plugins
zhuye plugin marketplace add local-mkt --from directory --path /path/to/mkt

# List / refresh / discover / remove
zhuye plugin marketplace ls
zhuye plugin marketplace refresh --name my-tools   # omitting --name refreshes all auto_update ones
zhuye plugin marketplace discover --limit 20       # discover repos by GitHub topic (default zhuye-plugin)
zhuye plugin marketplace remove local-mkt
```

Once registered, `zhuye plugin install <name>@<marketplace>` can install
plugins from it (including the dependency closure). For the manifest
format, source-type details, and the publishing flow, dig into
[plugin-marketplace.md](plugin-marketplace.md).

> ⚠️ **The directory source is dangerous**: a directory-type marketplace
> uses **the source directory itself as the install location** (no copy is
> made, see
> `crates/runtime/zhuye-cli/src/plugin/marketplace.rs:120`), so
> `marketplace remove` runs `remove_dir_all` on that path
> (`marketplace.rs:216`) — **it can delete your original directory
> outright**. Do not register a directory marketplace from a valuable
> directory; before removing, confirm the install_location recorded in
> `marketplace ls`.

## State files and hot-reload

```
~/.zhuye/
├── config.toml
│   └── [plugins].enabled_plugins = ["demo@inline", ...]   ← authoritative enabled list
└── plugins/
    ├── installed_plugins.json      ← install facts: id → per-scope entries (path / version / scope)
    ├── marketplaces.json           ← known marketplaces (name / source / install_location)
    └── cache/<marketplace>/<plugin>/<version>/   ← plugin files themselves
```

The two tables are maintained independently: **install tracks "what is
installed", enable tracks "what is enabled"**.

- **Startup mounting**: at process start, the `enabled_plugins` list is
  read entry by entry from the cache's manifests and mounted; plugins that
  mount successfully emit a `plugin_loaded` event in the JSONL event
  stream (`msg.type == "plugin_loaded"`, with the plugin id) — no such
  event = not mounted, see "Troubleshooting".
- **Runtime hot-reload**: editing the `[plugins]` section of
  `config.toml` inside an exec / serve session is diff-synced — newly
  enabled plugins mount, removed ones un-register, no restart needed.
  Note that the CLI (`zhuye plugin ...`) itself does **not** trigger a
  reload in an already-running process; after changes you must restart
  `zhuye exec` (there is a hint at the end of the command); the
  `serve --plugin X` flag only affects that one startup and is not written
  back to config.
- **Partial-success semantics**: a failure in one capability category
  only warns and skips, never blocking the other categories; one
  plugin's failure does not affect other plugins.

## Troubleshooting

Common reasons a plugin is "installed but not active", in order of hit
rate:

1. **Not enabled** — installing only registers, it does not mount. Check
   that the id is in `[plugins].enabled_plugins` in
   `~/.zhuye/config.toml`; the status column of `zhuye plugin list`
   should be `enabled` (not `installed`).
2. **Installed under a different scope** — when the same id coexists in
   multiple scopes, the highest of `local > project > user > managed`
   wins, and `uninstall`'s `--scope` must match the install scope. Use
   `zhuye plugin info <id>` to see the actual scope.
3. **Manifest name does not match the expected id** — the `<name>` part of
   the plugin id comes from the manifest's `name` field, **not the
   directory name** (see the `plugin-demo` / `demo` example under
   "Plugins bundled in the repo"). A wrong id on enable / uninstall
   reports `NotInstalled`.
4. **A manifest field is misspelled** — the current schema does not
   validate unknown fields: a wrong top-level key is silently dropped,
   no error, it just does not take effect (misspelling inside a nested
   object is what triggers `ManifestParse`). Use
   `zhuye plugin show <id>` to compare against the raw file, and fall
   back to [plugin-dev-guide.md](plugin-dev-guide.md)'s field table when
   in doubt.

Deeper investigation: ① find `plugin_loaded` in the JSONL event stream
(missing = mount failure); ② stderr logs — a single plugin / capability
failure only `tracing::warn!`s (unknown hook event names, missing
commands, MCP server start failures all land here); ③ for binary plugins
(computer-use / git-tools / shell / kms), make sure the
corresponding build script (`./scripts/build_*_plugin.sh`) has been run,
otherwise `${PLUGIN_ROOT}/bin/...` points at a missing file; ④ after
changing enable / disable via the CLI, restart `zhuye exec` (the CLI does
not hot-reload across processes).

## Further reading

| Doc | Content |
|---|---|
| [plugin-marketplace.md](plugin-marketplace.md) | marketplaces: manifest format, source types, publishing and refresh flow |
| [plugin-dev-guide.md](plugin-dev-guide.md) | plugin development: manifest fields, capability declarations, security model, multi-language WASM |
| [architecture.md](architecture.md) | the six-layer architecture and where `zhuye-plugin` sits |
| [../plugins/README.md](../plugins/README.md) | the repo's built-in plugin directory and conventions |
