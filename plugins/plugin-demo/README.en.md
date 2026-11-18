# The ZhuYe sample plugin: `demo`

<p align="center">  <a href="README.md">中文</a> · <b>English</b>  </p>

A demo plugin with one minimal runnable sample for each capability
(five directory-declared types + a WASM component; sample source in
`wasm/rust/`, built via `scripts/build_plugin_demo_wasm.sh`):

| Capability | File | Effect |
|---|---|---|
| commands | `commands/hello.md` | `/demo:hello 张三` expands into the command body and is submitted |
| agents | `agents/reviewer.md` | the model can call the `call_demo_reviewer` sub-agent |
| skills | `skills/demo-skill/SKILL.md` | a demo skill the model can activate via `load_skill` |
| hooks | `hooks/hooks.json` | logs one line to stderr after every tool call (shell hook); the `wasm` entry is an in-process component hook (injects a reminder on PostToolUse) |
| mcp_servers | `.mcp.json` + `servers/echo_server.py` | the `mcp__plugin:demo@inline:echo__echo` echo tool |
| wasm_tools | `bin/*.wasm` (build artifacts, not checked in) | the `demo_echo` in-process tool + a WASM hook (the component implements the `wit/plugin.wit` world) |

## Install and enable

```bash
# 1. Install (copied into ~/.zhuye/plugins/cache and registered)
zhuye plugin install plugins/plugin-demo

# 2. Enable (written back into config.toml [plugins].enabled_plugins;
#    the id must be the full name@marketplace; for a local install it is demo@inline)
zhuye plugin enable demo@inline

# 3. Verify
zhuye plugin list
zhuye exec "/demo:hello 张三"
```

You can also reference it locally without installing: after adding
`demo@inline` to `[plugins].enabled_plugins` in
`~/.zhuye/config.toml`, just point `zhuye plugin install` at this
directory.

## MCP server notes

The executable entry in `.mcp.json` uses the `${PLUGIN_ROOT}`
placeholder — expanded at install time into the plugin's absolute
cache path, so it stays valid after the plugin is moved / copied. The
echo server only uses the Python standard library; no `pip install`
needed.

## Security model

- Plugin hooks are external processes (`ShellHook`): the event JSON goes
  over stdin, the decision JSON over stdout; timeout / non-zero exit
  always fail closed (Deny).
- Tools registered by plugins (agents / MCP) have the `Prompt`
  permission floor: even if declared `Auto`, they are raised, and user
  approval is required before a call.
- The `/demo:hello` command is only a prompt template substitution; it
  registers no tools and does not change the model's visible surface.
