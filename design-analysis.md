# 烛夜 ZhuYe 项目设计分析报告

> 本文是对烛夜（ZhuYe）代码库的一次多维度设计优点分析。结论均基于对
> 46 个 crate、约 23.7 万行 Rust 源码与配套测试/脚本/文档的逐层阅读，
> 每条优点附 `文件:行号` 级证据。分析视角覆盖：架构分层、协议契约、
> 运行时引擎、能力层、资源层、编排层、集成与 SDK、安全可靠性、
> 测试质量工程、文档知识管理、发布与工程纪律，以及贯穿全局的设计哲学。
>
> 报告日期：2026-11-26 · 基线：`main @ 63a0312`（v0.1.17）

---

## 0. 项目概览

烛夜是一个用 Rust 编写的 **AI agent 运行时框架**——它不是终端编程助手，
而是"造 agent 的底盘"：读代码、改文件、调工具、调模型、开子代理，宿主产品
把它嵌进自己的进程里驱动。规模与形态：

- **46 个 crate，6 层 workspace**（`protocol` / `abilities` / `resources` /
  `orchestration` / `integrations` / `runtime`），约 23.7 万行 Rust
  （`crates/**/src/*.rs` 全量统计）。
- **单一版本号贯穿四处**：Rust workspace / Python `pyproject` /
  `__init__.py __version__` / npm `package.json`，发布脚本强制对齐
  （`scripts/release_sdk.sh` `check_consistency`）。
- **三种消费形态共用一套引擎**：进程内链接（`zhuye` 库门面）、headless
  单轮（`zhuye exec`）、常驻会话（`zhuye serve` + Python/TypeScript 双 SDK）
  （`docs/features.md:10-18`）。
- **开发节奏**：636 次提交集中在约 3 个月（2026-08 至 2026-11），
  conventional commit 风格执行率极高（`fix` 197 / `feat` 185 / `test` 67 /
  `docs` 37 / `refactor` 15 / `ci` 11），3 位主要贡献者。

一句话定位：**引擎是库，二进制只是它的薄壳**（`docs/architecture.md:34`）。
这个定位是整个项目一切设计决策的出发点。

---

## 1. 架构分层与依赖治理

### 1.1 严格的单向依赖分层

六层 workspace 的依赖方向**自上而下**，`protocol` 是所有层的公共契约，
`runtime` 在最顶聚合入口（`docs/architecture.md:44-54`）。分层规则不是口头
约定，而是被代码结构与 CI 双重固化：

- `resources` 层不得依赖 `orchestration` 层。这条规则的落地证据是
  `zhuye-task` 的 `TaskActor` 纯类型被**下沉到 protocol 层**以打破
  `resources → orchestration` 的依赖（`crates/orchestration/zhuye-task/src/actor.rs:1-6`）。
- 主引擎 `zhuye-core` 的**正常依赖**里只有一个编排 crate `zhuye-goal`
  （`crates/runtime/zhuye-core/Cargo.toml:30`）；`zhuye-subagent` 对 core
  只是 dev-dependency（利用 Cargo 允许 dev 依赖成环的特性），生产路径上
  工厂由 `zhuye-exec::bootstrap` 装配后以 `Arc<SubAgentFactory>` 注入
  （`crates/runtime/zhuye-exec/src/bootstrap.rs:379-491`）——引擎本身
  **不 import** 编排 crate。

这种"依赖方向被刻意压平"的做法让每个 crate 的边界清晰、可独立测试、
可被第三方替换，是框架型项目避免"大泥球"的关键纪律。

### 1.2 解耦靠"下沉"而非"上浮"

协议层通过把共享类型**下沉**来打破循环依赖，而不是让上层互相引用：

- `ToolError` 定义在 `protocol` 层（`crates/protocol/zhuye-protocol/src/item.rs:526`），
  明确为了打破 `tools ↔ hooks` 循环（让 `HookEvent::PostToolUseFailure`
  能携带它）。
- `ReasoningEffortMirror` 是 llm 层 `ReasoningEffort` 的**镜像枚举**
  （`item.rs:392-398`），避免 protocol 反向依赖 `zhuye-llm`。
- `RolloutRecorder` trait 放 protocol（`recorder.rs:1-9`），使 core 持
  `Option<Arc<dyn RolloutRecorder>>` 而不依赖具体的 `zhuye-rollout` 实现。

"镜像类型 + 下沉 trait" 是 Rust 大型 workspace 里治理循环依赖的成熟手法，
这里用得很克制、很一致。

### 1.3 库门面（facade）的组织

`zhuye` crate 是二次开发的主入口，提供**两种访问面**
（`crates/runtime/zhuye/src/lib.rs:5-26`）：

1. **扁平 re-export**——按消费方分节（Core/LLM/Protocol/Tools/Hooks/
   Rollout/Telemetry/编排/讨论/Builder，`lib.rs:38-171`），**每节注释说明
   为何暴露**（如 Rollout 读侧"SDK 消费者不依赖 zhuye-cli 即可查询历史
   会话"，`lib.rs:106-111`）。
2. **Builder 一行启动**——`Zhuye::builder("openai/gpt-4o").workspace(".")
   .build()?`，自带完整 M4 依赖。

门面上 `#![warn(missing_docs)]` 只管本 crate 新增的 `pub` item，re-export
沿用来源 crate 的文档（`lib.rs:30-32`）——既保证对外 API 有文档，又不因
re-export 产生噪音。`m4_bootstrap.rs` 把 exec 的装配核心逻辑下沉进库，
并**逐条列出与 exec 路径的差异**（不挂 LSP/MCP、notes 落盘失败降级
InMemory 等，`m4_bootstrap.rs:1-15`），做到"库 = 精简默认、exec = 全量
装配，同源不复制"。

---

## 2. 协议与契约设计

协议层是烛夜最成熟的部件之一，其核心优点是**"冻结"不只是文档承诺，而是
被多层机制固化、可被 CI 验证的契约**。

### 2.1 单一 tag 判别 + 只增不改的冻结契约

- `Op`（28 变体，`op.rs:18-28`）、`EventMsg`（60 变体，
  `event_msg/mod.rs:89-91`）、`RolloutRecord`（19 变体，`recorder.rs:110-112`）
  统一 `#[serde(tag = "type", rename_all = "snake_case")]`。
