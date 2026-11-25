**何时读我**:你要新增 / 修改 / 移除内置工具,或改动工具注册表、工具执行队列、审批门、路径沙箱时,先读本篇;改了工具清单或工具输出上限,必须按下方同步点走完。

# zhuye-tools —— 内置工具与工具执行

## 本 crate 是什么

`Tool` trait 与全部内置工具的实现所在,外加工具注册表、执行队列、审批门与路径沙箱。
`ToolError` / `ToolOutput` 定义在 `zhuye-protocol`(见 `tool.rs` 模块头),不在本 crate —— 这是为了打破 tools ↔ hooks 的循环依赖。

## 本地契约

### 1. 内置工具清单必须三处同步

新增或移除内置工具时,以下三处必须同时改到位,缺一即视为未完成:

| 同步点 | 落点 |
|---|---|
| 计数常量 | `src/builtin_tools.rs` 的 `BUILTIN_TOOL_COUNT`(全开门控时的总数)与 `UNGATED_BUILTIN_TOOL_COUNT`(无条件数) |
| 计数断言 | 同文件 `tests` 模块的 `registers_the_full_builtin_set` / `gates_off_skip_plugin_tools_only` |
| 权威清单 | `docs/tools-and-hooks.md` §2「内置工具契约」的名称与用途表 |

注册入口是 `register_builtin_tools`(全开,等价 `register_builtin_tools_with_gates(_, _, _, BundledToolGates::all_on())`),它统一注册核心内置工具;该清单只是**核心注册清单**,不是运行时可见工具的全集(编排 / cron / 插件 / MCP / SDK 远程工具会另外注册)。

**门控型插件工具**:web_search(web-search 插件)/ notebook_edit(notebook 插件)默认开,注册行在 `register_builtin_tools_with_gates` 内由 `BundledToolGates` 包裹 —— 关闭 = 不进注册表(区别于 `[active].disallowed_tools` 的可见性过滤)。判定不来自本 crate:装配层用 `zhuye_plugin::bundled_tool_gates(enabled, disabled)` 构造门(插件名 → 工具的映射只在该函数维护)。

### 2. AST 工具由调用方注入,不得反向依赖

`zhuye-ast` 依赖 `zhuye-tools`(它复用本 crate 的 `Tool` trait),所以 **本 crate 不得依赖 `zhuye-ast`** —— 反向依赖会成环。

因此 `register_builtin_tools` 把 AST 工具作为参数接收,由调用方注入具体实现;测试侧用替身(`FakeAst`)顶替,同样是为了避开成环。新增「需要别的 crate 提供实现」的工具时沿用这一形状:本 crate 只定义 trait 与契约,实现由上层注入。

### 3. 工具输出上限与文档同源

各工具有**各自的**截断上限,没有统一截断值;16 KiB 是 rollout JSONL 单字段的落盘上限,与工具输出无关。
引擎另对发给模型的所有来源工具文本统一控制上下文预算,不改变工具对宿主的原始结果;溢出文本由本 crate 的 `sandbox::write_spill_text` 安全落盘供 read 读回。
`read` 支持 `byte_offset` / `byte_limit` 字节分页，最大 4096 字节并返回下个 UTF-8 字符边界；不得与行分页混用。
`bash` 只读自动放行遇到展开、引号、转义、重定向或其他未解析语法时必须退回审批，包括不带空格的重定向。
Linux 前台 bash 的网络策略必须与后台 bash 同源；只要启用沙箱，安装失败不得裸执行。
Unix 前后台命令共用 `child_process::GroupedChild`：取消、超时或执行任务被丢弃时先终止独立进程组，再回收直接子进程；主动脱组的后代及 Windows 进程树不在此机制保证范围。
按字节截断必须走 `zhuye_protocol::truncate_str` / `truncate_string_in_place` 做 char 边界对齐 —— CJK 一个汉字 3 字节,直接 `String::truncate` / byte-slice 会在非边界处 panic。
**改动任一工具的输出上限时,`docs/tools-and-hooks.md` §2 的上限表必须同步**;两者是同一份口径的两个落点。

## 核心文件导航

| 文件 | 一句话职责 |
|---|---|
| `src/tool.rs` | `Tool` trait 与伴随类型 |
| `src/registry.rs` | name → 工具的注册表(带 source 标记) |
| `src/builtin_tools.rs` | 内置工具注册入口与计数常量 |
| `src/queue.rs` | 工具执行队列(串行 / 并行分组) |
| `src/sandbox.rs` | 路径沙箱(workspace-write 口径:读全盘 + 凭证拒绝;写限 `resolve_writable_path` 允许根 + `.git`/`.agents`/`.aws` 元数据只读;`.zhuye` 走提权子项 denylist(`PROTECTED_ZHUYE_CHILDREN`:config/sessions/cron 等),其余子树(spill/plan/worktrees/uploads)放行;`resolve_sandbox_path` 旧工作区墙保留给 LSP) |
| `src/approval/gate.rs` | 审批门与决策路由 |
| `src/approval/mod.rs` | `ApprovalContext`：waiter 表族（审批 / 问答 / 询问 / MCP elicitation）+ 会话放行集 + 挂起交互注册表 |
| `src/builtins/` | 各内置工具实现(按工具分文件) |
| `src/loop_guard.rs` | 重复调用 / 无进展的循环守卫 |
| `src/pending_subagent.rs` | 在飞异步子代理登记表(`PendingSubagentRegistry`)—— 异步派发写、引擎挂起等待读 |

