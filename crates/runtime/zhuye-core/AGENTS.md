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
| `src/cache_warming.rs` | v1.x prompt cache 保温调度器:候选槽 / 单飞 / 代际失效 / 事件出口 |
| `src/side_channel/` | side-channel 旁路 agent 的注册表与驱动（并发旁路） |
| `src/resume.rs` | `RolloutRecord` → `ChatMessage` 的纯映射，恢复历史用 |
| `src/steering_queue.rs` | 下一轮 steer 输入队列 |
| `src/misc/` | 引擎杂项三组：`f63`(干跑预览 / 回合截止 / 系统提醒信封 / 迟到审批)、`f64`(sudo 凭据 / 通知投递栈 / 系统权限引导)、`f65`(ADR / 错误人话化 / CLI↔GUI 行为差异) |

## 本地契约与同步点

- **StateGraph 是手写 `enum` + `match`**，转移表集中在 `src/graph/mod.rs` 的 `GraphNode::next`；本 crate 的 `Cargo.toml` 不依赖 petgraph 等图算法库，换成库实现属于架构级改动。
- **节点提前终止有三态**：自然结束（`CheckStop` 终止且**回合令牌未触发**时置 `state.completed_normally`，判定在 `GraphNode::is_natural_stop`）、取消（`CancellationToken` 触发）、错误（`model_call` / `tool_exec` 返回 `None`）。只有自然结束算正常完成，取消与错误都不置该标记 —— `CheckStop` 在等待异步子代理时被取消同样返回 `None`，不算自然完成。
- **`NodeContext` 是回合级依赖的唯一装配点**：定义与组装都在 `src/submission_loop.rs`。它携带 model spec、路由策略、model registry、`HookEngine`、工具执行队列、事件通道、取消令牌、审批 gate、在飞异步子代理登记表（`pending_subagents`）等。
- **Stop hook 否决续跑链路**在 `src/graph/nodes/check_stop.rs`：节点派发 `HookEvent::Stop`，被否决时累加尝试计数、把 hook 注入的提醒投递给下一轮，并让循环回到 `PreLoop` 强制续作；只有放行才终止本轮。
- **异步子代理的挂起 / 激活**同样落在 `src/graph/nodes/check_stop.rs`：节点先查 `ctx.pending_subagents`（在飞异步子代理登记表，`zhuye_tools::PendingSubagentRegistry`）。有在飞项说明模型只是**还没拿到结果**，回合应挂起等待而非结束 —— `wait_any` 拿到结果后把结果作为尾部 user `<system-reminder>` 注入 `state.messages`，再回 `PreLoop` **在同一轮内**续跑。五条要点：①顺序上**先等子代理、再判 Stop hook**（让模型先看到结果再决定是否收尾）；②注入的结果**要落盘**（`role=user` 记录，与 steering 插话同格式、同回合）—— 引擎每轮从 rollout 重建历史，不落盘的结果下一轮就只剩「已派发」占位回执；正文走 `tool_exec::limit_model_tool_output`，与同步工具回执同一套上下文预算；③等待被取消时直接返回 `None`，不要为了绕开「`CheckStop` 的 `None` = 自然完成」而返回 `PreLoop` —— 那会让已取消的回合完整跑一遍前置流水线（hook / 压缩 / steering 收割），自然完成的判定已由 `is_natural_stop` 挡住；④**异步子代理不跨越派发它的回合**：回合任务在 `graph.run()` 返回后调 `pending_subagents.abandon_turn(&turn_cancel)`，放弃本回合仍登记在表的条目（正常收尾为空操作）。不做这一步，被打断的回合的子代理会继续跑完，结果被之后某个回合当成待回填结果注入；卡在审批上的子代理还会让此后每个回合收尾都为它挂起；⑤等待与取走**按回合令牌筛选**（`wait_any_for(&ctx.cancel, …)`）—— 登记表是每线程一份，同线程可能并发跑多个回合，无差别等待会让 A 回合的子代理结果注入 B 回合的上下文。
- **SessionMeta 落盘前的通告不持久化**（`src/submission_loop.rs`）：权限模式切换 / plan 派发发生在会话头未写（`meta_written=false`）时只广播内存事件，待会话头落地后按需补记 —— 保证 JSONL 物理首行是 `SessionMeta`，rollout 索引依赖此前置（索引侧的恢复兜底见 resources 层篇 `zhuye-rollout` 条目）。
- **assistant 内容按步骤增量落盘**：`graph/nodes/mod.rs::persist_assistant_step` 在每次 `model_call` 产出（文本 + 工具调用）和每批 `tool_exec` 结果后立即写一条同 `turn_id` 的 assistant 记录。回合收尾不再整体重写，否则回填会把历史读两遍。流被取消时，已流出的半截文本由 `preserve_interrupted_text` 就地落盘。消费端 `RolloutRecord::coalesce_turn_messages` 按回合同角色把数组内容拼回一条，块顺序与旧的整体记录一致。
- **工具输出上下文预算**：`tool_exec` 先向宿主发原始工具结果，再对全部来源的模型文本按至多 12 KiB 门槛处理；`tool_results` 可设更小门槛、预览字符数与是否落盘。默认保留最多 8 KiB 头尾预览、标记原文偏移，并将提示计入预算后安全溢出到工作区供 read 读回；该回执同时进入模型与 rollout，保证恢复一致。图片块保持原样。
- **压缩动作判定**：压缩器返回修改后的消息列表时，即使只截断而删除数为零，也须发送事件并记录审计；Noop 不回写、不登记。快速回填守卫上膛有两个条件，缺一不可：①**校准水位降到触发阈值下**（`below_trigger`）②**本地估算可信**（校准比例未撞上限 3.0 —— 模型上报的 token 数远大于引擎自己的估算时，「水位降了」可能只是估算偏差的产物）。此外守卫判定 `Skip`（跳过本轮压缩）时**也推进窗口锚点** —— 跳过的轮次消耗的模型步数照样计入窗口，不推进会让熔断窗口提前过期、连续计数被清零，死循环得以反复喘息。
- **手动强制压缩（`Op::Compact`）的 force 语义必须穿透到压缩阶段**：`compact_outcome` 的 `force` 参数会一路传进 microcompact / smart_prune —— 两阶段各自用自己的本地估算再判一次阈值，force 不穿透就会被整段跳过（手动强制压缩静默失效）。force 时 smart_prune 的裁剪目标降到 0（按 trigger 推导的目标会让裁剪循环一次都不执行），且 microcompact 之后不因「水位低于阈值」提前返回（microcompact 只替换工具结果正文、不删消息，提前返回会让 force 只压缩工具结果就收工）。
- **压缩恢复与阈值**：每次决策读取当前模型及窗口并固定阈值快照；新压缩记录含消息快照，恢复时从该边界继续，旧记录仍按摘要兼容回放。
- **后台 shell 生命周期**：复用工具层独立进程组，会话取消或任务丢弃时先终止 Unix 同组后代，再回收直接子进程；自行脱组的后代不在保证范围。
- **side-channel 并发上限**由 `src/side_channel/registry.rs` 的 `MAX_CONCURRENT` 约束，它与子代理工厂的在途上限是两套独立计数器。side-channel 的三条不变式：①取消令牌**独立于主令牌**（非 `cfg.cancel` 子级，registry.start 显式声明）；②事件**不进主历史**——终态经 `fan_out_session` 扇出仅供观察，rollout 由工厂以 `recorder=None` 承诺不落盘；③审批/问答经 approval_bridge 回注原 thread 的 submission sender，**不得**在 side-channel 内绕过主会话审批门。
- **prompt cache 保温(`src/cache_warming.rs`)有四条不变式**,改动调度器或其钩子点前必读:
  1. **候选 = wire 请求逐字节克隆**:重放只允许改 `max_tokens=1` 与 `warm_replay` 标记,其余一律不动;且必须用 `model_call` 成功那次的同一 client(同凭据同缓存),不经 `ModelRegistry` 轮询。
  2. **零污染**:保温重放不写 rollout、不触发 hook、不进 `session_usage`、不发 `TokenCount`、不喂 `CacheBreakDetector`(旁路 `model_call` 天然成立);花费只进 quota_tracker、`WarmingStats` 与 `CacheWarming` 事件 —— 从 TokenCount 流算命中率 / 成本的口径不得被扰动。
  3. **代际失效**:`AgentConfig::cache_generation` 在 rewind / compact / model / effort / tools / prompt 切换 / 配置重载 / `RegisterTools`(serve)处 bump;任何新增「改变下一请求 wire 前缀」的变更点**必须**同步 bump,否则保温会刷新过期前缀。
  4. **决策在 zhuye-llm**:本模块只管调度;`evaluate` / TTL / 延时全部来自 `zhuye_llm::cache_warming` 纯函数,不得在本模块复刻算式。
