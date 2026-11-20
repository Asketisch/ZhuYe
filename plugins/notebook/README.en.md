# notebook plugin (gated)

Plugin-form delivery of the `notebook_edit` tool: cell-level editing
(replace / insert / delete) of Jupyter `.ipynb` files, integrated with the
`read_before_edit` hook's `WRITE_TOOLS` allowlist (notebooks not read first
are blocked).

## Form: gated (on by default)

Per the "form judgment" iron rule, Jupyter cell editing is domain-specific
(data science) and should be a plugin; to keep external usage
byte-compatible (tool name / hook integration / default availability
unchanged) it uses the **gated** form (precedent: `goal-queue`): the tool
body stays in an engine crate (`zhuye-tools`), decided by this plugin's
enable state, **on by default**.

## Two ways to turn it off (different granularity)

| Entry | How | Effect |
|---|---|---|
| Plugin-level (registration gate) | `zhuye plugin disable notebook@inline`, or config `[plugins].disabled_plugins = ["notebook@inline"]` | Tool **not registered**: `tool_search` cannot find it, calls get tool not found |
| Tool-name-level (visibility filter) | config `[active] disallowed_tools = ["notebook_edit"]` | Tool stays registered but is **invisible to the LLM** |

Layering: registration gate → visibility filter → execution-time approval.

## Install / enable

The gate does not depend on installation; installing enables `plugin
enable` management and the `/notebook` command:

```bash
zhuye plugin install plugins/notebook --scope user
zhuye plugin enable notebook@inline
```