- 冻结策略写进**每个变体的 doc**：`op.rs:13-17` 记录了 v0 6 个变体 → v1.x
  逐步追加到 9 个的完整演化史；`sdks/PROTOCOL.md:13,32-36` 明确规定
  "Op/EventMsg 只增不改；改动既有变体字段属 breaking，禁止"。
- 新增字段一律 `#[serde(default)]` 实现**双向兼容**：如 `Event.seq`
  旧 producer 无字段 → 解析为 0（`event.rs:31`，roundtrip 测试
  `event.rs:84-87`）。

### 2.2 变体级"弃用但保 wire"

`PermissionMode::Deny/Bubble/Bypass` 等变体被标记弃用，但**保留线/rollout
兼容**、不进 UI 循环（`item.rs:156-178,194-199`）。这实现了"安全基线收紧
而不破坏旧客户端"——安全语义可以渐进收紧，wire 稳定性不受影响。

### 2.3 协议层自带"防退化"测试

这是协议设计最亮眼的部分：让"冻结"从纪律变成**可机器验证的属性**
（`crates/protocol/zhuye-protocol/tests/`）：

- `size_probe.rs`：用 `size_of` 给 `Op≤200 / EventMsg≤280 / Event≤320`
  定内存基线（mpsc 按值搬运的拷贝成本），超限应评估 Box 化。
- `event_contract.rs`：用 `include_str!` 从源码提取 EventMsg 变体与样本表
  校验奇偶——**实现是唯我真源**，文档与测试不能漂移。
- `event_mutation_fuzz.rs`：把"截断/字节替换不得 panic"固化为 CI 回归
  （默认 1 万次，nightly 提至 10 万次）。
- `examples/dump_schema.rs`：导出 JSON Schema 供前端 `json2ts` 生成 TS 类型。

### 2.4 可插拔性来自语义化 trait 文档

`RolloutRecorder` 的 4 个方法（`record` / `replay` / `list_sessions` /
`truncate_after`，`recorder.rs:561-584`）**每个都写明可观察语义**：
`record` 是 best-effort（磁盘满不中断 turn）、`replay` 坏行只 warn 跳过、
`truncate` 返回丢弃条数且 JSONL 实现先写 `.bak`。trait bound 的选择也
有解释（`Send+Sync` 挂 NodeContext、`Debug` 供错误路径、`async_trait`
因 dyn 兼容，`recorder.rs:557-559`）。`NullRecorder` 兜底（`recorder.rs:588-608`）。
这种"接口语义文档化"让任何宿主实现 recorder 时不需要读引擎内部。

---

## 3. 运行时引擎设计

### 3.1 手写 enum+match 状态机，理由显式化

`StateGraph` 走 4 节点 `PreLoop → ModelCall → (ToolExec → PreLoop)* →
CheckStop`。转移表 `GraphNode::next(has_tool_calls)` **仅 6 行 match**
覆盖全部转移（`crates/runtime/zhuye-core/src/graph/mod.rs:33-42`），
`run()` 主循环仅 20 行（`graph/mod.rs:69-90`）。

**不引入 petgraph** 的理由被写进三处（AGENTS.md、`zhuye-core/AGENTS.md`、
架构文档）："控制流显式可审计，是框架相对黑盒 agent 循环的核心差异"
（`docs/architecture.md:238-239`）。4 节点固定拓扑下，引入图库只增加不可见性。
同样的克制也体现在 `zhuye-pipeline` 的 DAG：自实现约 120 行 `DiGraph`
（add_node/add_edge/topo_sort/cycle_detect），避免约 5000 行依赖
（`crates/orchestration/zhuye-pipeline/src/lib.rs:23-24`）。

### 3.2 异常/取消语义处处一致

状态机最常见的错报是"被打断被记成完成""客户端按 id 配对不到"。烛夜用
几个显式机制消除它们：

- `is_natural_stop(node, turn_cancelled)`：明确"只有 CheckStop 的 `None`
  **且**回合未被取消才算自然结束"（`graph/mod.rs:44-52`），专治"等异步
  子代理被取消 → 误报 TurnComplete"（单测 `graph/mod.rs:98-109`）。
- 取消令牌**三级派生**：会话令牌 `cfg.cancel` → child → 回合级
  `turn_cancel` 登记 `active_turns` 表（`submission_loop.rs:492-504`）；
  Interrupt 粒度到回合、Shutdown 级联全部；终态幂等 remove
  （`submission_loop.rs:1181-1184`）；hook 拒绝 turn 时补注销防陈旧条目
  （`submission_loop.rs:544-549`）。
- `model_call` 用 `select!` 竞速 stream 与 cancel（`model_call/mod.rs:505-508`），
  重试 backoff 可中断（`retry_cancelled`，`model_call/mod.rs:1227-1230`），
  另有 5 分钟 `RETRY_TOTAL_BUDGET` 防"次数限不住的死端点"
  （`model_call/mod.rs:37-44`）。
- turn 边界隔离：异步子代理按"本回合令牌"筛选，别的回合的在飞子代理不被
  等待、不注入（`check_stop.rs:44-51`，测试 `check_stop.rs:569-602`）；
  回合收尾 `abandon_turn` 防"用户中断后子代理继续跑完、结果被别的回合误
  当回填"（`submission_loop.rs:871-880`）。

### 3.3 并发竞态有实证注释

- `AgentThread::submit` 先注册 per-turn channel 再 send；send 失败
  （loop 已退出）必须 remove 条目，否则残留 Sender 让 per-turn channel
  永不关闭、调用方永久等待（`agent_thread.rs:149-163`，典型触发路径已注明）。
- **turn 并发化解死锁**：每个 UserInput spawn 独立 turn 任务，
  submission_loop 才能继续处理审批/中断/关闭——"原先同步
  `graph.run().await` 会让任何审批流程死锁"（`submission_loop.rs:833-837`）。
- `NodeContext` 廉价克隆（`#[derive(Clone)]`），共享态全是
  `Arc<RwLock<…>>` / `Arc<dyn>` / `CancellationToken`
  （`submission_loop.rs:66-169`）；model 槽共享让热重载下一请求即生效。
  全程 `parking_lot::Mutex`，无 std poison 语义负担（`agent_thread.rs:6`）。

### 3.4 防御性一次性 flag 防死循环/死锁

