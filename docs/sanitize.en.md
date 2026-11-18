# Output Sanitization

<p align="center">  <a href="sanitize.md">中文</a> · <b>English</b>  </p>

> Implementation in `zhuye-tools/src/sanitize/mod.rs` and
> `zhuye-sanitize` (standalone pattern library); config schema in
> `zhuye-config/src/schema/mod.rs::SanitizeSection`.

## 1. Purpose

Tool output / rollout persistence / telemetry all flow through the
`Sanitizer` uniformly: secret-like content is replaced with
placeholders **before it leaves the process boundary**, so API keys,
private keys, etc. never hit the JSONL on disk or enter the LLM context.

The default marker is `[REDACTED]`. Most patterns carry a type suffix,
written as `[REDACTED:<type>]`, so you can later tell which kind of
secret it was; a few replace only the value and keep the surrounding
structure.

## 2. Default Patterns (10)

In actual matching order (the return order of `default_patterns()`):

| id | matches | replaced with |
|---|---|---|
| `AWS_ACCESS_KEY` | `AKIA` / `ASIA` prefix + 16 uppercase alphanumerics (20 total) | `[REDACTED:aws_key]` |
| `ANTHROPIC_KEY` | `sk-ant-…` (≥20) | `[REDACTED:anthropic_key]` |
| `OPENAI_KEY` | `sk-…` (≥20) | `[REDACTED:openai_key]` |
| `GITHUB_TOKEN` | `ghp_` / `gho_` / `ghs_` / `ghr_` / `ghu_` prefix (≥30) | `[REDACTED:github_token]` |
| `PRIVATE_KEY_BLOCK` | a whole `-----BEGIN … PRIVATE KEY-----` block | `[REDACTED:private_key]` |
| `JWT` | `eyJ….eyJ….…` three segments | `[REDACTED:jwt]` |
| `DB_URL` | connection strings of `postgres://` / `postgresql://` / `mysql://` / `mongodb://` / `mongodb+srv://` / `redis://` / `amqp://` / `amqps://` (including the credential part) | scheme kept + `[REDACTED]` |
| `SLACK_TOKEN` | `xox[abprs]-…` | `[REDACTED:slack_token]` |
| `BEARER_TOKEN` | `Bearer …` (case-insensitive) | `Bearer [REDACTED]` |
| `KEY_ASSIGN` | `(key|token|secret|password|credential|passwd|pwd) = value` assignment | key kept + `[REDACTED]` |

The last three (`DB_URL` / `BEARER_TOKEN` / `KEY_ASSIGN`) use the **bare
marker** — they must keep the scheme, the `Bearer ` prefix, or the key
name so context readability is not destroyed.

Order matters: concrete provider keys precede `KEY_ASSIGN`;
`ANTHROPIC_KEY` precedes `OPENAI_KEY` (`sk-ant-…` is a subset of
`sk-…`). The `KEY_ASSIGN` value character class excludes `[` and `]`,
so `[REDACTED]` is never re-matched (the Rust regex crate has no
look-around; an explicit prefix capture group is used instead).

## 3. Configuration (config.toml `[sanitize]`)

```toml
[sanitize]
enabled = true                          # master switch
marker = "[HIDDEN]"                     # custom replacement placeholder (default [REDACTED])
disable_default_patterns = false        # turn off the 10 defaults (advanced users)
extra_patterns = [                      # append custom regexes (whole match replaced)
  "(?i)\\bmy_token\\s*=\\s*\\S+",
]
```

`marker` is a **prefix**, not a complete placeholder: with
`[HIDDEN]`, typed patterns become `[HIDDEN:aws_key]` and bare-marker
patterns become `[HIDDEN]`.

A regex that fails to compile errors at startup (fail-fast, never
silently skipped).

## 4. Application Points

- tool output before it enters context (the `ToolOutput` serialization path);
- rollout JSONL before it is written (entries over 16 KiB are truncated first, then sanitized);
- telemetry / LLM trace (`~/.zhuye/traces/model-io/`) before it is written.

Protection on the **source** side of secrets is a separate layer: MCP
sub-process `env_clear()` passes only a whitelisted env; credentials
support env references instead of plain text in config. The two layers
stack — no leakage at the source + a safety net at the exit.

## 5. Prompt Injection Detection (config.toml `[prompt_guard]`)

Complementary to the **output-side** secret sanitization above,
`zhuye-sanitize::prompt_guard` scans for injection **before untrusted
text enters the model context**. **Enabled by default since F01**
(earlier it was opt-in). Two runtime wiring points:

- **tool output** (`ToolExecutionQueue::execute_single`, after
  sanitization, before PostToolUse);
- **tool definitions** (`ToolRegistry::register_with_source_floor` /
  `register_if_absent_with_floor`; external MCP / plugin / runtime tools
  are scanned on name+description at registration; builtin
  descriptions are compile-time constants and skip this scan).

```toml
[prompt_guard]
enabled = false     # default true (on by default since F01); set false to explicitly disable
# env ZHUYE_PROMPT_GUARD=1/0 force-on/force-off, takes precedence over config
```

Three verdicts (any block hit ⇒ Block; otherwise any review hit ⇒
Review):

- **Block** — classic injection directives ("ignore all previous
  instructions", stealing the system prompt, exfiltrating credentials):
  - tool output: the whole entry is replaced with a guard error
    (rule code + SHA-256 fingerprint); the original text never enters
    the model context and `tool_call_end` is marked `is_error=true`;
  - tool definitions: **registration is refused** (fail-closed; the
    injection phrasing never enters the model schema).
- **Review** — suspicious but not conclusive (role-takeover phrasing,
  invisible characters): the body / definition is kept as-is; tool
    output gets `verdict="review"` + rule code + fingerprint in
    `metadata.prompt_guard`, for audit and higher-level adjudication;
    tool definitions are allowed to register but are counted.
- **Allow** — no hit; output unchanged.

Deliberately **not applied** to direct instructions typed by the user
themselves (that is the permission layer's job); the `UserPromptSubmit`
hook and this layer do not replace each other. The fingerprint
algorithm matches the audit log / rollout (lowercase hex SHA-256),
supporting cross-layer correlation of "which input triggered which
block". Verdict counters on both sides flow into
`zhuye_sanitize::guard_stats::GuardStats` (definition side in the
registry, output side in the queue, sharing one instance), shown in the
CLI `/status` security section.
