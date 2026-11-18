# wit/ — ZhuYe WASM Plugin Contract (Authoritative Community Interface Definition)

<p align="center">  [中文](README.md) · **English**  </p>

`plugin.wit` defines the language-agnostic interface between the host
and WASM plugin components. Any toolchain that can produce a
**component-model .wasm** can implement it:

| Language | Toolchain | Maturity |
|---|---|---|
| Rust | wit-bindgen (`wasm32-wasip2` target) | production-ready (verified by this repo) |
| TypeScript / JS | jco (componentize-js) | interface-compatible, not verified by this repo |
| Python | componentize-py | interface-compatible, not verified by this repo |
| Go | tinygo + wasm-tools | interface-compatible, not verified by this repo |

The ABI deliberately uses **JSON strings in and out** (instead of
fine-grained WIT records): authors in dynamic languages can join
without generating strongly-typed bindings; the schema is defined by
the `plugin.wit` comments and `docs/plugins.md`.

## Versioning Policy

- package name `zhuye:plugin`, semantic versioning. **During `@0.x`,
  additive evolution is allowed** (new worlds / new optional exports);
  **breaking changes bump the major version** and keep the old world
  file coexisting (e.g. `plugin@0.1.0.wit` alongside
  `plugin@1.0.0.wit`).
- The host instantiates components by package name + version: unknown
  major version → explicit error (no panic, no silent downgrade);
  unknown minor (additive) → use the known exports, missing exports
  error out.
- Component imports (WIT `import`) are currently empty — plugins are
  pure-compute sandboxes with no host capabilities to call; if this
  opens up in the future (e.g. controlled FS reads), it evolves via a
  new world or explicit imports, with this document updated in sync.

## Timeout and Security Semantics

- every exported call has a host-enforced deadline (epoch interrupts);
  timeout = immediate truncation.
- hook verdicts: timeout / `Err` / malformed JSON / missing `action`
  are all **Deny** (fail-closed, same semantics as shell hooks).
- tool: timeout / `Err` → tool error (`is_error = true`), no panic.
- permissions: WASM tools follow the existing external-tool floor =
  `Prompt` — a component declaring `Auto` does not take effect (same
  baseline as MCP tools).
- sandbox boundary: components have no filesystem, no network, no
  environment variables, and no host capabilities beyond the clock by
  default.

## Building a Component (Rust Example)

```bash
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2   # standalone project, not part of the main workspace
# output: target/wasm32-wasip2/release/<name>.wasm → copy into the plugin's bin/
```

The `zhuye plugin new` scaffolding command is not implemented yet;
build manually with the target above. The full WASM tutorial is in the
"WASM Components" section of `docs/plugin-dev-guide.md`.
