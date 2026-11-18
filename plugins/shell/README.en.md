# shell —— the resident shell session plugin

<p align="center">  [中文](README.md) · **English**  </p>

Complements the built-in `bash` (one-shot): launch a process in the
background → read output incrementally → write to stdin → kill. Good
for REPL / dev servers / watch / long tasks that need to be stopped
mid-way. PTY (resize / raw mode) is not done (a shared gap with the
desktop client).

## Install (three steps)

```bash
./scripts/build_shell_plugin.sh
zhuye plugin install plugins/shell --scope user
zhuye plugin enable shell@inline
```

## Isolation

The session process has its env cleared, then only HOME / PATH are
injected — the host's API keys and other credentials are not passed to
the subprocess. cwd is specified by the `shell_run` argument (default a
temp directory).

## MCP tools (full name `mcp__plugin:shell@inline:shell__<tool>`)

| Tool | Description |
|---|---|
| `shell_run` | start a session, returns the session_id immediately |
| `shell_read` | incremental output since the last call + exit status |
| `shell_write` | write to stdin |
| `shell_close_stdin` | send EOF |
| `shell_kill` | terminate (SIGKILL) and remove; on Unix also terminates the child processes in the same process group |
| `shell_list` | list active sessions |

## slash command

`/shell <command>` —— see `commands/shell.md`.
