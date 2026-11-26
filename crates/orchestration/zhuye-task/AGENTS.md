# zhuye-task AGENTS.md

> **何时读我**:改动结构化任务(Task*)、团队(TeamCreate/Delete)、TodoWrite、squad 委派或 Coordinator 模式时,读本篇。

## 本 crate 是什么

结构化任务管理 + 团队管理 + Coordinator 多 agent 协调中枢(v1.1.0 起):
`TaskManager` 组合两个 store 提供任务 / 团队高层 API,`tools/` 落成 builtin
工具(`register_all` 注册 11 个;note 两工具需 scratchpad 路径,由 Coordinator
启用时另行注入),`coordinator.rs` 提供 worker 装配与 scratchpad。squad 在其上
做 Leader 委派语义层。

## 核心文件导航

| 路径 | 一句话职责 |
|---|---|
| `src/lib.rs` | crate 门面:公开类型扁平 re-export(`TaskManager` / `Task` / 12 个工具等) |
| `src/model.rs` | `Task` / `TaskStatus`(四态)/ `TeamFile` / `TeamMemberSpec`;时间用 `SystemTime` 不引 chrono |
| `src/error.rs` | `TaskError` 统一错误 + `From<TaskError> for ToolError` 转换 |
| `src/actor.rs` | Actor 兼容层:纯类型已下沉 `zhuye_protocol::actor`,本文件只留 `actor_from_team_member` + re-export |
| `src/store.rs` | `TaskStore` trait + 内存 / 文件两实现;文件布局 `$ZHUYE_HOME/tasks/<list>/<id>.json` + `.highwatermark` |
| `src/team_store.rs` | `TeamStore` trait:整个 team 一个文件 `<home>/teams/<name>.json`,写侧 advisory lock |
| `src/team.rs` | `team-lead@<team>` agent id 约定 + `validate_team_name`(与 `SubAgentSpec::validate` 同规则)+ `TeamMemberSpec → SubAgentSpec` 转换 |
| `src/manager/` | `TaskManager` 高层 API(CRUD + 钩子触发 + event_sink + `sync_team_specs`);`tests.rs` 同目录 |
| `src/squad.rs` | Squad + Leader 委派:复用 `TaskManager` / `TeamFile` 存储,不改 Task schema |
| `src/task_graph.rs` | F56:依赖图(拓扑 / 就绪队列 / 死锁检测)+ 重复防护 + 项目容器 |
| `src/coordinator.rs`(+`src/coordinator/coordinator_prompt.md`) | Coordinator 模式:开关(env / `[coordinator]`)、worker 工具白名单、scratchpad 路径、默认 prompt(`include_str!` 嵌入) |
| `src/tools/mod.rs` | `register_all`:11 个工具以 `ToolSource::Builtin` 注册的统一入口;note 两工具因需 scratchpad 路径**不在其中** |
| `src/tools/task/` | 8 个 Task 工具(task_create / get / update / list / stop / output / claim / release),同构 stateful |
| `src/tools/note_read.rs` / `note_write.rs` | coordinator scratchpad note 读写(单文件 64 KiB 截断);启用 Coordinator 时另行注入 |
| `src/tools/team_create.rs` / `team_delete.rs` | 团队创建 / 删除(走 Prompt 审批) |
| `src/tools/todo_write.rs` | V1 内存 todo 清单,输出到 `ToolOutput.metadata["todos"]` |

## 本地契约与同步点

- **状态文档 ≠ 事件流**:Task / Team 持久化到 `$ZHUYE_HOME/tasks/` 与
  `teams/` 下的 JSON 文件,**不复用** `zhuye-rollout` 的 JSONL 事件流;对生命
  周期的对外可见走钩子(`TaskCreated` / `TaskCompleted` / `TaskUpdated`,
  团队生命周期经 ToolCallEnd 透传)。
- **agent id 格式固定 `team-lead@<team>`**(`team.rs`);`validate_team_name`
  的字符规则(`[a-z0-9_-]+`)与 `zhuye-subagent::SubAgentSpec::validate` 保持
  一致,两类 spec 才能互转 —— 改任一侧规则必须两边同步。
- **Actor 纯类型在 `zhuye-protocol::actor`**(WP12:telemetry 需要 Actor 而分层
  禁止 resources → orchestration 依赖);`actor.rs` 仅保留引用本 crate
  `TeamMemberSpec` 的转换函数并 re-export 协议类型,`zhuye_task::Actor` 等公开
  API 名不变 —— 不要把协议类型搬回本 crate,也不要删这层兼容 re-export。
- **工具审批分级**(`tools/mod.rs`):`TaskUpdate` / `TeamCreate` / `TeamDelete`
  走 `required_permission = Prompt`(改持久化状态);其余 task 工具与
  `TodoWrite` 为 `Auto`(manager 内部 per-key 锁,重复调用天然安全)。新增工具
  时按同一口径分级。
- **TodoWrite 清单「只出不进」**:`metadata` 是输出通道,GUI 能展示但没有任何
  组件把它回灌进下一轮模型上下文 —— 这是 V1 与 Task 系统(V2)并存的既定
  设计,不要在引擎侧偷偷加回灌。
- **依赖方向**:`zhuye-task → zhuye-subagent`(层契约);`zhuye-pipeline`
  建立在本 crate 之上,team 节点能力是流水线阶段的唯一来源。

## 禁止事项

1. 不要绕过 `TaskManager` 直接读写 tasks / teams 目录下的 JSON 文件。
2. 不要让 `TaskError::NotFound` 语义映射成 `ToolError::InvalidArgs` —— 约定走
   `Execution`,让 LLM 把「不存在」当正常提示处理(见 `error.rs` 注释)。
3. worker 注册表不得包含 `INTERNAL_WORKER_TOOLS`(`TeamCreate` / `TeamDelete` /
   `send_message`)—— 白名单排除是 Coordinator 的安全边界。

## 深入阅读

- [crates/orchestration/AGENTS.md](../AGENTS.md) —— 本层「任务与团队」节与依赖约定
- [docs/features.md](../../../docs/features.md) —— 任务 / 团队编排的产品定位