`force_final_answer`（防无限续作，`state.rs:93-98`）、
`plan_stop_guard_attempts<2`（防审批死锁，`state.rs:107-108`）、
`hit_max_iterations` 标志**替代硬编码 `iteration>32`**（`state.rs:87-92`）、
`history_seeded` 防每迭代清空历史导致工具调用死循环（`state.rs:58-63`）。
每个安全阀都有"当初怎么坏的"的注释。

---

## 4. headless / serve 双模式与 IO 纪律

- **stdout=协议、stderr=日志，双向防污染**：tracing 显式
  `with_writer(std::io::stderr)` 且非 tty 关 ANSI（"此前常驻服务进程收到
  带色码日志混入聚合后不可读"，`runtime_config.rs:28-32`）；serve 单一
  writer task 串行写 stdout（`serve.rs:121-131`），全局顺序有保证。
- **攒批不损语义**：`JsonlWriter` 对 delta 事件 4KB 攒批（数千次 write
  降为一次 flush），非 delta 立即 flush 且顺带带出积压 delta
  （`jsonl.rs:7-35`）；broken pipe 优雅退出。
- **握手防死锁**：serve 启动即主动 emit `session_configured`
  （`serve.rs:167-175`），因为 core 只在首条 user_input 才发，客户端"等
  握手再发首条输入"会死锁。
- **坏行容错**：stdin 坏 JSON 不终止会话，emit
  `Error{code:"invalid_submission"}` 带 120 字符行预览后继续读
  （`serve.rs:236-250`）。
- **per-submission 收尾不变量**：drain task 在 per-turn 通道排空后补
  `SubmissionClosed`——"非 turn 操作没有终态事件，SDK submit_op 迭代器
  靠本变体收尾，否则永久阻塞"（`serve.rs:353-362`）。
- **双入口共享装配**：`bootstrap_common/normal/resumed` 被 exec 与 serve
  共用，"保证两条入口行为不再漂移"（`headless.rs:5-13`）；`headless.rs:1238-1243`
  记录了实测回归"stdout 锁被持久持有时 serve 输出永久阻塞"——管道隔离
  是有事故背书的纪律。集成测试 `serve_stdio.rs` 用 tokio duplex 内存管道
  + MockClient 全程离线覆盖握手/多轮/远程工具/Shutdown/坏行。

---

## 5. 能力层（abilities）设计

### 5.1 LLM provider 抽象

- **流式优先的 trait + 默认实现**：`ModelClient` 只强制 `stream()`
  （`client.rs:59`），`complete()` / `list_models()` 有默认实现——插件/
  测试 stub 零成本接入；`list_models` 不支持时返回 `Ok(None)`
  "诚实降级，不编造"（`client.rs:104-112`）。
- **wire 层错误统一分类**：`LlmError` 把 429/529/Auth/5xx/
  context-length 各带结构化载荷（如 `Retry-After`），重试引擎只需对**一个
  类型**做决策（`error.rs:9-49`）。
- **超时语义从实战修正**：`stream_http_client` 用"建连 10s 封顶 + 读空闲
  超时"双层替代 reqwest total timeout，注释记录内网 vLLM 60s 整点断流
  事故；`ensure_sse_response` 修复"网关把非 SSE 当 200 返回 → 协议故障
  伪装成模型没说话"（`providers/mod.rs:25-106`）。
- **凭据池 + cooldown**：`ModelRegistry` 加权轮询游标锁外 `fetch_add`
  （热路径无锁）；`mark_cooldown` 指数退避 `max(2×remaining, requested)`
  cap 1h，抽成纯函数 `compute_backoff` 便于边界单测（`cooldown.rs:71-77`）。
- **角色路由与迟滞选模**：`TierRouter` "升级立即生效、降档需连续 K 轮"，
  注释明确解释无迟滞时"模型每轮换、缓存全失效"（`routing.rs:15-19`）。
- **Mock provider 作为测试基础设施**：JSONL 脚本每行对应一次 `stream()`，
  支持 tool_call/分片参数/RequestEcho 等形态；配置错误显式 `InvalidRequest`
  冒泡**防"离线测试假绿"**（`providers/mock.rs:161-178`）。

### 5.2 工具系统

- **自描述 trait**：`Tool` 含 `parameters_schema`，且自带
  `is_concurrency_safe` 声明并行安全性、`required_permission` /
  `action_permission` per-action 权限路由（`tool.rs:54-67`）。
- **单一事实源收编**：28 个内置工具常量 `BUILTIN_TOOL_COUNT=28`
  （`builtin_tools.rs:166`）。注释记录**真实事故**——此前 exec 与 lib 两份
  注册清单漂移，"prompt 宣告 9 个工具但 SDK 路径报 tool not found"
  （`builtin_tools.rs:2-11`）；收编后三条路径（exec / serve / lib facade）
  都调同一函数。
- **并发策略**：`ToolExecutionQueue` 按安全性切分——unsafe 串行保序、
  safe `join_all` 并行且 Semaphore 限流（默认 5），结果按原始下标合并
  保序（`queue.rs:313,390-432`）。
- **审批 gate 内嵌执行路径**：`execute_single` 流程
  PreToolUse → ApprovalGate → execute → PostToolUse（`queue.rs:427-428`）；
  gate 有 session_allow/deny、yolo 分类器置信阈值、审批 TTL、回合级 cancel
  ——`gate.rs:64-75` 注释记录"等审批时按中断，回合永远不终止（实测）"的
  事故。
- **防失控工具链**：loop_guard 双档处置且用**输出指纹**作"进度证据"——
  同参重复但输出在变 = 轮询不阻断（`loop_guard.rs:1-27`）；arg_repair
  只做可证明无损修复"绝不猜语义"；result_budget 超长落盘 +
  `<persisted-output>` 信封，修复"被砍内容彻底蒸发"；web_guard 两级
  SSRF 防护含 DNS rebinding（`web_guard.rs:1-30`）。

### 5.3 Hook 系统

- **事件/决策模型**：12 类事件、7 种决策形态含 `Combined`；`resolve()`
  按叶子出现序 DFS 展开，注释记录"顶层 match 把 Combined 当 no-op、Deny
  被静默丢弃、Stop 否决失效"的实事故（`decision.rs:63-67`）。
