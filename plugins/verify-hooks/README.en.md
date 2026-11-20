# verify-hooks plugin (gated, off by default)

Plugin-form enable path for the `verification` / `test_runner` builtin
hooks.

- **verification** (Stop hook): runs a verify command (default
  `cargo test`, configurable via `[hooks.verification].test_command`)
  before the turn ends; failure vetoes completion and feeds output back.
- **test_runner** (PostToolUse hook): detects test-failure patterns in
  bash output and injects a system-reminder.

## Form: gated (off by default, dual path)

Per the "form judgment" iron rule these hooks are domain-specific (they
run shell commands in the host cwd) and were already opt-in via config;
the plugin stays **off by default**. Either enable path is equivalent:

| Path | How |
|---|---|
| config (unchanged) | config `[hooks] enabled = ["verification", "test_runner"]` |
| plugin (new) | `zhuye plugin install plugins/verify-hooks --scope user && zhuye plugin enable verify-hooks@inline` |

The plugin path also works for serve / SDK embedded scenarios with
explicit-only hook tightening (injection happens after the tightening).
Hook parameters still come from the `[hooks.verification]` /
`[hooks.test_runner]` sections; the plugin only decides *whether* they
are enabled.

## Disable

`zhuye plugin disable verify-hooks@inline` (or remove the names from
`[hooks] enabled` for the config path).
