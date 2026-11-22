# CLI Reference

<p align="center">  <a href="cli.md">中文</a> · <b>English</b>  </p>

> Full list and usage of the subcommands of the `zhuye` binary. The
> single `zhuye` binary is produced by the `zhuye-cli` crate and has
> 22 top-level subcommands, **no `tui` subcommand** (the TUI has been
> split out into the separate repository
> [Reflect-TUI](https://cnb.cool/Demon1019/Reflect-CLI)), and **no
> `import` subcommand either**. For the complete parameter list of each
> command,
> use `zhuye <command> --help`.

## Overview

| Command | What it does |
|---------|--------------|
| `exec` | run one headless turn and exit; events written to stdout as JSONL |
| `serve` | persistent stdio JSONL session server (the protocol entry point for the Python / TS SDKs) |
| `discussion` | run a multi-agent discussion from a TOML config file |
| `task` | structured tasks + team management (TaskCreate / TaskList / TeamCreate, etc.) |
| `pipeline` | run a DAG pipeline (team-plan → team-prd → team-exec → team-verify) |
| `plugin` | install / list / enable / disable / uninstall plugins + marketplace management |
| `mcp` | list / add / remove / test / show MCP server configuration |
| `lsp` | list / view LSP server configuration |
| `hooks` | built-in hook on/off management (state persisted in `[hooks].disabled`) |
| `skill` | Skill marketplace: install skills from a GitHub repo into `~/.zhuye/skills/` |
| `config` | view or edit `~/.zhuye/config.toml` (show / set / unset / edit / ls) |
| `session` | list / view / delete persisted sessions |
| `traces` | view local LLM call records (`~/.zhuye/traces/model-io/`) |
| `login` | configure provider credentials (written to `~/.zhuye/config.toml`) |
| `models` | list / pick the models currently available on a provider (the list is fetched live from the API) |
| `secrets` | encrypted key store (set / get / list / delete; AES-256-GCM + machine key) |
| `security` | security scan (cargo audit) |
| `workspace` | git clone / sync of workspaces |
| `doctor` | environment self-check (config / rollout / mcp / rustc; `--dump-config` prints a snapshot of the effective configuration) |
| `update` | print upgrade notes (offline) |
| `init` | generate an AGENTS.md project description in the current directory (agent context anchor) |
| `version` | print the version number and exit |

## exec — headless single turn

Run one turn and exit. The positional `prompt` argument is optional
(when `--resume` / `-c` / `-r` is given, the prompt is ignored and
resume takes precedence).

| Parameter | Description |
|-----------|-------------|
| `prompt` | user input to run (optional) |
| `--resume <UUID>` | resume a historical session by thread id (replays Message and Compaction records) |
| `-c` / `--continue` | continue the most recent session |
| `-r <N>` | resume by ordinal (1-indexed, newest first) |
| `--agent <NAME>` | agent definition name (`~/.zhuye/agents/` and `{workspace}/.zhuye/agents/`, default `default`) |
| `--plan-mode` | start in Plan mode (read-only; mutually exclusive with `--full-auto`) |
| `--full-auto` / `--yolo` | fully delegated: starts with `PermissionMode::Bubble`, non-high-risk tools are approved automatically, high-risk tools still require manual confirmation |
| `--ephemeral-tasks` | tasks are not persisted (in-memory store) |
| `--ephemeral-teams` | teams are not persisted (in-memory store) |
| `--auto-root` | walk up from cwd to detect the project root (the first directory containing `.git` / `Cargo.toml` / `.zhuye/`) |
| `--image <PATH>` | image attachment, may be passed multiple times (png/jpeg/gif/webp/bmp, single file ≤ 10MiB; mutually exclusive with the resume parameters) |

```bash
ZHUYE_MODEL=mock ./target/debug/zhuye exec "重构这个模块"
./target/debug/zhuye exec --resume <UUID> "继续"
./target/debug/zhuye exec -c          # continue the most recent
```

## serve — persistent session (SDK protocol entry point)

Reads `Submission` line by line from stdin, writes `Event` line by
line to stdout, stderr is logging; multiple turns share the same
session. The Python / TS SDKs both wrap this (see
[sdk.md](sdk.md)).

| Parameter | Description |
|-----------|-------------|
| `--agent <NAME>` | same as exec |
| `--resume` / `-c` / `-r` | the three resume options, same as exec |
| `--full-auto` / `--yolo`, `--plan-mode` | same as exec; the two are mutually exclusive |
| `--ephemeral-tasks` / `--ephemeral-teams` / `--auto-root` | same as exec |
| `--plugin <ID>` | plugin id to enable explicitly, may be given multiple times; when passed, **overrides** the config's `[plugins].enabled_plugins`, so SDK embedders load precisely |

Difference from `exec`: serve has **no** prompt positional argument
and no `--image` (input is driven by stdin Submissions).

## plugin / marketplace

`zhuye plugin install <path or name@marketplace>` (local installs use
the id `name@inline`, `--scope` supported), `list`, `info`,
`enable`, `disable`, `uninstall --yes`, `show`.

`zhuye plugin marketplace` manages plugin marketplaces: `add`
(five source kinds: github / git / url / file / directory), `ls`,
`refresh`, `discover` (by topic), `remove`.

⚠️ **directory source — dangerous behavior**: it uses the source
directory itself as the install location, and `remove` will execute
`remove_dir_all` on that path. For full usage and publishing see
[plugins.md](plugins.md) and
[plugin-marketplace.md](plugin-marketplace.md).

## Remaining subcommands

- **discussion**: runs a multi-agent discussion from a TOML config
  file; see the orchestration section of [features.md](features.md).
- **task / pipeline**: structured tasks, teams and DAG pipelines,
  persisted in `~/.zhuye/tasks/`, `~/.zhuye/teams/`
  (`--ephemeral-*` switches to in-memory).
- **mcp**: `ls` / `add` (`--command` stdio and `--url` http are
  mutually exclusive) / `remove` / `test` / `show`; configuration
  lives in `[mcp_servers]`.
- **lsp**: list / view LSP server configuration.
- **hooks**: built-in hook on/off; state persisted in
  `[hooks].disabled`.
- **skill install <repo>**: install a skill from a GitHub repository
  (root containing SKILL.md) into the user-level
  `~/.zhuye/skills/` (`--dir` to change it).
- **config**: `show` / `set` / `unset` / `edit` / `ls`,
  operating on `~/.zhuye/config.toml` (hot-reloaded after changes,
  see [configuration.md](configuration.md)).
- **session** / **traces**: persisted session management and local LLM
  call record viewing.
- **login**: configure provider credentials (`anthropic` /
  `openai` / `ollama`; `local` is an alias for `ollama`);
  `--api-key` passes the key directly, `--force` skips the
  short-key warning.
- **models**: the model list is fetched live from the provider API.
- **secrets**: `set` (plaintext read from stdin; no key in argv) /
  `get` / `list` / `delete --yes`; AES-256-GCM + machine key.
- **security**: cargo audit security scan.
- **workspace**: git clone / sync.
- **doctor**: `--check-network` sends a 1-token probe to the current
  provider; `--dump-config` prints a JSON snapshot of the currently
  effective configuration (keys show label only).
- **update**: `--check-only` only prints the check notes, offline.
- **init**: `--force` overwrites an existing AGENTS.md (skipped by
  default).

## Standalone binaries

- **Reflect-TUI**: the TUI binary provided by a separate repository;
  the `zhuye` binary in this core repo has no `tui` subcommand.

## Global parameters & environment

- `-C` / `--cwd <DIR>`: switch the working directory (available on
  all subcommands).
- Common environment variables: `ZHUYE_MODEL` (model spec;
  `mock` is offline), `ZHUYE_BIN` (SDK binary path; resolution
  priority in [sdk.md](sdk.md)), `ZHUYE_MOCK_SCRIPT` (scripted mock
  replies), `ZHUYE_HOME` (data directory redirect),
  `ZHUYE_REMOTE_TOOL_TIMEOUT_SECS` (remote tool receipt limit,
  default 120s). The full list is in
  [configuration.md](configuration.md).
