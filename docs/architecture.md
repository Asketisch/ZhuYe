# 架构详解

<p align="center">  <b>中文</b> · <a href="architecture.en.md">English</a>  </p>

> 本文是烛夜(ZhuYe)的架构参考:框架边界与入口、workspace 六层分层、协议与
> 会话生命周期、一轮执行的状态转移、节点上下文与能力装配、关键设计决策。
> README 讲用法,本文面向需要理解内部实现或二次开发的读者。更细的 wire
> 语义见 [sdks/PROTOCOL.md](../sdks/PROTOCOL.md);工具与 hook 的字段级规范见
> [tools-and-hooks.md](tools-and-hooks.md)。

## 1. 框架边界与入口

烛夜是 **Rust 编写的 AI agent 运行时框架**,不是终端里的编程助手。它的
消费方式有几种,主接口与边界如下:

- **库门面(`zhuye` crate)** —— 二次开发的主入口。`Zhuye::builder(model)`
  是唯一构造器,链式装配 workspace / 审批 / 插件 / 提示词库 / 遥测 /
  rollout 等,产出可进程内驱动一个 `AgentThread` 的 handle;`zhuye` 库把
  各层 crate 的关键类型平铺 re-export 出去(百余个公开名),宿主进程直接
  依赖它即可。
