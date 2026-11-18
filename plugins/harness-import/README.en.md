# harness-import —— the external Harness history import plugin

<p align="center">  [中文](README.md) · **English**  </p>

Imports Claude Code / Codex / ZCode conversation history into native
ZhuYe sessions (`~/.zhuye/sessions/YYYY/MM/DD/<thread_id>.jsonl`); the
import artifact is an ordinary session: visible in `zhuye session ls`,
resumable with `zhuye exec --resume <id>`, and shown under its
workspace in Desktop.

The capability core is the standalone crate
`crates/integrations/zhuye-import`; this plugin only does **packaging
and wiring**: one dual-mode binary + one slash command.

## Install

```bash
# 1. Build and assemble the binary into the plugin's bin/ (bin/ content is not checked in)
./scripts/build_import_plugin.sh

# 2. Install + enable
zhuye plugin install plugins/harness-import --scope user
zhuye plugin enable harness-import@inline

# 3. Restart zhuye exec / serve (or wait for hot reload), then it can be used in the conversation
```

## Usage

| Mode | Entry point | Description |
|---|---|---|
| In conversation (CLI / Desktop) | `/harness:import <path>` | command expansion → the model calls the MCP tool, one approval |
| In conversation (just say it) | 「帮我把 Codex 会话导进来」 | the model calls `import_session` / `list_importable` on its own |
| Run directly in terminal (batch) | `bin/zhuye-import <path> --from auto` | no LLM involved; good for scripts and the desktop import wizard |

MCP tools:

- `import_session(path, source_kind?, dry_run?, include_thinking?, force?)`
  —— import a file or directory (directories are walked recursively,
  collecting `.jsonl` files and sniffing each); with `force`, when
  overriding a session of the same ID the original file location is
  kept and its old rotation shards are replaced;
- `list_importable(source_kind)` —— scans that harness's default local
  install location.

Direct-run examples:

```bash
bin/zhuye-import ~/.codex/sessions --all --from codex --dry-run  # preview
bin/zhuye-import ~/.claude/projects/*/*.jsonl                    # auto-sniff import
bin/zhuye-import --from zcode ...                                # ZCode, see below
```

## Fidelity boundaries

- Preserved: turn structure, visible text, tool calls and results,
  token usage, compaction summaries, workspace ownership; when the
  source id is a valid UUID it is preserved as-is (repeated imports are
  naturally idempotent).
- Dropped by default: the thinking chain (kept with
  `include_thinking` / `--include-thinking`); harness-injected
  environment scaffolding (kept with `keep_injected` /
  `--keep-injected`; when kept it is **front-loaded into the turn that
  holds the immediately following real user input**, not a standalone
  turn); Claude Code sub-agent trajectories (kept with
  `--include-sidechain`).
- ZCode: history is stored in `~/.zcode/cli/db/db.sqlite`; on import an
  online SQLite backup is produced in a temp directory and parsed
  read-only — the source database is never written to; the MCP tool
  entry does not expose the SQLite path, so for batch ZCode imports use
  the direct-run mode's `list_importable` + the single-session import
  flow.

## Known limitations

- DeepSeek Harness is not yet supported (the architecture reserves an
  adapter slot, see `zhuye-import/src/adapter.rs`).
- Codex encrypted reasoning cannot be decrypted; a warning is recorded
  and it is skipped.
- Codex inline base64 images keep their raw bytes and MIME; local or
  remote images that cannot be recovered are recorded as a warning and
  skipped.
