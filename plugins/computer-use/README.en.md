# computer-use —— the screen and input plugin

<p align="center">  [中文](README.md) · **English**  </p>

xcap (screenshot) + enigo (mouse and keyboard). MCP tools carry the
`Prompt` permission floor by nature; every call pops the host
approval.

## Install (three steps)

```bash
./scripts/build_computer_use_plugin.sh             # default --features real
zhuye plugin install plugins/computer-use --scope user
zhuye plugin enable computer-use@inline
```

## Permission requirements

- macOS: System Settings → Privacy & Security → **Screen Recording**
  (screenshot) + **Accessibility** (keyboard and mouse).
- Not authorized / headless / built without the `real` feature → all
  tools uniformly return `Unavailable` (an isError message, no
  panic).

## MCP tools (full name `mcp__plugin:computer-use@inline:computer__<tool>`)

| Tool | Description |
|---|---|
| `computer_screenshot` | screenshot of the main display, returns a PNG image |
| `computer_click` | click at absolute coordinates (button optional) |
| `computer_type` | type text |
| `computer_key` | key combination (`ctrl+c`) |
| `computer_scroll` | scroll wheel (dx/dy) |

## slash command

`/computer <action description>` —— see `commands/computer.md`.
