# 测试实施计划

本文件是统一调度器的权威任务目录。计划包含三个独立产品;已实现的
任务映射到本仓现有测试入口,尚未实现的任务必须保持 `pending` 或
`conditional`,不能把跳过、零用例或平台缺口计为通过。

## 1. 公共基座

所有非 live 套件使用确定性模型、隔离用户目录、临时工作区、进程组超时、
复现 seed、JSON/JUnit 报告和脱敏日志。`shared.harness` 先检查 Python 版本、
工具链可发现性和报告目录可写性;注册表启动前做任务与选集引用完整性校验。

结果状态固定为 `pass`、`fail`、`skip`、`infra_error`、`timeout`、`pending`。
`pending` 与基础设施错误返回退出码 2;断言失败或超时返回 1。dry-run 只验证
调度计划,全部报告为 `skip`。

## 2. Runtime + 双 SDK 任务

| 任务 | 覆盖目标 | 状态 |
|---|---|---|
| `agent.static` / `agent.build` | 格式、lint、全目标编译 | 映射现有 Cargo 命令 |
| `agent.docs` / `agent.release` | 协议文档、版本与双包 dry-run | 映射 SDK 发布检查 |
| `agent.unit.*` | 引擎、上下文、配置状态机 | 映射核心 crate 单测 |
| `agent.contract.protocol` | 原始 JSONL wire 与事件字段 | 映射能力覆盖层 |
| `agent.contract.event` | `EventMsg` 全变体样本与 serde roundtrip | 映射协议契约测试 |
| `agent.contract.properties` | `Op` / `Submission` / `Event` 随机属性往返 | 映射 proptest |
| `agent.contract.cli` | headless CLI 参数面 | 映射 Rust/CLI 套件 |
| `agent.integration.*` | 工具、审批、扩展、编排、持久化 | 映射能力层与专项冒烟 |
| `agent.contract.provider` | provider 请求与流解析 | 映射 `zhuye-llm` 单测 |
| `agent.integration.provider` | 代理、TLS 与真实端点矩阵 | 条件项 |
| `sdk.*` | TS/Python 纯逻辑、真二进制、PyO3 | 映射 SDK 与原生绑定层 |
| `agent.security` | 权限、路径、凭据隔离 | 映射能力层安全断言 |
| `agent.fault` / `agent.fuzz` | 故障注入与模糊测试 | 只读存储、磁盘满、跨实例锁竞争、确定性变异与万级多重变异映射 |
| `agent.soak` | 高负载长稳 | env 锁压测 + 长会话压缩/重启映射 |
| `agent.performance` | 冷启动、首事件、RSS 基线 | 映射 bench |
| `agent.eval` | 真实模型协议与任务质量 | 条件项 |

`CASE_INDEX` 保留 A 系列编号,允许按单个规划用例选择所属任务。新增能力时,
能力断言优先落在 `tests/functional/`,SDK 只测语言绑定差异。

## 3. 执行与报告

选集固定为 `pr`、`smoke`、`main`、`nightly`、`release`。PR 覆盖静态、单元、
协议、确定性能力与双 SDK;主分支补真二进制、绑定和核心集成;夜间执行性能、
fuzz 与条件扩展;发布执行包检查、原生绑定和端到端冒烟。

报告写入 `.test-runner/report.json` 与 `junit.xml`,并记录 HEAD、工作树状态、
平台、架构、Python 版本、子模块指针、复现命令、日志路径和 seed。正式支持
平台若没有可执行映射,统一转为 `infra_error`。
