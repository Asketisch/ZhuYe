# shell —— 常驻 shell 会话插件

与内置 `bash`(一次性)互补:后台启动进程 → 增量读输出 → 写 stdin →
kill。适合 REPL / dev server / watch / 需要中途停止的长任务。PTY
(resize / raw mode)不做(与桌面端共同的缺口)。

## 安装(三步)

```bash
./scripts/build_shell_plugin.sh
zhuye plugin install plugins/shell --scope user
zhuye plugin enable shell@inline
```

## 隔离

会话进程 `env_clear` 后只注入 HOME / PATH —— 宿主的 API key 等凭据
不传给子进程。cwd 由 `shell_run` 参数指定(默认临时目录)。

## MCP 工具(全名 `mcp__plugin:shell@inline:shell__<tool>`)

| 工具 | 说明 |
|---|---|
| `shell_run` | 启动会话,立即返回 session_id |
| `shell_read` | 自上次调用的增量输出 + 退出状态 |
| `shell_write` | 写 stdin |
| `shell_close_stdin` | 发 EOF |
| `shell_kill` | 终止(SIGKILL)并移除 |
| `shell_list` | 列活跃会话 |

## slash 命令

`/shell <命令>` —— 见 `commands/shell.md`。