## 在飞异步子代理登记表

`src/pending_subagent.rs` 的 `PendingSubagentRegistry` 是「异步子代理派发」与
「回合挂起激活」两侧共用的状态:

- **工具侧**(`ToolContext.pending_subagents`):派发时 `register`(带父回合的
  回合级取消令牌 `owner`,返回放弃句柄),子代理终态时 `complete`;
- **引擎侧**(`zhuye-core` 的 `NodeContext.pending_subagents`):回合收尾前
  `wait_any` 挂起等待,拿到结果后注入上下文并续跑本轮;回合结束时
  `abandon_turn` 放弃本回合仍登记在表的条目。

四条不变式:

1. **下沉到本 crate 是依赖方向的必然**:依赖是 `zhuye-subagent → zhuye-core →
   zhuye-tools`,`zhuye-core` 不能反向依赖 `zhuye-subagent`。登记表必须能被
   工具与引擎同时拿到,只能落在两者共同的下游。
2. **每线程一份**:基座 `ToolContext` 在 `AgentThread::new` 里按 `AgentConfig`
   构造,`AgentConfig::new` 为每个线程新建一张表 —— 嵌套场景下子代理用自己
   的表,孙代理的完成不会误唤醒父回合。
   **同线程上可能并发跑着多个回合**(`submission_loop` 每条 UserInput 都 spawn
   一个回合任务),因此表内每条登记都带**派发回合的令牌**(`owner`):等待与取走
   一律按它筛选(`has_pending_for` / `wait_any_for` / `take_completed_for`),
   跨回合无差别版本(`has_pending` / `wait_any` / `take_completed`)只供单测与
   诊断用。否则 A 回合的子代理会被 B 回合等到,并把结果注入 B 的上下文 ——
   B 的模型会看到一份自己没派发过的子代理结果。
3. **异步子代理不跨越派发它的回合**:后台任务必须同时监听条目的放弃句柄与
   父回合令牌(任一触发即丢弃 `SpawnedChild` = 取消子代理)。同步模式下
   「打断父回合 ⇒ 取消在途子代理」是工具 future 被丢弃的副产品,异步的后台
   任务不随工具 future 析构,**不显式接线就不会级联**。`abandon_turn` 按
   `owner` 令牌匹配(同一线程上可能叠着别的回合的条目,不能清空整表)。
4. **终态首个为准,迟到 / 未登记的终态丢弃**:`complete` 幂等(先到者胜);
   条目已被 `take_completed` 取走或被放弃后再写入一律忽略、不重建条目 ——
   否则兜底守卫的补写会让下一回合凭空收到一条「子代理执行失败」。
   `PendingCompletionGuard` 只兜**后台任务意外中止**(runtime 掉线、panic):
   条目不能永远停在「在飞」,否则此后每个回合的 `check_stop` 都为它挂起。

## v1.x 新增能力

- **工具结果预算**(`src/result_budget.rs`):提供独立预算工具及队列可选接口。运行时配置在宿主事件发出后应用于模型上下文；持久化输出写入工作区的安全溢出目录，保持可读回与恢复一致。
- **deferred_call**(tool-defer 门控插件,默认关):只有入口已注册且目标来自 MCP 时,queue 才在**派发前**展开调用,审批/hook/floor 按真实工具走;直接执行、嵌套转发和非 MCP 目标均拒绝。`registry.deferred_tool_names()` 供 core 目录公告。
- **stop_turn 级联**:工具以 `metadata["stop_turn"]=true` 请求结算后收口 turn(置 `force_final_answer`,结果仍完整回灌)。

## 禁止事项

- 不得在本 crate 与 `zhuye-hooks` 之间另造错误类型 —— 一律复用 `zhuye-protocol` 的 `ToolError`。
- 不得依赖 `zhuye-ast`(见上文反依赖约束)。
- 不得在新增工具时跳过「清单三处同步」,也不得只改文档不改断言。
- 不得把工具输出改成统一截断值,或绕过 char 边界对齐直接按字节切。

## 深入阅读

- [docs/tools-and-hooks.md](../../../docs/tools-and-hooks.md) —— 内置工具契约、事件与决策协议、队列模型、审批门、沙箱(本 crate 的系统级说明)
- [docs/architecture.md](../../../docs/architecture.md) —— 工具在 StateGraph 中的位置
- [docs/development.md](../../../docs/development.md) —— 改动流程与验证矩阵