- **引擎合并语义**：dispatch 锁内抓快照、**锁外 await** 避免跨 await 持锁
  （`engine.rs:152-170`）；hook 异常 fail-closed 转 Deny；merge 优先级
  Deny > Ask > ModifyArgs > InjectMessage。热重载不变量：disabled 集合在
  unregister+register 后保留，被禁 hook 不会"复活"（`engine.rs:41-88`）。
- **Stop 否决 + 计数安全阀**：内置 verification 强制
  `attempt < MAX_ATTEMPTS(=2)` 才否决，防无限续跑；否决时 InjectMessage
  以 system-reminder 追加进历史让续作轮可见原因（`check_stop.rs:104-111`）。
- **权衡显式化**：`read_before_edit` 明确声明"bash 绕过是已知权衡，本层是
  防误覆盖不是安全边界"（`read_before_edit.rs:15-22`）——把安全层级讲清楚。

### 5.4 上下文治理

- **三级 escalation 压缩**：microcompact → smart_prune → LLM 摘要逐级升级，
  每级后检查水位；零改动时诚实降级 Noop——"谎报非空策略会给 rapid-refill
  熔断器上膛误杀正常 turn"（`strategy.rs:5-7`）。
- **Rapid-refill 熔断器**：压缩后 ≤3 步内再满 = rapid refill，连续 3 次
  Abort，手动压缩豁免（`refill.rs:1-14`）。
- **BM25 词法为中文专门设计**："ASCII 词 + CJK bigram"双路，注释记录
  纯中文查询经旧版 ASCII 过滤后**零词项**、BM25 空转，是 memory/notes
  共同能力缺口（`bm25/lib.rs`）；全角折叠半角；同一套词法供 SQLite FTS
  列复用。
- **跨会话记忆**：memory 三 scope（project/user/session），按 `##` 分节 +
  BM25 检索替代全量注入，确定性 consolidate 去重零 LLM 成本；notes 用
  8KiB 注入预算防"30×4096 撑爆 prompt cache"（附 review 日期）。

### 5.5 评测体系

三层结构（assertion → LLM judge → runner）。**断言失败不进 judge 被明确
定性为正确性而非优化**——judge 对明显错误输出可能给 pass，断言兜底才
可靠（`runner.rs:6-9`）；judge 可注入 mock 闭包保证 CI 不依赖真模型；
`JudgeVerdict` 三档中 Uncertain"记 fail 但不计硬性回归"。

---

## 6. 资源层（resources）设计

### 6.1 配置系统

- **热重载 = 文件监听 + polling 双通道，互相兜底**：`notify` 监听父目录
  非递归（兼容 vim "rename+create" 保存风格），250ms debounce；另加 60s
  polling 兜底 macOS FSEvents 漏事件，用内容 hash 去重（`watch.rs:39-42,
  75-89,136-156`）。
- **失败不破坏运行态**：reload 解析失败保留上一个有效配置并 warn，有测试
  `invalid_reload_keeps_previous_value`（`watch.rs:177-183,260`）。
- **home 口径单一事实源**：全框架数据目录统一委托 `zhuye_fs::data_dir()`
  （`$ZHUYE_HOME > $HOME/.zhuye`），文档明确批评历史上各 crate 各自解析
  导致口径漂移（`src/paths.rs:1-12,32-43`）。
- **三值语义**：`Option<bool>` 刻意区分"未表态 → 走默认基线"与"显式
  false"（如 `[sandbox].os_level`），避免调用方回读原始 TOML。

### 6.2 权限系统

- **四态区分"未配置"与"强制 prompt"**：resolver 结果是
  `RuleMatch{Allow, Deny, Ask, NoMatch}`——NoMatch 走默认审批路径而非 Ask
  （`rules.rs:55-64`）。
- **安全底线不可被规则放行**：风险标签（凭据外传/提权）`deny_labels`
  **硬拒**，默认开启且规则无法覆盖；**控制面自保护**——改权限/配置/插件
  的工具名清单（`CONTROL_PLANE_TOOLS`）命中时 Allow 降级为 Ask，由人裁决
  （`policy.rs:122-145`）。另有无人值守模式 Ask→Deny 防挂死。
- **fail-closed 存储**：权限文件损坏 → 整个 resolver 拒绝工具，有测试
  `corrupt_permission_file_denies_until_repaired`（`resolver.rs:146-151`）。
- **稳定 reason 码**：`DecisionReason` 为 snake_case 稳定码，UI/审计按码
  分支而非解析人话（`policy.rs:83-111`）。
- `PolicyEngine` 是**纯函数评估器**，支持可配置 precedence
  （DenyWins/FirstMatch/LastMatch）、缺配置可配 Fail-closed 全拒
  （`policy.rs:29-50`）——便于单测与嵌入方定制。

### 6.3 插件系统

- **installed 与 enabled 分离**：`installed_plugins.json` 只描述"装了什么"，
  `enabled_plugins` 在 config 描述"启用什么"；同 id 可多 scope
  （Managed/User/Project/Local）共存，precedence 数值化；状态机
  `Installed/Enabled/Disabled/Error{reason}`，Error 可重新 enable 触发重试
  （`state.rs:26-37`）。
- **bundled 门控插件单一事实源**：`BUNDLED_PLUGIN_DEFAULTS` 常量表集中定义
  默认开关（默认开 = 升级行为不变、默认关 = opt-in）；判定函数
  `bundled_plugin_enabled` 三级 disabled > enabled > 默认表
  （`bundled.rs:61-66`），且 `bundled_tool_gates` 是"插件名 → 工具门"映射
  的**唯一维护点**，三条装配路径共用。
- **manifest 能力槽清晰**：`PluginManifest` 集中 hooks/commands/agents/
  skills/mcp_servers 五槽（`manifest.rs:267-312`）；顶层未知键宽容、嵌套
  对象 strict 抓错拼。
- **能力加载与注册两阶段**：capabilities loader 是纯函数只产
  `LoadedPlugin`，实际挂载集中在 `loader::register`——易测试、依赖收敛
  （`capabilities/mod.rs:1-24`）。
