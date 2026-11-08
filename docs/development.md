# 开发指南

> 构建、检查、测试与端到端门禁。项目约定（分层、注释规范、能力覆盖
> 检查表）见 [AGENTS.md](../AGENTS.md)；测试分层与覆盖矩阵见
> [tests/COVERAGE.md](../tests/COVERAGE.md)。

## 常用命令

```bash
make check                                            # cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./tests/run_all.sh functional                         # 能力覆盖层
./scripts/e2e.sh                                      # 端到端冒烟
```

examples 和 e2e 都走 `ZHUYE_MODEL=mock` 离线模型，不需要 API key。

## 更多入口

| 入口 | 内容 |
|------|------|
| [AGENTS.md](../AGENTS.md) | 项目概览、开发流程检查表、验证矩阵、约定 |
| [tests/COVERAGE.md](../tests/COVERAGE.md) | 「能力 × 覆盖层」映射矩阵 |
| [docs/release.md](release.md) | SDK 发版流程（PyPI / npm） |
| [scripts/](../scripts/) | bench / full_link / discussion_smoke 等辅助脚本 |
