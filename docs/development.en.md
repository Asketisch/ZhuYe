# Development Guide

<p align="center">  [中文](development.md) · **English**  </p>

> Building, checking, testing, and end-to-end gates. Project conventions
> (layering, comment rules, capability-coverage checklist) are in
> [AGENTS.md](../AGENTS.md); test layering and the coverage matrix are in
> [tests/COVERAGE.md](../tests/COVERAGE.md).

## Common Commands

```bash
make check                                            # cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./tests/run_all.sh functional                         # capability coverage layer
./scripts/e2e.sh                                      # end-to-end smoke
```

Examples and e2e both run on the offline `ZHUYE_MODEL=mock` model — no
API key needed.

## More Entry Points

| Entry | Contents |
|------|------|
| [AGENTS.md](../AGENTS.md) | project overview, dev-flow checklist, verification matrix, conventions |
| [tests/COVERAGE.md](../tests/COVERAGE.md) | "capability × coverage layer" mapping matrix |
| [docs/release.md](release.md) | SDK release process (PyPI / npm) |
| [scripts/](../scripts/) | bench / full_link / discussion_smoke and other helper scripts |