- **WASM 组件宿主**：wasmtime 组件模型，`wit/plugin.wit` 定义
  `zhuye-hook` / `zhuye-tool` 两 world；JSON 字符串 ABI 让 TS/Python/Go
  无需生成绑定；进程级单例 Engine + epoch 中断 + 10ms ticker 实现确定性
  30s 超时截断；组件**默认无 FS/网络/env**；hook 超时/坏 JSON 一律 Deny，
  WASM 工具权限 floor 固定 `Prompt` 不可被组件声明 Auto 提升
  （`zhuye-wasm/src/lib.rs:13-17,59-86`）。

### 6.4 持久化（rollout）

- **JSONL + tagged union**：`RolloutRecord` serde `tag="type"`，外部工具
  可直接 `jq`。记录类型覆盖 SessionMeta（含 `parent_session_id` 血缘）/
  Message / Compaction / Fork / Checkpoint / InputDisposition /
  PromptSnapshot / StreamRecoveryDiscarded 等。
- **崩溃对账永不静默重放**：`InputDisposition` 落每个 turn 的终态处置
  （promoted/aborted/failed/discarded），resume 时"有 Message 无
  InputDisposition = 崩溃窗口"，**显式告知用户而非重放**
  （`recorder.rs:207-223`）。
- **关键不变量"每个文件首行 = SessionMeta"**：writer 缓存首份 meta，
  轮转时重写，否则 session 会随轮转从索引消失（`writer.rs:15-24,49-68,
  180-185`）。256KB 轮转、保留 3 份。
- **向前兼容读取**：reader 区分"未知 variant"（debug+skip）与"真损坏"
  （warn+skip），且**不打印行内容防敏感泄露**（`reader.rs:21-53`）。
  万级会话用内存倒排索引 + `Lineage::children_of` 画会话树。

### 6.5 OS 沙箱

- **双平台真沙箱 + 失败关闭**：macOS 走 `sandbox-exec` 生成 `.sb` profile
  （`allow default deny` 语义），Linux 走 Landlock ABI v3+ 在 fork/exec 之间
  syscall 应用 + seccomp 限网（`os_sandbox.rs:9-16`）；`OsSandbox::default()`
  自 v1.3 起 `enabled=true`，strict 模式下后端不可用**绝不回退裸 `sh -c`**。
- **防提权的元数据目录保护**：可写根顶层 `.git/.agents/.aws` 默认受保护
  ——"改 git hook 即借宿主身份提权"（`os_sandbox.rs:63-70`）。`.git` 归属
  判定精到"unborn HEAD 空仓库视为 agent 脚手架可写，一旦有提交历史变用户
  资产只读"，且判定锚在**会话首次建 profile**而非逐命令，避免 agent 卡进
  "init 成功但 commit 永远拿不到 index.lock"半截状态（有桌面实证）。

### 6.6 审计与密钥

- **决策级审计记录"为什么"**：`DecisionAudit` 除最终裁决外记录 reasoning、
  命中规则 id、风险标签、**被否决的备选裁决**（`RejectedAlternative{action,
  why_not}`）——能回答"为什么这条 rm -rf 被放行/本应 Deny 为何降级"
  （`zhuye-audit/src/decision.rs:1-16,39-52`）。
- **密钥 provider 抽象 + AEAD**：`Provider` trait（**非 enum**，外部可塞
  公司 vault）三内置后端 File/Command/Env；文件后端每条目独立 salt+nonce 的
  AES-256-GCM，同明文两次写入密文不可关联；配套 `scrub_env` 出口护栏，
  子进程 env 过滤 `*_API_KEY/*_TOKEN` 防 agent 从 `/proc/self/environ` 读密钥
  （`provider.rs:29-32`）。

---

## 7. 编排层（orchestration）设计

- **subagent 并发在途 ≤16**：`MAX_DEPTH: u8 = 16`（`zhuye-subagent/src/lib.rs:49`）；
  factory 持 `Arc<AtomicU8>` in_flight 计数器，`fetch_add` 达上限即回滚并
  返回 `MaxDepthExceeded`，计数器**跨父/子/孙 factory 共享**（嵌套共用同一
  预算）；槽位由 `CleanupGuard` RAII 管理，spawn 在任何 await 被取消也释放
  （`factory/mod.rs:821,860-895`）。
- **discussion 角色编排**：orchestrator 不直接调 LLM，caller 注入 `prompt_for`
  闭包（标准接法 = SubAgentFactory.spawn + drain），**单测无需 mock LLM 即跑
  完整状态机**（`orchestrator.rs:13-21`）；顺序/并发两种 runtime，
  `consensus_window` 轮窗口内全员 Consensus 才判共识，可选 judge 仲裁；
  全程 emit `Collab*` 事件 + 写 `DiscussionTranscript` 到 rollout。
- **task 委派**：`TaskStore` trait + 内存/文件双实现同构；文件后端跨进程
  文件锁 + `tmp+rename` 原子写 + `.highwatermark` 单调 id。
- **pipeline DAG**：按 depth 分 level 供并行；**失败策略语义**——失败节点的
  传递性下游在 `Abort` 与 `ContinueCollect` 下都标 `skipped`，无依赖旁支照跑，
  差别只在总状态 failed/partial（e2e 测试 `pipeline_e2e.rs:233-246`）；
  F33 durable 执行：run 级 journal + `RecoveryAction{Run, Replay, Uncertain}`
  ——**副作用做了一半的步骤判 Uncertain 不盲目重放**（`durable.rs:15-143`）。
- **goal 目标模式**：`GoalController::on_turn_end` 每 turn 结束自校验
  （verifier 用 LLM 输出结构化 JSON `{verdict: Met/Unmet/Blocked +
  evidence/remaining}`）；Unmet 时生成续作 prompt，经引擎 **steering 队列**
  推入（非工具调用），`submission_loop.rs:1097-1131` 是引擎侧唯一接入点。

---

## 8. 集成层与双 SDK

### 8.1 MCP 客户端

- **双传输 + 统一构造分发**：`transport::build()` 按枚举分派 stdio /
  streamable-http / legacy SSE，三种 transport 共享同一 handler，不向调用方
  泄漏 rmcp 类型（`transport.rs:278-335`）。
- **密钥隔离**：stdio 子进程 `env_clear()` 后仅注入 `HOME`/`PATH` + 用户显式
  env，防 `OPENAI_API_KEY` 泄漏给第三方 server（`transport.rs:514-555`）；
  **有专门单测断言父进程 API key 不泄漏**。`kill_on_drop(true)` 防 zombie。
