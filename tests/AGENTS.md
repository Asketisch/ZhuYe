**何时读我:** 当你要新增 / 修改测试用例、调整测试分层口径、同步覆盖矩阵或运行测试套件时读本篇 —— 本篇说明各分区测什么、断言写在哪一层、入口有哪些子命令。

# tests/ —— 全仓测试套件

## 评测基线

真实任务评测的固定任务集 / 运行器 / 质量门禁在 `tests/eval/`;
基线与前后对比的操作手册见 [docs/eval-baseline.md](../docs/eval-baseline.md)。
离线自检(`bash tests/eval/run_offline.sh`,已并入 `functional` 套件)用 mock
provider 验证流水线可用 —— **不衡量模型能力**,真实基线必须走
`tests/eval/run_quality.sh`(真模型 + 凭据)。

## 目录职责

本目录是仓库的统一测试入口:用分层的方式覆盖 Rust 框架层与 Python / TypeScript 两个 SDK 的行为,确定性套件全程离线(隔离 HOME + mock 模型),不依赖外网与 API key。

> 本篇正文里的文件路径一律相对仓库根目录。

## 分区速览表

| 分区 | 一句话职责 |
|---|---|
| `tests/functional/` | 能力覆盖层(主):Python 驱动真二进制走原始 JSONL wire,字段级断言 |
| `tests/rust/` | CLI 参数面的老 bash 套件(参数矩阵与输出契约) |
| `tests/sdk-python/`、`tests/sdk-ts/` | 双 SDK 真二进制绑定层:只测两个客户端自己的行为 |
| `tests/native-python/` | 原生 Python 绑定:安装实际构建的 PyO3 wheel 并验证 |
| `tests/live/` | 实模型演练层:真实模型下的引擎行为,默认 SKIP |
| `tests/contracts/` | 协议契约样本(EventMsg 全变体线格式) |
| `tests/eval/` | 评测用例与质量跑批 |
| `tests/verification/` | 事故回放与核心语义差分:新旧二进制在同一确定性输入上对比语义投影;Protocol v0 加变体走 `event:<TYPE>` / `rollout:<TYPE>` 逐条批准 |
| `tests/run_all.sh`、`tests/lib.sh` | 套件入口与公共助手(隔离环境 / 断言助手 / 超时) |
| `tests/runner.py`、`tests/test_registry.py` | 按 `tests/PLAN.md` 登记任务到测试入口的统一调度器 |

## 各分区本地契约

### 能力断言的唯一去处

- 新增能力断言只落 `tests/functional/`,断言要到**字段级**(事件字段、落盘内容、工具成对);不接受「退出码 0」「不抛错」充当能力断言。
- `tests/functional/` 不经任何 SDK:直接起 `zhuye serve` 子进程、走原始 wire,避免 SDK 自身的 bug 遮住 Rust 的 bug。

### CLI 参数面不再加新能力

- `tests/rust/` 里是 CLI **参数面**的老 bash 套件(如 `tests/rust/run.sh`);新能力面统一用 Python 写,不要往这里加能力用例。
- 判定口径:断言「服务端发什么 / 落盘什么」→ 能力覆盖层;断言「某个 CLI 参数组合的输出契约」→ bash 参数面。

### SDK 行为断言的归属

- 两个客户端自己的行为(迭代器收尾、handler 调度、回执形状、close 语义)落 `tests/sdk-python/` 与 `tests/sdk-ts/`。
- SDK 纯逻辑测试(mock serve)落 `sdks/python/tests/` 与 `sdks/typescript/tests/`,随包发布。
- 协议 / 能力断言不在 SDK 测试里重复写。

### 入口子命令

`./tests/run_all.sh` 接受 7 个子命令:

| 子命令 | 覆盖范围 |
|---|---|
| `functional` | CI 门禁入口:能力层 + Rust 参数面 + 双 SDK + 原生 Python |
| `functional-layer` | 只跑能力覆盖层 |
| `rust` | 只跑 Rust 侧 |
| `sdk-python` | 只跑 Python SDK 绑定层 |
| `sdk-ts` | 只跑 TS SDK 绑定层 |
| `native-python` | 构建并安装 wheel,验证原生 Python 绑定 |
| `live` | 实模型演练层(真实模型;默认 SKIP,需 `ZHUYE_LIVE=1`) |

- `functional` 跳过 cargo test / clippy / fmt / doc(那部分由既有 CI job 覆盖),并对 Rust 参数面设置 `ZHUYE_SKIP_WORKSPACE=1`。
- 拼错套件名会在触发构建前直接报错退出。

### 离线与隔离

- `tests/lib.sh` 统一提供:mktemp 隔离 HOME、`ZHUYE_MODEL=mock` 的 mock provider 环境、断言助手(`assert_ok` / `assert_fail` / `assert_contains` / `assert_json`)、便携超时与失败汇总。
- 能力层缺必需产物(导入器 / 插件服务二进制 / mock MCP server / git)时**直接失败**,不允许以跳过冒充覆盖。

### 实模型演练层不是能力覆盖

- `tests/live/` 断言协议形状与运行时行为(事件序 / 终态 / usage 字段 / 工具成对),**不断言具体回答文本**。
- 它需要外网与额度,默认 SKIP,不得当作 CI 门禁,也不与能力覆盖层互相替代。
- `cli_e2e.sh`(**本地专用**,不进 CI / 门禁链):CLI headless 链路(`zhuye serve`)的**场景套件** —— 28 个场景覆盖 plan / goal / compact / cache / interact / session / ext 七组协议面;一场景一进程(崩溃互不牵连),每个检查项折成一条 tcase。两种模式:`ZHUYE_LIVE=1` 走 `cli_e2e.toml` 的真实模型,`ZHUYE_E2E_MOCK=1` 走内置 mock provider 离线自检(验证场景脚本本身)。隔离假 HOME + 独立工作区,配置模板 `cli_e2e.example.toml`(含各场景参数的中文注释),真实 toml 已 gitignore。免费模型池(pytest 用例)测协议形状;这个套件测**完整 agent 会话**。场景清单与断言口径见 `live/README.md`。

### 覆盖矩阵同步

- `tests/COVERAGE.md` 的「能力 × 覆盖层」矩阵随改动同步:新增能力补一行;移除能力移到「未覆盖项」并写明原因;发现的 SDK 缺陷记入「已知缺陷」。
- 该文件第 0 节给出「谁测什么」的分层口径与规模,本篇不重复其数字。

## 禁止事项

- 不用「退出码 0」或「不抛错」充当能力断言。
- 不在 SDK 测试里重复写协议 / 能力断言。
- 不向 `tests/rust/` 追加新能力用例。
- 不在确定性套件里依赖真实 HOME、外网或真实 API key。
- 不把实模型层当能力覆盖门禁。

## 深入阅读

- [COVERAGE.md](COVERAGE.md) —— 能力 × 覆盖层矩阵、分层口径与未覆盖项
- [README.md](README.md) —— 快速开始、前置依赖与四层分工
- [SCENARIOS.md](SCENARIOS.md) —— 按用户任务串联的场景与下一批缺口
- [run_all.sh](run_all.sh) —— 套件入口与子命令定义
