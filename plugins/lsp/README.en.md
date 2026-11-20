# lsp plugin (gated, config-driven activation)

Plugin-form delivery of the LSP integration: the `lsp` tool (definition /
references / diagnostics / hover — structured code navigation beyond
tree-sitter) plus `LspConnectionManager` (server process pool, diff
restart).

## Form: gated (activation always requires config)

Per the "form judgment" iron rule, LSP is external-system integration
(language server subprocesses) and was already config-gated via
`[lsp_servers]`. Pluginization keeps **config semantics unchanged**:

- **Activate**: configure `[lsp_servers.<name>]` in
  `~/.zhuye/config.toml` (command / args / languages; `zhuye lsp add`
  can write it) → servers start and the `lsp` tool registers. **With no
  server definitions, enabling the plugin does nothing** (the plugin
  carries no server definitions).
- **Coarse switch (new)**: `zhuye plugin disable lsp@inline`, or config
  `[plugins].disabled_plugins` containing `"lsp"` → LSP is **fully off**
  even with `[lsp_servers]` configured: no tool registration, no server
  startup.

| Entry | Effect |
|---|---|
| `[lsp_servers]` configured (default state) | LSP on (identical to before) |
| Plugin explicitly disabled | LSP fully off |
| Plugin enabled | No standalone effect (activation still follows config) |

## CLI

`zhuye lsp add / list / remove` only read/write the `[lsp_servers]`
section; the CLI surface is unchanged.
