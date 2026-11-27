# orchestration 层 AGENTS.md

> **何时读我**：在 `crates/orchestration/` 下工作——加或改子代理工厂、角色讨论、任务与团队、DAG 流水线、目标模式这类多 agent 编排原语时，读本篇。

## 层定位

中上层的**多 agent 编排**层：把「一个模型 + 一套工具」的单元扩成多个 agent 的协作结构。五个原语各自独立成 crate，互不吞并。`docs/features.md` 的「编排五原语」一节是这一层的系统说明。

## crate 速览

本层五个 crate 中 `zhuye-task` 已有独立篇，其余四个 crate **没有独立的 crate 篇**，本地契约集中写在本篇。

| crate | 一句话职责 | 本地契约在哪 |
|-------|-----------|------------|
| `zhuye-subagent` | Tool-per-Agent 子代理工厂与 session forking，对父级 LLM 以工具形式暴露 | 本篇「子代理工厂」节 |
| `zhuye-discussion` | 多角色讨论编排器与进程内消息总线 | 本篇「角色讨论」节 |
| `zhuye-task` | 结构化任务、团队 / squad 委派与 Coordinator 模式 | [zhuye-task/](zhuye-task/AGENTS.md) |
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

- `call_<role>` 可传 `session_id` 续派同一工厂中已完成的同角色子会话;保留其历史、工具与工作区,同时按当前父任务账本计费。运行中、角色或工具不匹配、隔离或协调模式变化、已中断或已淘汰的会话拒绝续派;进程内最多保留 64 个会话,不承诺跨进程续派。
- 工作区共享时继承父会话当前工作区;显式注入 worktree 隔离后,创建失败必须拒绝派发并释放槽位,不可静默回退共享目录。子会话元数据记录实际执行目录,供恢复定位。
- 父历史尾部保留跨边界的工具调用与结果;按实际消息计数,选中工具结果时也会传递它后面的来源说明。
- 子会话工具白名单保留父注册表中的工具来源及权限包装;coordinator 按原有白名单筛选。
- 子代理派发先在当前根任务账本预留派发次数;创建失败或取消释放预留,成功派发后计入总数并把同一账本传给子回合。

- 并发在途（in-flight）槽位上限常量 `MAX_DEPTH` 定义在 `crates/orchestration/zhuye-subagent/src/lib.rs`。它是**在途**计数而非累计 spawn 数：子代理终态回收后槽位自动释放。
- 深度与并发是两件事：`MAX_DEPTH` 只管在途槽位，递归深度由治理模块另行守卫，两者不要混用同一个计数器。
- **派发有两种模式**（`call_<role>` 的 `mode` 参数，缺省 `sync` = 旧行为）：
  - `sync`：父回合阻塞等子代理结束，结果作为本次工具调用的回执。
  - `async`：立即返回占位回执，子代理交给后台任务继续跑；结果由引擎在回合收尾前**挂起等待**并注入本轮上下文（见 `crates/runtime/zhuye-core/src/graph/nodes/check_stop.rs`）。
- `async` 的四条约束：①登记表在 `zhuye_tools::PendingSubagentRegistry`（**每线程一份**，工具侧经 `ToolContext.pending_subagents` 写、引擎侧经 `NodeContext.pending_subagents` 读）；②`SpawnedChild` 必须移交给后台任务 —— 它析构会 cancel 子令牌，丢在工具 future 里等于当场杀掉子代理；③后台任务须同时监听条目的放弃句柄与**父回合令牌**（`ToolContext.cancel`，队列注入的就是回合级令牌），任一触发即丢弃 `SpawnedChild`；④登记表缺席（纯库 / 测试）时**降级为同步**，不能给模型一个「已派发但没人回填」的假回执。
- **异步子代理不跨越派发它的回合**：`zhuye-core` 在回合任务收尾时调 `PendingSubagentRegistry::abandon_turn`（按回合令牌匹配），仍在表的条目被移除并触发放弃句柄。所以「打断父回合 ⇒ 取消在途子代理」在 async 下同样成立 —— 这条级联不是自动的：同步模式靠工具 future 被丢弃，async 的后台任务不随工具 future 析构，去掉 ③ 的监听子代理就会在用户中断后继续跑。
- **登记表按派发回合隔离**：登记表是**每线程一份**，而同一线程上可能并发跑着多个回合（每条 UserInput 都 spawn 一个回合任务）。引擎侧的等待与取走一律按**派发回合的令牌**筛选（`has_pending_for` / `wait_any_for`），否则 A 回合派发的子代理会被 B 回合等到，结果被注入 B 的上下文。

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
