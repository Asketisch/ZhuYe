# orchestration 层 AGENTS.md

> **何时读我**：在 `crates/orchestration/` 下工作——加或改子代理工厂、角色讨论、任务与团队、DAG 流水线、目标模式这类多 agent 编排原语时，读本篇。

## 层定位

中上层的**多 agent 编排**层：把「一个模型 + 一套工具」的单元扩成多个 agent 的协作结构。五个原语各自独立成 crate，互不吞并。`docs/features.md` 的「编排五原语」一节是这一层的系统说明。

## crate 速览

本层五个 crate 目前**没有独立的 crate 篇**，本地契约集中写在本篇。

| crate | 一句话职责 | 本地契约在哪 |
|-------|-----------|------------|
| `zhuye-subagent` | Tool-per-Agent 子代理工厂与 session forking，对父级 LLM 以工具形式暴露 | 本篇「子代理工厂」节 |
| `zhuye-discussion` | 多角色讨论编排器与进程内消息总线 | 本篇「角色讨论」节 |
| `zhuye-task` | 结构化任务、团队 / squad 委派与 Coordinator 模式 | 本篇「任务与团队」节 |
| `zhuye-pipeline` | DAG 驱动的多阶段流水线运行时 | 本篇「流水线」节 |
| `zhuye-goal` | 目标模式：每轮结束自校验，未达成则生成续作 | 本篇「目标模式」节 |

## 层内依赖约定

- 依赖方向：`zhuye-task` → `zhuye-subagent`；`zhuye-pipeline` → `zhuye-task` + `zhuye-subagent`；`zhuye-discussion` → `zhuye-subagent`；`zhuye-goal` 不依赖本层其他 crate。
- 禁止反向依赖：`zhuye-subagent` 不得引用 task / pipeline / discussion / goal——它是本层的公共底座，被其余四个复用。
- 依赖事实以各 crate 的 `Cargo.toml` 为准，`docs/architecture.md` §2 的分层示意是理想模型；`zhuye-core` 目前反向依赖 `zhuye-subagent` / `zhuye-goal`，改动前先看实况。

## 跨层边界

- 本层不定义 wire 协议。对客户端可见的编排进度一律经 `zhuye-protocol` 的 `EventMsg` 变体发射，不要在本层自造事件类型。
- 本层不直接实现 LLM 调用。`zhuye-discussion` 的编排器通过 `prompt_for` 闭包把「跑一轮 LLM」交给调用方注入，便于单测替换。
- 子代理工厂 spawn 的是 `zhuye-core` 的 `AgentThread`，即本层复用 runtime 层已有的执行引擎，而不是另起一套循环。

## 子代理工厂

- 并发在途（in-flight）槽位上限常量 `MAX_DEPTH` 定义在 `crates/orchestration/zhuye-subagent/src/lib.rs`。它是**在途**计数而非累计 spawn 数：子代理终态回收后槽位自动释放。
- 深度与并发是两件事：`MAX_DEPTH` 只管在途槽位，递归深度由治理模块另行守卫，两者不要混用同一个计数器。

## 角色讨论

- 讨论编排不直接调 LLM，也不自己实现 transcript 持久化——总线与落盘在 `message_bus`，LLM 步进由 `prompt_for` 闭包注入。

## 任务与团队

- 任务 / 团队的持久化与内存两套 store 是并行的：CLI 走落盘 store，单测与临时运行走内存 store。

## 流水线

- 流水线阶段以 DAG 声明，节点能力来自 `zhuye-task` 的 team 节点与 `zhuye-subagent` 的 spawn 能力；新增阶段不要绕过这两者另建 spawn 通道。

## 目标模式

- 目标模式只负责「自校验 + 续作文本」，是否真的开下一轮由引擎决定；它不自行驱动事件循环。

## 本层通用规则

1. **新增编排原语的固定套路**：独立 crate → 门面 re-export → CLI 子命令 → `tests/functional/` 下的能力用例。四步缺一不可，细则见根 `AGENTS.md` 的开发流程检查表。
2. 能力覆盖用例只写在 `tests/functional/`，断言到字段级；不要在 SDK 测试里重复协议断言。
3. 本层 crate 内部状态机尽量不依赖具体 LLM 实现，便于用 stub 闭包做单测。
4. 注释与文档一律中文；库 crate 用 `thiserror`。
5. 改了跨客户端可见的编排行为，按根 `AGENTS.md` 的验证矩阵跑 `./tests/run_all.sh functional`。

## 深入阅读

- [docs/features.md](../../docs/features.md) —— 「编排五原语」一节
- [docs/architecture.md](../../docs/architecture.md) —— §2 workspace 六层分层、§4 一轮执行与状态转移
