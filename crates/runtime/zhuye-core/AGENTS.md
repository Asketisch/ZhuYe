# zhuye-core AGENTS.md

> **何时读我**：改 `AgentThread` / StateGraph 引擎、回合级上下文（`NodeContext`）、节点实现、side-channel 旁路 agent 或会话恢复逻辑时，读本篇。

## 模块概述

运行时引擎本体：`AgentThread` 持有 submission 通道，`submission_loop` 消费 `Submission` 并驱动 4 节点 StateGraph 跑完一轮 turn，全程经协议层发事件。

## 核心文件导航

| 文件 | 一句话职责 |
|------|-----------|
| `src/graph/mod.rs` | `GraphNode` 四节点枚举与 `next` 转移表，`StateGraph::run` 主循环 |
| `src/graph/nodes/` | 四个节点的实现：`pre_loop` / `model_call` / `tool_exec` / `check_stop`；`nudge` / `retry` 是节点内复用的子例程模块，**不是**图节点 |
| `src/graph/state.rs` | `AgentState` 回合状态载体，节点间传递 |
| `src/submission_loop.rs` | 消费 `Submission`、组装 `NodeContext`、驱动 StateGraph 并发射事件 |
| `src/agent_thread.rs` | `AgentThread`：持有 submission 通道与每 turn / 每 session 的事件分发 |
| `src/side_channel/` | side-channel 旁路 agent 的注册表与驱动（并发旁路） |
| `src/resume.rs` | `RolloutRecord` → `ChatMessage` 的纯映射，恢复历史用 |
| `src/steering_queue.rs` | 下一轮 steer 输入队列 |

## 本地契约与同步点

- **StateGraph 是手写 `enum` + `match`**，转移表集中在 `src/graph/mod.rs` 的 `GraphNode::next`；本 crate 的 `Cargo.toml` 不依赖 petgraph 等图算法库，换成库实现属于架构级改动。
- **节点提前终止有三态**：自然结束（`CheckStop` 终止时置 `state.completed_normally`）、取消（`CancellationToken` 触发）、错误（`model_call` / `tool_exec` 返回 `None`）。只有自然结束算正常完成，取消与错误都不置该标记。
- **`NodeContext` 是回合级依赖的唯一装配点**：定义与组装都在 `src/submission_loop.rs`。它携带 model spec、路由策略、model registry、`HookEngine`、工具执行队列、事件通道、取消令牌、审批 gate 等。
- **Stop hook 否决续跑链路**在 `src/graph/nodes/check_stop.rs`：节点派发 `HookEvent::Stop`，被否决时累加尝试计数、把 hook 注入的提醒投递给下一轮，并让循环回到 `PreLoop` 强制续作；只有放行才终止本轮。
- **SessionMeta 落盘前的通告不持久化**（`src/submission_loop.rs`）：权限模式切换 / plan 派发发生在会话头未写（`meta_written=false`）时只广播内存事件，待会话头落地后按需补记 —— 保证 JSONL 物理首行是 `SessionMeta`，rollout 索引依赖此前置（索引侧的恢复兜底见 resources 层篇 `zhuye-rollout` 条目）。
- **side-channel 并发上限**由 `src/side_channel/registry.rs` 的 `MAX_CONCURRENT` 约束，它与子代理工厂的在途上限是两套独立计数器。
- 改了节点行为或事件发射顺序，要同步 `tests/functional/` 下对应的能力用例（断言到字段级），并按根 `AGENTS.md` 的验证矩阵跑 `./tests/run_all.sh functional`。

## 禁止事项

1. 不要引入 petgraph 或其他图算法库——四节点转移是手写 `enum` + `match`。
2. 不要在节点里直接写 stdout / 网络；事件一律经 `ctx.event_tx` 走协议层。
3. 不要把 `NodeContext` 的字段拆到别的模块——它是回合依赖的单一装配点，热重载与审批都依赖这份结构。
4. 不要绕过 `ToolExecutionQueue` 直接执行工具调用。
5. 不要在 `src/graph/nodes/` 之外新增节点实现；节点集合与转移表是本 crate 的对外契约。
6. 不要在本 crate 引用 `zhuye-exec` / `zhuye-cli` / `zhuye` / `zhuye-py`。

## 注意事项

- `NodeContext` 里的 model spec 是 `Arc<RwLock<String>>` 共享副本，`zhuye-exec` 的配置热重载靠它切模型；改这个字段的类型会波及热重载路径。
- `src/resume.rs` 是纯映射，不碰 IO；恢复流程的落盘侧在 `zhuye-rollout`。
- 审批 gate 在 headless 免确认路径下是 `None`，不要假设它一定存在。
- 注释与文档一律中文；库 crate 用 `thiserror`。

## 深入阅读

- [docs/architecture.md](../../../docs/architecture.md) —— §4 一轮执行与状态转移、§5 节点上下文与能力装配