- **与引擎 Tool 系统桥接**：`McpToolAdapter` 实现 `zhuye_tools::Tool` trait；
  命名强制 `mcp__<server>__<tool>` 与内置不冲突；`inputSchema` 即 JSON
  Schema 2020-12，LLM 层零改动；**默认 `PermissionMode::Prompt`——MCP server
  不可信，走 ApprovalGate 逐次确认**。
- **运行期工具刷新竞态处理**：`notifications/tools/list_changed` → 重拉清单 →
  快照带**单调版本号**，解决"刷新早于 start 收尾"拿到旧清单的竞态；
  `stop_server` 按当前生效快照反注册，否则残留死工具。
- **OAuth**：token 端点缺省按 RFC 9728/8414 发现；拿不到 token 不硬失败，
  先匿名握手、401 时上报 `AuthRequired` 带可执行配置提示，**引擎不代跑浏览器**。

### 8.2 流式会话后端（zhuye-stream）

- **queue（v1.10）**：session 级 FIFO 串行消费，完成信号走
  **per-submission 终态事件监视**（TurnComplete/TurnAborted/通道关闭按任务
  id 结清），周期 tick 仅兜底；注释**完整记录为什么弃用 Stop hook**——引擎
  存在 5 条绕过 check_stop 的结束路径（凭证耗尽/重试上限/预算耗尽/Interrupt/
  流取消），曾致队列死锁到重启（`queue.rs:15-23`）。
- **解耦层次**：integrations 层不依赖 zhuye-core，经 `TurnSubmitter` /
  `GoalProbe` **闭包注入**，由 exec 装配时闭包读 controller 快照。
- **cron**：5 字段表达式自包含解析（无外部 cron crate，"对齐项目风格"），
  next_after 暴力枚举至多 366 天防死循环 + 持久化 + 原子写。
- **autopilot**：把桌面端"stub 摆设"收编为真闭环，schedule 与 cron
  **共用同一 CronSchedule 口径**（"不再各写一套"）。

### 8.3 plugins/* 作为能力本体的架构决策

代码体现为**两种真实形态**，而非假 feature：

- **MCP 子进程型**（shell/browser/computer-use/git-tools/kms）：plugin.toml
  `mcp_servers = "./.mcp.json"`，能力本体在 integrations 层的 `[[bin]]`
  server 二进制里，与引擎经 **stdio JSON-RPC 隔离**——"装插件 = 授权任意
  代码执行"的安全语义由进程边界天然成立。
- **real feature 门控（真实 feature，default 关）**：computer-use
  `real = ["dep:xcap","dep:enigo","dep:image"]`；未开时全部操作返回
  `Unavailable`/isError 而非 panic（`backend.rs:26-42`），plugin.toml 注释
  承诺"缺权限或 headless 时优雅降级"。
- **门控型**（lsp/goal-queue/verify-hooks/notebook/web-search）：无独立
  二进制，本体在引擎，插件只承载开关——`BUNDLED_PLUGIN_DEFAULTS` 单一事实源。

### 8.4 双 SDK 设计

- **Python SDK 零依赖**：纯 stdlib，协议类型手写 `TypedDict`（"代替 pydantic
  运行时依赖"）；serve 客户端 reader 线程按 submission id 路由；
  **listener 在写 stdin 前注册**防首批事件丢失；`tool_execution_request`
  路由前拦截、handler 跑独立线程（防并行工具调用卡死）；handler 异常兜底转
  `is_error=true` 回执让 LLM 看到错误而非挂死。
- **未知变体降级**：双 SDK 的 `EventMsg` Union 含 `UnknownEventMsg`，
  `Submission.op` 宽化为 dict"便于 unknown op 透传"；测试用真实
  `tests/contracts/events.json` 样本 + 人为 `future_field`/`future_event`
  断言**逐字段完整透传且不破坏终态**——协议冻结与 SDK 可演进在这里闭环。
- **PyO3 绑定 zhuye-py 是第三条 Python 通道**（进程内、同步），不走
  JSONL/子进程，直接依赖 `zhuye`/`zhuye-core`/`zhuye-protocol`；`py.detach`
  释放 GIL 后阻塞；**明确声明流式未提供、是后续扩展点——不假装能力完整**。
- **PROTOCOL.md 把隐性并发/时序语义写进规范**：握手（serve 主动发
  `session_configured`）、坏行不终止会话、单一 writer 保证全局顺序、
  终态语义精确区分（`shutdown_complete` 携带 shutdown 那条 submission 的
  id）、远程工具流（ASCII 时序图 + "回执 call_id 与模型 tool_call_begin/end
  的 id 不是同一个"）。

---

## 9. 安全与可靠性（横切维度）

烛夜的安全设计有一个非常一致的哲学：**fail-closed + 可追溯 + 诚实降级**。

- **fail-closed 遍布每个失败路径**：沙箱不可用拒绝执行、权限文件损坏拒绝
  工具、WASM hook 超时判 Deny、`.git` 归属判读失败视作用户资产、pipeline
  副作用半完成判 Uncertain、Linux 沙箱"限制无法安装即失败关闭"。
- **三层工具管控正交**：注册门控（插件级，工具不进注册表）→ 可见性过滤
  （工具名级，LLM 不可见）→ 执行期审批（权限规则/floor，操作级）
  （`docs/features.md:54-66`）。三层入口不混用。
- **输入守恒 ledger**：每个进入引擎的 turn 终态显式落盘
  （`InputDisposition`），崩溃窗口在 resume 时显式告知，**永不自动重放**
  （`submission_loop.rs:1185-1219`）。
- **流锚点恢复**：可重试流错误（断流/网络/5xx）已有可见增量时，从安全锚点
  重开而非整段重来；丢弃的尾部以 `StreamRecoveryDiscarded` 留痕（resume
  不回放），恢复过程经 `stream_recovery_*` 四事件可观察。
- **空结果防护**：provider 自证零产出的正常结束判为可疑
  （`suspicious_empty_model_result`），details 持久化原始归因。
- **会话中毒检测**：四类中毒（输出复读/错误率/超时/传输），**用户主动取消
  不算毒**；退役只触发一次，且 `ContinuityNotice`"明确告诉用户丢了什么"
  （四类工作记忆）。
