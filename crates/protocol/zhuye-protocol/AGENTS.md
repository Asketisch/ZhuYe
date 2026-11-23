# zhuye-protocol AGENTS.md

> **何时读我**：要改 `Op` / `EventMsg` / `Submission` / `Item` 等对外数据类型、加新协议变体、或排查 SDK 与协议不一致时，读本篇。

## 模块概述

全仓库唯一定契约层的实现 crate：定义客户端与引擎之间的全部数据类型与 `RolloutRecorder` trait，不含任何运行时行为，也不依赖仓库内其他 crate。

## 核心文件导航

| 文件 | 一句话职责 |
|------|-----------|
| `src/op.rs` | `Op` 操作枚举（客户端 → 引擎），serde 以 `type` 标签（snake_case）编码 |
| `src/op_prompt.rs` | `Op::PromptCommand`（`/prompt`）的 `PromptAction` 与 `PromptSelection` 三态选择策略（default / auto / manual） |
| `src/event_msg/mod.rs` | `EventMsg` 事件枚举本体；各域负载类型拆到同目录子模块 |
| `src/event.rs` | `Event` 外壳：事件 id + 消息体 |
| `src/submission.rs` | `Submission` 提交信封，一次提交的请求单元 |
| `src/item.rs` | 会话与输入项数据类型，`ToolError` 也定义在此 |
| `src/recorder.rs` | `RolloutRecorder` trait 与 `RolloutRecord` 持久化记录 |
| `src/error.rs` | `ProtocolError` 协议层错误类型 |
| `src/lib.rs` | 模块声明与对外 re-export 汇总 |

## 本地契约与同步点

- **`Op` 枚举形态是发布门禁的解析输入**：`scripts/release_sdk.sh` 的 `check_docs` 从 `src/op.rs` 读入，以「列 0 的 `}`」判定枚举边界，再用 `^    ([A-Z][A-Za-z0-9]+)` 提取变体名；边界解析不到或提不出变体，脚本直接 FAIL。
- **新增 `Op` 变体的同步链路**：`src/op.rs` → `sdks/python/README.md` 与 `sdks/typescript/README.md` 的 Op 参考表 → `sdks/python/zhuye/protocol.py` 的 `Literal["..."]` → `sdks/typescript/src/protocol.ts` 的 `type: '...'` → （wire 语义变化时）`sdks/PROTOCOL.md`。
- **新增 `EventMsg` 变体的同步链路**：枚举本体加在 `src/event_msg/mod.rs`，负载类型放同目录子模块；随后同步双 README 的 EventMsg 参考表与双 SDK 类型层。注意 `check_docs` 只解析 `Op`，EventMsg 变体靠这套流程人工同步，脚本不会替你兜底。
- **带 action 载荷的 `Op` 变体拆独立模块**：`PromptCommand` 的 `PromptAction` / `PromptSelection` 定义在 `src/op_prompt.rs`，应答事件体（`PromptList` / `PromptSwitched`）在 `event_msg` 域 —— 既守住「枚举体外定义」的禁止事项，也让 `check_docs` 不误判变体。新增同类变体照此办理。
- **门禁命令**：`./scripts/release_sdk.sh check_docs`。协议文档落后于源码，会在 `check` / `tag` 发布前直接失败。
- **覆盖矩阵**：新增或移除协议能力时同步 `tests/COVERAGE.md` 的「能力 × 覆盖层」映射表。
- `McpServerFailedEvent.will_retry` 只能报告实际的自动重试；当前各传输启动失败均不自动重试。
- **`TokenUsage` 的不变式**（`src/event_msg/turn.rs`）：`input_tokens` 是「本次 prompt 的全部输入」，`cached_tokens` / `cache_write_tokens` 都是它的**子集**，`total_tokens = input_tokens + output_tokens`。Anthropic 线缆的 `input_tokens` 不含缓存分段，由 provider 适配层在上报前折入（`TokenUsage::fold_cache_segments`）。读取历史 rollout 时用 `TokenUsage::repair_unfolded_cache_segments` 兼容未折入的旧记录（特征：缓存分段之和 > `input_tokens`；幂等）。改动任何 usage 字段语义前，先确认 `zhuye-llm` 的适配器、`zhuye-rollout` 的聚合器与 `pricing::price` 三处口径一致。
- **同一事件变体不得承载两种语义**：`TokenCount` 既发「单次调用快照」又发「回合累计」，靠 `turn_total: bool`（`src/event_msg/agent.rs`）区分 —— 缺省 `false` = 单次调用，单次调用时序列化省略该字段（wire 与引入前逐字节一致）。新增这类「同型不同义」的事件时必须给出显式判据字段，否则消费方只能靠「哪些字段为空」猜，猜错即静默双计。

## 禁止事项

1. 不要改动既有变体的字段名、字段类型或 wire 名——它们是已发布的对外契约。
2. 不要在 `Op` 枚举体内部再定义 enum / struct——`check_docs` 会把它们误当成新 Op 变体（`PlanApprovalChoice` / `UserInputItem` 因此定义在枚举体之外）。
3. 不要改变 `Op` 枚举体的收尾形态（列 0 的 `}`）——门禁脚本靠它判定枚举边界。
4. 不要把新变体插在既有变体中间；新变体追加到枚举末尾，保持既有变体的相对顺序不变。
5. 不要在本 crate 引入仓库内其他 crate 的依赖，也不要在此塞入运行时行为（IO、事件循环、配置读取）。
6. 不要用「退出码 0」「不抛错」代替能力断言——协议层改动的断言必须到字段级。

## 注意事项

- serde 采用 `tag = "type"` + `rename_all = "snake_case"`；协议冻结的前提就是客户端必须容忍未知 `type`，改标签方式等同于破坏冻结。
- 既有变体的可选字段用 `#[serde(default)]` / `skip_serializing_if` 处理，调整字段时要保持同样的可选性语义。
- `src/text.rs` 的截断函数按字符边界切分（多字节安全），改动会影响 wire 上文本的可见长度。
- 注释与文档一律中文；库 crate 用 `thiserror`。

## 深入阅读

- [docs/architecture.md](../../../docs/architecture.md) —— §3 协议与会话生命周期、§3 的 v0 冻结与 additive 规则
- [sdks/PROTOCOL.md](../../../sdks/PROTOCOL.md) —— wire 语义与双 SDK 的对应关系
