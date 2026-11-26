# web-search plugin (gated)

Plugin-form delivery of the `web_search` tool: web search via the Brave
Search API, with sanitized result URLs injected into context and `Auto`
approval (read-only, no side effects).

## Form: gated (on by default)

Per the "form judgment" iron rule, `web_search` binds to the Brave
third-party API (external system integration) and should be a plugin; but it
was previously an unconditional builtin, so to keep external usage
byte-compatible (tool name / approval / default availability unchanged) it
uses the **gated** form (precedent: `goal-queue`): the tool body stays in an
engine crate (`zhuye-tools`), and whether it registers is decided by this
plugin's enable state.

- **On by default** (`BUNDLED_PLUGIN_DEFAULTS`): with empty config,
  behavior is identical to before pluginization.
- **API key**: `[web_search]` config section or `BRAVE_API_KEY` env var,
  semantics unchanged.

## Two ways to turn it off (different granularity)

| Entry | How | Effect |
|---|---|---|
| Plugin-level (registration gate) | `zhuye plugin disable web-search@inline`, or config `[plugins].disabled_plugins = ["web-search@inline"]` | Tool **not registered**: absent from tools list, `tool_search` cannot find it, calls get tool not found |
| Tool-name-level (visibility filter) | config `[active] disallowed_tools = ["web_search"]` | Tool stays registered but is **invisible to the LLM** |

Layering: registration gate → visibility filter → execution-time approval.

## Install / enable

The gate does not depend on installation; installing enables `plugin
enable` management and the `/web-search` command:

```bash
zhuye plugin install plugins/web-search --scope user
zhuye plugin enable web-search@inline
```

## Future work

The multi-backend failover chain in `zhuye-tools/src/guards/web_guard.rs`
(keyless DuckDuckGo / SearXNG / Brave / Tavily / custom) is pre-wired and
will migrate into this plugin as its capability evolves.
