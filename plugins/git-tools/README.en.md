# git-tools —— the git workflow plugin

<p align="center">  [中文](README.md) · **English**  </p>

Reuses the framework's `zhuye-git` capability family (branch / PR
probing + tag-ified checkpoints).

## Install (three steps)

```bash
./scripts/build_git_tools_plugin.sh
zhuye plugin install plugins/git-tools --scope user
zhuye plugin enable git-tools@inline
```

## MCP tools (full name `mcp__plugin:git-tools@inline:git__<tool>`)

| Tool | Description |
|---|---|
| `git_branch_summary` | branches + diff stats against the default branch + open PRs (gh; best-effort) |
| `git_checkpoint_create` | take a tag snapshot (rewindable; returns no-changes when there is nothing to commit) |

## Stop auto-checkpoint (optional)

`hooks/hooks.json` mounts a Stop hook (`zhuye-cp-hook`): automatically
takes a snapshot at the end of every turn. The hook **always exits 0** —
external hooks are fail-closed (non-zero = Deny), and a Stop Deny means
forcing the turn to continue; a checkpoint failure must not block the
turn. To skip the auto snapshots, simply drop the hooks capability in
`zhuye plugin` management.

## slash commands

- `/git-summary [cwd]` —— probing summary;
- `/checkpoint [label]` —— explicit snapshot.