- **缓存前缀稳定性纪律(5.3 审计结论)**:一切 turn 边界注入必须走**尾部 user `<system-reminder>` 追加**(append-only),禁止内联 `ChatMessage::System`(fold_system_messages 会折叠进 system 块,改写已发送前缀 → 缓存全量失效;实例修复:PreCompact InjectMessage)。已知仍影响前缀的动态点(诚实登记,后续演进方向 = 移入公告):①todos checklist 注入 `compose_prompt_core`(todo 变更即改写 system 首块);②memory 检索结果注入 system 首块(演化/整合时变化);③子代理登记 / 恢复注入(已走 `<system-reminder>` 追加 ✓)。开启 Prompt Snapshot 的会话冻结这些内容(见 model_call 快照文档),快照失效后可见。
- 改了节点行为或事件发射顺序，要同步 `tests/functional/` 下对应的能力用例（断言到字段级），并按根 `AGENTS.md` 的验证矩阵跑 `./tests/run_all.sh functional`。
- **`TokenCount` 有两种语义，必须用 `turn_total` 区分**：`model_call` 每次成功调用发一条**单次调用快照**（`turn_total = false`）；回合收尾时 `submission_loop.rs` 另发一条**回合累计**（`turn_total = true`，数字 = 本回合逐次调用的累计，`provider` / `credential_label` / `model` 均为 `None`）。消费方把两者**一并累计**会让会话总量与费用翻倍 —— GUI 的 `sessionTotal` / `sessionCost` 因此只累计 `turn_total = false` 的事件。

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
