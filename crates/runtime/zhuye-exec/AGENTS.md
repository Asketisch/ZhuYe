# zhuye-exec AGENTS.md

> **何时读我**：改 headless 单轮（`zhuye exec`）或常驻会话（`zhuye serve`）的装配、参数结构、JSONL 输出、退出码语义、配置热重载时，读本篇。

## 模块概述

headless 与 serve 的共享装配层：读参数、bootstrap 会话、把协议事件以 JSONL 流写 stdout，并按终结事件映射进程退出码。`zhuye-cli` 的 `exec` / `serve` 子命令复用这里的参数结构与入口。

## 核心文件导航

| 文件 | 一句话职责 |
|------|-----------|
| `src/lib.rs` | `ExecArgs` 参数结构、`PLAN_MODE_EXIT_CODE`、`run` 入口与终结事件到退出码的映射 |
| `src/headless.rs` | `HeadlessArgs` 与 exec / serve 共用的会话装配层 |
| `src/serve.rs` | `ServeArgs` 与常驻 stdio JSONL 会话服务（双 SDK 的协议入口） |
| `src/bootstrap.rs` | 会话引导：装配 agent 定义 / 记忆 / 技能 / 压缩器等依赖 |
| `src/jsonl.rs` | stdout 流的 JSONL 事件 writer |
| `src/runtime_config.rs` | 运行时配置构造器：tracing 初始化、telemetry sink、quota tracker |
| `src/reload.rs` | 配置热重载：registry 替换、section diff、事件推送 |
| `src/builtin_hooks.rs` | 内置上下文注入 hooks（AGENTS.md 注入等），归属 exec 层不进引擎循环 |

## 本地契约与同步点

- **stdout / stderr 分工是硬不变量**：协议事件以 JSONL 写 stdout，tracing 日志写 stderr（`src/runtime_config.rs` 的 `init_tracing`）。`zhuye exec | jq` 依赖这条，任何日志混进 stdout 都是回归。
- **压缩热配置**：模型或配置变更时刷新压缩阈值策略与模型窗口覆盖；每次压缩决策以当前模型窗口计算有效阈值。
- **`ExecArgs`（`src/lib.rs`）与 `ServeArgs`（`src/serve.rs`）是两个入口的参数结构**，也是 CLI 文档的同源定义；`zhuye-cli` 的 clap 外壳投影自它们。改参数先改这里，再改 CLI 侧投影。
- **恢复链三选一**：`--resume <UUID>` / `-c`（`continue_last`）/ `-r N`（`resume_by`）在 clap 层用 `conflicts_with_all` 互斥；`async_main` 另有 `if/else if` 优先级链兜底（`--resume` 优先于 `-c` / `-r`，二者优先于 `prompt`）。两边都要改才能保持一致。
- **`--prompt-profile` 三态提示词方案**（`ExecArgs::prompt_profile` + `src/lib.rs` 的 `apply_prompt_profile`）：取值 `default` / `auto` / 方案标识，映射 `PromptSelection` 后在会话装配前以一条 `Op::PromptCommand { Select }` 提交生效；与 resume 输入不互斥，仅设定本会话的提示词选择。方案库与来源登记见 abilities 层篇 `zhuye-prompt` 条目。
- **`PLAN_MODE_EXIT_CODE`**（`src/lib.rs`）：`--plan-mode` 下计划未产出时的退出码，`docs/PLAN_MODE.md` 对它有明文承诺，改它要同步文档与依赖该码值的调用方脚本。
- **hooks 启用口径**由 `HeadlessArgs::hooks_explicit_only`（`src/headless.rs`）决定：serve 侧收紧为 `true`（内置 hook 需在配置段显式列出才启用），exec / 库侧保持 `false`（未配置即全部启用）。
- **门控型 bundled 插件的装配接线**（`src/headless.rs` `bootstrap_common`）与 hooks 收紧共用同一插件判定源：builtin 注册经 `zhuye_plugin::bundled_tool_gates(enabled, disabled)` 构造 `BundledToolGates` 传入 `register_builtin_tools_with_gates`（web-search / notebook 默认开，关闭 = 不注册）；verify-hooks 插件启用时在 explicit-only 收紧之后 `enable_hook_names` 注入；`bootstrap_lsp` 的 `lsp_plugin_disabled` 参数承载 lsp 插件粗粒度开关。goal-queue 的 gate（`wire_goal_queue_plugin`）判定同样委托 `zhuye_plugin::bundled_plugin_enabled`。
- **telemetry 与 activity 开关**：sink 由 `src/runtime_config.rs` 的 `build_telemetry_sink` 构造；activity 审计是否落 `~/.zhuye/activity/activity.jsonl` 由配置段的 `activity` 开关决定（父开关 `enabled` 关闭时一并关闭）。
- **MCP elicitation 桥接**（`src/bootstrap.rs` `bootstrap_m6`）：服务端请求转成全局 `EventMsg::McpElicitation`，回执 waiter 登记在 `AgentConfig.approval_context.mcp_elicitations`（与父子代理共享同一张表）。登记必须**先于**发事件，否则宿主极快回执会丢；宿主不回执时按 TTL（默认 300s，`ZHUYE_MCP_ELICITATION_TTL_SECS` 可调）以 `cancel` 了结（fail-closed）。
- 改了参数面或事件形状，同步 `docs/cli.md` 的子命令总览表与 `tests/functional/` 下的能力用例。

## 禁止事项

1. 不要把日志、调试信息或提示文本写到 stdout——stdout 只允许出现 JSONL 事件流。
2. 不要绕过 `HeadlessArgs` 另起一条装配分支——exec 与 serve 必须共用同一层装配，否则两条路径的行为会漂移。
3. 不要在 `zhuye-exec` 里引用 `zhuye-cli`，也不要把 CLI 层的解析逻辑搬进来。
4. 不要改 `PLAN_MODE_EXIT_CODE` 而不同步 `docs/PLAN_MODE.md` 与依赖它的脚本。
5. 不要把 serve 侧的内置 hook 恢复成默认全开——收紧是刻意的，嵌入方的宿主工作目录里跑验证命令既慢又不可预期。
6. 不要用 `print!` / `println!` 往 stdout 打任何非事件内容。

## 注意事项

- `src/lib.rs` 是库入口、`src/main.rs` 是独立二进制入口，`zhuye-cli` 复用库入口而不是二进制。
- serve 的主 writer task 长期持有 stdout 锁；另起线程写 stdout 时要按事件逐次构造 writer，持久持锁会互相阻塞。
- `src/tests.rs` 是从 `lib.rs` 内联迁移而来的单元测试，改行为时同步更新断言。
- 注释与文档一律中文；应用层错误用 `anyhow`。

## 深入阅读

- [docs/architecture.md](../../../docs/architecture.md) —— §1 框架边界与入口、§4 一轮执行与状态转移
- [docs/cli.md](../../../docs/cli.md) —— `exec` / `serve` 的对外参数说明
- [docs/PLAN_MODE.md](../../../docs/PLAN_MODE.md) —— Plan mode 流程与退出码承诺