- **缓存零污染不变式**：prompt cache 保温不写 rollout、不进 session_usage、
  不发 TokenCount、不喂 CacheBreakDetector。

---

## 10. 测试与质量工程

烛夜的测试体系是"**确定性离线门禁 + 诚实的未覆盖清单 + 反假绿机制**"。

### 10.1 八层分工明确

| 层 | 位置 | 测谁 | 规模 |
|---|---|---|---|
| 能力覆盖层（主） | `tests/functional/` | Rust 代码在真产物上的行为（原始 JSONL wire） | 401 条 |
| CLI 参数面 | `tests/rust/*.sh` | 18 个子命令参数矩阵 | 128+ 断言 |
| 事故回放/差分 | `tests/verification/` | 新旧版本同输入行为语义对比 | 自测 29 + 差分 5 |
| SDK 绑定层 | `tests/sdk-python|ts/` | 两个客户端自己的行为 | Py 19 / TS 19 |
| SDK 随包单测 | `sdks/*/tests/` | SDK 纯逻辑（mock serve） | Py 28 / TS 40 |
| 原生 wheel | `tests/native-python/` | PyO3 wheel 接口 | 2 |
| 实模型演练（可选） | `tests/live/` | 真模型下协议形状 | 10 + 28 场景 |
| 协议契约 | `tests/contracts/` | EventMsg 全变体线格式 | 55+ 变体 |

**关键设计"能力断言唯一去处"**：只写"退出码 0 / 不抛错"被**明文禁止**；
新能力断言只落 functional 一层。理由是有真实实例——Python SDK `close()`
不发 shutdown op，经 SDK 驱动会**遮住 Rust bug**（缺陷 D1）。

### 10.2 覆盖矩阵是活文档

"能力 × 覆盖层"映射表约 60 行，未覆盖项有"诚实清单"。矩阵里的用例计数
由 `tests/update_coverage_count.py` 从 pytest `--collect-only` **实测刷新/
校验**，CI job 跑 `--check`，**数字过期即红**——文档与测试永不漂移。

### 10.3 反假绿三重闸

- `validate_registry()` 启动前校验 mapped 任务必须有命令、任务必须进至少
  一个选集、CASE_INDEX 不得引用未知任务。
- `infra_error` 时中止后续任务"基座坏了结果不可信"。
- `cargo test` 退出码 0 但日志里**没有任何用例跑过**也判 infra_error。
- `check_junit.py`（仅 22 行）：拒绝缺失、空集、全跳过的 JUnit 报告。

### 10.4 离线开发：`ZHUYE_MODEL=mock`

mock provider 实现于 `zhuye-llm/src/providers/mock.rs`：免 API key、零网络；
`ZHUYE_MOCK_SCRIPT` 指向 JSONL，每行对应一次模型调用，支持 text / tool_call /
tool_call_delta（参数分片，锻炼流式聚合路径）；"turn1 调工具、turn2 收尾"
两行脚本即可确定性编排多阶段对话。

**防假绿细节**：默认固定回复文案自带"mock 固定回复"可辨识标记，脚本写错时
断言文本非空不会假通过；脚本路径不存在显式返回 `InvalidRequest` 而非静默
降级。全链路贯通（runner.py 默认注入、make headless、e2e.sh、CI）。

### 10.5 属性测试定位精准

proptest 用在**纯函数不变式**上：协议 serde 往返不漂移、sanitize 幂等/永不
panic（字符策略刻意含 `\n`——"否则消费换行的分支永远不在随机探索范围内"）、
smart_prune 截断幂等。另有 10 万条协议突变 fuzz（nightly）。

### 10.6 脚本门禁

- `scripts/e2e.sh` 13 步冒烟，`set -euo pipefail` 每步快失败；注释记录两次
  修复假阳性——`|| true` 吞掉 example 失败、注入假 API key 打真网络必败再被
  `||` 吞。`E2E_HOME` **按步注入**而非全局 export，避免破坏 workspace 测试中
  断言 `$HOME/.zhuye` 形态的用例。
- `scripts/bench.sh` 冷启动/首事件/首文本/单轮耗时/峰值 RSS 五指标独立采样，
  **任一指标缺失即非零退出——是硬门禁而非打印报告**。
- `scripts/release_sdk.sh check_docs` 从 `op.rs` **源码**解析 `pub enum Op`
  变体 → snake_case wire 名，逐一校验双 README Op 表、`protocol.py` 的
  `Literal[...]`、`protocol.ts` 的 `type: '...'` 四处收录；协议加 Op 后文档未
  同步会在**发布前**失败。
- `scripts/check_brand_names.py`：7 个他牌品牌名登记，扫**工作树 + 全部可达
  提交信息**——"品牌名一旦进历史，clone 即永久可见，改写历史成本远高于当下
  拦住"；豁免只两类且必须窄到具体字面量。

### 10.7 Lint 与 profile

- 根 `Cargo.toml [workspace.lints.clippy]` `all=deny` + 3 条性能纪律
  （redundant_clone / needless_collect / map_unwrap_or）+ 11 条集中 allow
  清单（注释："逐项修复后收紧，每收紧一条同次提交内清零存量"）；各 crate
  一行 `[lints] workspace = true` 接线。CI 双保险：全局 `RUSTFLAGS=-D
  warnings` + `cargo clippy --workspace --all-targets -- -D warnings`。
- profile 策略：release（thin LTO、strip）分发；release-fast（无 LTO、
  cu=16）本地迭代；**dev 关 incremental**（注释：incremental 是 30+GB 元凶）；
  test profile debug=1 加速测试编译。
- MSRV 1.89 由**安全依赖面**驱动（pyo3/rmcp/rusqlite 安全升级把 MSRV 顶到
  1.89）。

---

## 11. 文档与知识管理

### 11.1 双语文档体系

`docs/` 29 个文件，14 对中英双语（`.md`/`.en.md` 互链），覆盖 architecture /
cli / sdk / plugins / configuration / development / release / eval-baseline
等。README 有"文档表"导航。

### 11.2 AGENTS.md 渐进式披露