- **headless CLI(`zhuye exec`)** —— 薄 CLI 入口,由 `zhuye-cli` crate 产出
  单一 `zhuye` 二进制。**不含 `tui` 子命令**(TUI 已拆至独立仓库
  [Reflect-TUI](https://cnb.cool/Demon1019/Reflect-CLI))。跑一轮就退出:
  事件以 JSONL 流写 stdout,tracing 日志走 stderr,`zhuye exec | jq` 不被
  日志污染。
- **serve 协议入口(`zhuye serve`)** —— 常驻 stdio JSONL 会话服务,一个进程
  = 一个常驻 `AgentThread`,多轮共享内存状态。Python / TypeScript SDK 都
  构建在这套协议之上(见 [sdks/PROTOCOL.md](../sdks/PROTOCOL.md))。
- **PyO3 绑定(`zhuye-py` crate)** —— 走独立 crate,不进 `zhuye` 库门面,供
  Python 宿主直接链接。
- **外部会话导入(`zhuye-import`)** —— 独立二进制(由 `zhuye-import` crate
  的 `bin` 产出),把其他 Harness 的会话记录转换进 rollout,见
  [plugins/harness-import](../plugins/harness-import)。
- **WASM 插件宿主 / git 工具 / computer-use / shell** —— 这些是 `plugins/*`
  的能力本体,在 integrations 层实现,但**不进** SDK facade,按插件形态挂载。

一句话:引擎是库,二进制只是它的一个薄壳。

## 2. Workspace 六层分层

workspace 共 **45 个 crate**,按 6 层分组于 `crates/<layer>/<crate>/`,
依赖方向**自上而下**(上层依赖下层;`protocol` 是所有层的公共契约,被所有
层依赖)。完整成员清单以根目录 [Cargo.toml](../Cargo.toml) 的 `members` 段
为唯一事实源;下表与其逐行对应。

| 层 | Crate | 职责 |
|----|-------|------|
| `protocol/` | `zhuye-protocol` | `Submission` / `Op` / `Event` / `EventMsg` / `Item` 基础类型 + `RolloutRecorder` trait。所有层的公共契约 |
| `abilities/` | `llm`, `tools`, `bm25`, `hooks`, `skills`, `memory`, `agent-def`, `prompt`, `compact`, `recovery`, `notes`, `sanitize`, `kms`, `eval` | 能力原语:`ModelClient` trait + providers、`Tool` trait + 内置工具、BM25 相关性排序、`HookEngine`、skills 目录、跨会话记忆、Agent 定义、提示词库、压缩 escalation、断点恢复、会话笔记、输出脱敏、密钥管理、评测体系 |
| `resources/` | `config`, `permissions`, `plugin`, `rollout`, `telemetry`, `sandbox`, `audit`, `secrets`, `fs`, `git`, `ast`, `wasm` | 配置热重载、权限规则、插件 lifecycle、JSONL 持久化、可观测性遥测、OS 沙箱(macOS Seatbelt / Linux Landlock)、审计日志、密钥加密存储、共享文件原语(原子写)、git 能力族(checkpoint/branch/指令解析)、tree-sitter AST 搜索、WASM 组件插件宿主 |
| `orchestration/` | `subagent`, `discussion`, `task`, `pipeline`, `goal` | 多 agent 编排:子代理工厂(并发在途 ≤16)、角色讨论、任务/团队/squad 委派、DAG 流水线、目标模式 |
| `integrations/` | `mcp`, `lsp`, `integration`, `stream`, `import`, `computer-use`, `shell`, `git-tools` | MCP 客户端(stdio / streamable-http)、LSP、外部集成、流式会话后端(含 cron / autopilot / 队列)、外部 Harness 会话导入、Computer Use(截屏/键鼠,`real` feature 门控)、常驻 shell 会话池、git 工作流 MCP server |
| `runtime/` | `core`, `exec`, `cli`, `zhuye`, `py` | `AgentThread` + `StateGraph`(core)、headless + serve(exec)、CLI 入口(cli)、库门面 + Builder(zhuye)、PyO3 绑定(py) |

分层规则:依赖只允许从上往下流动。`protocol` 在最底,任何层都可以依赖它;
`runtime` 在最顶,聚合所有层对外暴露入口。`orchestration` 与 `integrations`
同处中上层,彼此不依赖,只共同依赖 `abilities` / `resources` / `protocol`。

## 3. 协议与会话生命周期

客户端与引擎之间只有一套契约,定义在 `protocol` 层,贯穿 exec / serve /
lib 门面 / SDK:

- **`Submission` / `Op`** —— 客户端 → 引擎。一个 `Submission` 携带一个
  `Op`(用户输入、工具审批回执、compact、rewind、set_permission_mode、
  query_guard_stats 等)。serve 模式下 stdin 每行一个 `Submission` JSON。
- **`Event` / `EventMsg`** —— 引擎 → 客户端。每个 `Event` 带一个
  `msg: EventMsg` 变体(turn_started / agent_message_delta /
  tool_call_begin / tool_execution_request / approval_request /
  turn_complete / …)。serve 模式下 stdout 每行一个 `Event` JSON。
- **`Item` 基础类型** —— `ContentBlock` / `ToolError` / `ChatMessage`
  等共享数据结构(`item.rs`)。`Item` 是这些类型的模块概称,没有同名公开
  类型。
- **`RolloutRecorder` trait + `RolloutRecord`** —— 持久化。JSONL
  落盘(`~/.zhuye/sessions/YYYY/MM/DD/<thread>.jsonl`),记录
  `SessionMeta` / `Message` / `Compaction` / `Fork` / `Checkpoint` /
  `DiscussionTranscript` / `TokenCount` 等。

### v0 冻结与 additive 规则

**Protocol v0 已冻结**:新增 `Op` / `EventMsg` 变体属 non-breaking,
**不可改动既有变体**(改字段 = breaking,禁止)。wire 稳定性是所有客户端
(TUI / exec / serve / SDK)共同的前提;判别符为 serde `snake_case`,
**容忍未知 `"type"` 是客户端义务**。

近期的 additive 例子(只增不改):

- `QueryGuardStats` / `GuardStats`(prompt injection guard 裁决统计快照);
- `ToolCallEndEvent` 新增可选 `args` 字段(带出工具调用参数);
- `tool_call_args_delta`(流式工具参数增量);
- `submission_closed`(非 turn 操作确定性收尾)。

完整 wire 语义、握手、并发顺序、远程工具执行流、退出与错误格式见
[sdks/PROTOCOL.md](../sdks/PROTOCOL.md);hooks 在 serve 上的默认行为(内置
hook 默认不启用)见该文档 §3.1。

## 4. 一轮执行与状态转移

引擎的处理没有黑魔法。`AgentThread`(`zhuye-core`)持有 `Submission` 的
`mpsc` 通道并 `tokio::spawn` 启动 `submission_loop`;`submission_loop` 循环
接收 `Submission` 分派 `Op`,其中 `UserInput` 分支构造并运行 4 节点
`StateGraph`(`crates/runtime/zhuye-core/src/agent_thread.rs:57-68`、
`submission_loop.rs`)。

`StateGraph` 走 **4 节点**:`PreLoop → ModelCall → (ToolExec → PreLoop)* →
CheckStop`。转移由 `GraphNode::next(has_tool_calls)` 用枚举 + match 手写,
不引入 petgraph(`graph/mod.rs:28-40`):

- `PreLoop` → `ModelCall`(无条件)
- `ModelCall` 有工具调用 → `ToolExec`;无工具调用 → `CheckStop`
- `ToolExec` → `PreLoop`(工具结果回灌模型,继续多步推理)
- `CheckStop` 允许 → 结束 turn;Stop hook 否决 → 反馈追加历史后回
  `PreLoop`(强制续跑,见 `graph/nodes/check_stop.rs:20-43`)

```
PreLoop ──▶ ModelCall ──▶ ToolExec ─┐
   ▲            │  ▲               │
   │            ▼  │               │
   │         CheckStop◀────────────┘
   │            │
   │   (Stop hook 否决 → 回 PreLoop)
   │            │
   └────────────┘   (允许 → turn 结束)
```

各节点职责:

| 节点 | 职责 |
|------|------|
| PreLoop | 压缩 escalation 判定(按输入 token 阈值逐级升级)、hooks 前置处理、history 组装、转向队列收割 |
| ModelCall | 经 `RoutingPolicy` 解析主模型后调用 `ModelClient`(SSE 流式),产出文本/思考增量与工具调用 |
| ToolExec | 审批 gate 检查 → 经 `ToolExecutionQueue` 执行工具(内置 / MCP / 远程)→ 结果回填 history;Plan mode 在此切换 |
| CheckStop | 模型无新工具调用时判定回合结束;分发 Stop hook,可否决强制续跑 |

Stop hook 否决时,`check_stop` 按决策叶子的 `deny_reason` 判定否决,把 hook
注入的提醒(如失败测试输出)以 `<system-reminder>` User 消息追加进历史,然后
回到 `PreLoop` 让模型看到否决原因并续作;`stop_hook_attempts` 计数,
max-iteration 安全阀兜底防止无限续跑。

异常与取消:`model_call` / `tool_exec` 因错误或 `CancellationToken` 触发而
返回 `None` 时,turn 为**异常结束**,不标记 `completed_normally`;只有
`CheckStop` 自然终止才置位。取消来自用户中断、超时或预算耗尽,`TurnHandle`
随之关闭。

### 编排、路由、恢复的实际消费点

- **目标续作。** `EnterGoalMode` 之后,每轮 `TurnComplete` 调用
  `GoalController::on_turn_end`。未完成时续作文本推进 `steering_queue`,
  下一条 `UserInput` 在 `submission_loop` 里 `drain_all` 并进那条用户
  消息。引擎不会自己再提交一轮。
- **流水线失败策略。** `Abort` 与 `ContinueCollect` 都不提前结束循环。
  失败节点的下游一律 `skipped`,没有依赖的旁支照跑。总状态分别是
  `failed` 和 `partial`。
- **路由。** `model_call` 消费 `Role::Main` 的 primary、fallbacks 和凭据
  cooldown。构造摘要器时把 `Role::Compact` 的 primary 拷进 model_name;
  摘要调用只对这一个 spec 做 next_for,不读 compact 的 fallbacks。子代理用
  启动时的主模型,或 `[subagent_providers]` 的独立 registry。
- **上下文恢复。** 活跃文件只在 `compact_triggered`(任意非 Noop 压缩)时
  重读。子代理登记和会话笔记每个 `pre_loop` 都注入,再由 `model_call`
  追加进消息。

## 5. 节点上下文与能力装配

`NodeContext` 是一个**可廉价克隆**的回合级依赖包(`submission_loop.rs:65-155`),
每个 `StateGraph` 节点持其 clone。字段按职责分组:

- **身份与回合**:`turn_id`、`session_id`(M5,跨 turn 稳定,供 rollout
  recorder 标记同 session)。
- **模型与路由**:`model`(`Arc<RwLock<String>>`,热重载切 model 下一请求立刻
  生效)、`policy`(`RoutingPolicy`,角色 → spec slot)、`registry`
  (`SharedModelRegistry`,凭据池内自动切下一个)、`effort`(会话级 reasoning
  effort,`/effort` 切换影响下一轮)。
- **hooks 与工具**:`hook_engine`(`HookEngine`)、`tools_queue`
  (`ToolExecutionQueue`,内置 / MCP / 远程统一入口,内挂审批与脱敏)。
- **事件与取消**:`event_tx`(`mpsc::Sender<Event>`)、`cancel`
  (`CancellationToken`)、`approval_gate`(回合级审批 gate,headless 免确
  认时为 None)。
- **预算与用量**:`max_iterations`、`session_usage`(会话累计 token)、
  `token_budget`(硬上限,累计超阈值 `model_call` 提前返回 None)、
  `quota_tracker`(token plan 配额,耗尽触发凭据 cooldown)。
- **压缩与转向**:`force_compact_next`(`/compact` 强制标志)、
  `steering_queue`(回合中途边跑边改需求)。
- **可观测与目标**:`telemetry`(`TelemetrySink`,None = 关闭)、`goal`
  (目标模式 controller,turn 结束自校验 + 续作)、`recorder`
  (`RolloutRecorder`)。
- **Plan mode**:`plan_approval_gate`、`plan_session_subs`(会话级扇出)、
  `cfg`(共享 `AgentConfig`,让审批决策能翻转会话级 permission_mode 槽)。
- **沙箱**:`sandbox_override`(OS 沙箱策略每 turn 下发,经
  `ToolContext.os_sandbox` 透传给 BashTool)。

能力装配:`AgentThread::new` 把 sanitizer(输出密钥脱敏)、`HookEngine`
(含 builtin + 插件 hook)、prompt injection guard、会话级 permission_mode
句柄、permission resolver 注入 `ToolExecutionQueue`;`NodeContext` 的 clone
再分发给各节点。工具面裁剪见下节。

### 工具面裁剪(发给 LLM 的 schema)

发给 LLM 的工具 schema 由 `ToolRegistry::list_specs()` 产出后再经 PreLoop
过滤 —— 未注册或被裁剪的工具**根本不进 prompt**。核心内置工具共 **27 个**
= 25 无条件 + 2 门控插件工具(`web_search` / `notebook_edit`,默认开,可经
`[plugins].disabled_plugins` 整体禁用,禁用后不进注册表)
(`builtin_tools.rs`,三条路径 exec / serve / lib facade 共用这一份注册
清单):`echo`、`bash`、`background_status`、`read`、`write`、`edit`、
`delete_file`、`grep`、`glob`、`EnterPlanMode`、`ExitPlanMode`、
`PlanWrite`、`EnterWorktree`、`ExitWorktree`、`ast`、`web_fetch`、
`web_search`、`ask_user_question`、`ask_user`、`request_human_input`、
`tool_search`、`image_view`、`notebook_edit`、`brief`、
`get_context_remaining`、`checkpoint`、`rewind`。注意:这是核心注册
清单,**不是全集** —— 编排、cron、插件可再注册其他工具。

裁剪有三层入口,按"面向谁"划分:

1. **Agent 定义层(Markdown frontmatter,主入口)**。放在
   `.zhuye/agents/`(workspace)或 `~/.zhuye/agents/`(home;同名时 home 覆
   盖 workspace),`zhuye exec --agent <name>` 激活。语义为**白名单 ∩、黑名
   单 −、`readonly` 排除变更类**;三字段全空则不过滤(向后兼容)。Plan mode
   下 `write` / `edit` 会被强制移除,LLM 只能用路径受限的 `PlanWrite`。
2. **子代理层(config.toml / Markdown)**。`[[subagents]]` 的
   `allowed_tools` 白名单决定子代理工具面;spawn 时为子代理构造**仅含白名
   单工具**的过滤后 registry。来源合并优先级(后者覆盖同名):
   `~/.zhuye/subagents/*.md` → workspace `.zhuye/subagents/*.md` →
   config.toml `[[subagents]]`。
3. **代码层(ToolRegistry API)**。库用户经 `agent.thread().tools()` 拿
   registry 直接增删;`register_except` 签名带 source 首参:
   `registry.register_except(source, tools, excluded)`。

## 6. 设计决策

- **Protocol v0 已冻结**:新增 `Op` / `EventMsg` 变体 = non-breaking,不可
  改动既有变体。wire 稳定性是所有客户端共同的前提。
- **StateGraph 用 enum + match 手写转移**:4 节点固定结构,不引入 petgraph ——
  控制流显式、可审计,这是框架相对黑盒 agent 循环的核心差异之一。
- **`ToolError` 定义在 `protocol` 层**(`item.rs:515`):打破
  `tools ↔ hooks` 循环依赖(让 `HookEvent::PostToolUseFailure` 能携带它),
  错误类型下沉到公共契约层。
- **错误处理**:`thiserror` / `anyhow` 是约定而非排他规则 —— 库 crate 用
  `thiserror`(错误类型显式),应用层(exec/cli)用 `anyhow`(错误链传
  播);zhuye-core 两者都用。
- **MSRV 与 edition**:Rust stable,**MSRV 1.89**、`edition = "2024"`(安全
  依赖面要求,见根 Cargo.toml `workspace.package`)。
- **JSONL stdout,日志 stderr**:`zhuye exec | jq` 不破坏 —— 事件流与诊断
  日志严格分流,管道消费方永远拿到纯 JSONL。
- **serve 是嵌入入口,不是开发助手**:内置 hook 默认不启用(exec / TUI 保持
  "未配置 = 全启用"),避免 verification 之类 Stop hook 在宿主 cwd 跑测试。
  serve 的宿主是 SDK 使用者的进程,而非开发者本人。
- **profile 策略**:`[profile.release]` 用 thin LTO 供发布分发;
  `[profile.release-fast]` 无 LTO、高并行度(opt-level 2、incremental),供
  本地快速迭代;`[profile.dev]` 刻意关闭 incremental(见根 Cargo.toml 注
  释)。
- **facade 导出数量不写死**:`zhuye` 库门面的 re-export 数量随版本持续变
  化,本文只作定性描述;zhuye-stream 的 cron / autopilot / 队列类型已通过
  facade re-export(`lib.rs:125-130`)。