全仓 **18 篇 AGENTS.md**（根篇 + 六层篇 + 7 个关键 crate 篇 + scripts/sdks/
tests 篇）。根 `AGENTS.md:12-29` 导航表"想了解什么 → 读哪篇"，每篇首行
"何时读我"——把"何时读什么"的成本前置，避免全量上下文污染；并明确"修改
任何 crate/目录前先读该目录或其层篇"。这是给 AI 协作者看的"阅读地图"，
显著降低了大仓的导航成本。

### 11.3 注释即事故史（字条文化）

项目约定"非显然防御性代码必须用中文注明修复原因/依据"，执行得很彻底。
注释读起来像**事故报告**且可定位，例如：

- `providers/mod.rs:27-40`（vLLM 整点断流事故）
- `builtin_tools.rs:2-11`（两份注册清单漂移 → prompt 宣告 9 工具但 SDK 报
  not found）
- `check_stop.rs:126-130`（结果不落盘下一轮只见占位）
- `gate.rs:64-75`（等审批时按中断，回合永远不终止（实测））
- `decision.rs:63-67`（Combined 吞 Deny、Stop 否决失效）
- `notes/store.rs:25-29`（30×4096 撑爆 prompt cache）

回归测试**附事故编号/日期**（如 session 889c3bd0、Review 2026-06-30）。
这让"隐性经验显性化"，防止后人无意 revert。

### 11.4 诚实的边界与欠账

- `todo.md`：明确记录"诚实的延后项与验证欠账"——Windows 沙箱真机验证
  （代码已落地、双 target 编译零错误但 `--ignored`）、computer-use 真机手动
  验证、live/实模型门禁未跑等，按优先级排列。
- `iss.md`：记录已知问题**带具体数字**（max_tokens=8192 真实生效、远程工具
  120s 超时、二进制 32.8MB、版本漂移风险）。
- `docs/features.md` 的"边界与限制"一节如实说明 Landlock 不限制敏感读取、
  嵌套敏感路径无法强制只读、Windows 属未验证平台。

这种"不假装能力完整"的诚实，本身就是工程可信度的来源。

---

## 12. 发布与工程纪律

- **单一版本号贯穿四处**：Rust workspace / pyproject / `__init__.py` /
  package.json，`check_consistency` 不一致即 die；`bump` 幂等、用 Python 精确
  锚点替换且 `assert count==1` 防误伤依赖版本。
- **tag 是硬安全边界**：semver 校验、工作树干净、必须在 main、tag 不存在，
  `--yes` 也不绕过分支约束。推 `v*` 同时触发 CLI Release 与 SDK 发布。
- **release-sdks.yml 8 job**：gate（版本一致性 + 双 SDK 离线测试）→ 5 平台
  binaries → **SDK 携带平台二进制**（PyPI wheel 内嵌 zhuye-cli、npm 主包 +
  5 平台子包 optionalDependencies），全程 **OIDC Trusted Publishing、仓库
  secrets 不存任何 token**。
- **发布后验证闭环**：`tests/release_artifacts.py` 安装并验证指定二进制/
  双 SDK 包/wheel，release 选集 + `--artifact` 入口。
- **nightly 双轨语义差分**：同提交 release vs debug 恒严格；HEAD vs 最新
  已发布二进制，**v0.1.16 重置基线后恢复严格判定**——把"行为回归"变成可
  持续检测的属性。
- **CI 编排**：PR 面 11+ job（含 registry 自检、跨平台 macos/windows 离线
  mock 测试）；functional 失败自动上传证据 artifact 保留 7 天。

---

## 13. 贯穿全局的设计哲学

把上面各维度收拢，烛夜反复体现出一组一致的工程价值观，这或许是它最值得
借鉴的部分：

1. **"引擎是库，二进制是薄壳"的定位纪律**——所有抽象（facade、协议、
   trait、serve 入口）都服务于"被嵌入"这一目标，而不是服务于 CLI 本身。
2. **单一事实源（SSOT）收编**——内置工具清单、脱敏 pattern、BM25 词法、
   bundled 插件开关、home 目录口径、CronSchedule 口径，凡是会"两份漂移"的
   都收编到一处，且收编动作本身带事故注释。
3. **fail-closed / 诚实降级**——安全路径选保守侧，能力缺失不假装、不编造、
   不静默回退，并留下可追溯的 reason 码/事件。
4. **注释即事故史**——把"当初怎么坏的、现在为什么能防住"写进代码，让
   防御性代码可被搜索定位，不当无字据的暗知识。
5. **每个数字都可复算**——覆盖矩阵计数、性能基线、文档 Op 表、版本号，
   都有脚本从源数据重新计算并校验，"文档与实现永不漂移"靠机制而非靠人。
6. **反假绿**——对"可能假绿"的每个点（`|| true`、退出码 0、空 JUnit、
   文档过期、品牌名进历史、矩阵数字漂移）都有对应门禁拦在合入/发布前。
7. **边界清晰**——分层依赖方向、主循环 vs 插件的形态判断、SDK vs 引擎、
   能力层 vs 绑定层 vs 实模型层，每层只测自己该测的东西。
8. **可解释性作为一等需求**——决策级审计记"被否决的备选"、输入守恒
   ledger、丢弃留痕、ContinuityNotice 明确告知丢了什么。"事后可解释"与
   "功能"同级。

---

## 14. 结语

烛夜在三个层面表现出高于平均水准的设计成熟度：

- **架构层面**：六层单向依赖 + 下沉式解耦 + 门面 Builder，让 23.7 万行、
  46 crate 的代码库保持了清晰的边界与可测试性。
- **契约层面**：协议冻结被 size 基线、源码扫描契约、mutation fuzz、schema
  导出四重机制固化，"只增不改"从纪律变成可 CI 验证的属性。
- **工程层面**：八层测试分工、反假绿三重闸、可复算的覆盖矩阵、事故史注释、
  诚实的验证欠账清单——构成一套自洽的质量文化。

最突出的优点是它把**"为什么这样设计"和"曾经怎么坏过"**当成与"怎么做"
同等重要的交付物，系统性地沉淀进注释、文档、测试与发布门禁里。这种可追溯、
可复算、不假装的文化，是大型 agent 运行时框架在长期演进中保持可靠的关键。

> 附：本报告聚焦设计优点。项目的已知边界（Windows 未验证、Landlock 敏感读
> 限制、16 子代理并发上限等）已在 `docs/features.md`「边界与限制」与
> `todo.md`「验证欠账」中如实登记，此处不再重复。
