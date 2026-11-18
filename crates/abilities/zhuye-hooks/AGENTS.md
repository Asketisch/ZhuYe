**何时读我**:你要新增 hook 事件或决策、改内置 hook、改 shell hook(外部命令式 hook)、或调整 hook 启用策略时,先读本篇。

# zhuye-hooks —— Hook 引擎与事件 / 决策协议

## 本 crate 是什么

`HookEngine`(注册、派发、合并)、hook 的事件与决策类型、内置 hook 实现、以及把 hook 逻辑委托给外部进程的 shell hook。
本 crate **不依赖 `zhuye-tools`**(依赖方向是 `zhuye-tools` → 本 crate,单向):hook 事件不携带 `ToolContext`,需要上下文时只读事件上暴露的精简字段(workspace / session_id / turn_id / permission_mode),完整上下文由工具实现自己在 `Tool::execute` 里拿 —— 这是为了不与 `zhuye-tools` 成环。

## 本地契约

### 1. 事件与决策是对外协议,按 additive 规则演进

- `src/event.rs` 定义事件种类(`HookEventKind`),`src/decision.rs` 定义决策种类(`HookDecision`)。
- 两者都是**对外可观察**的:shell hook 以 JSON 形式收事件、回决策,插件 hook 也走同一套;因此**既有变体的语义不可改动**,新增只能是 additive。
- 新增变体时同步:`docs/tools-and-hooks.md` §4.1(事件表)与 §4.2(决策表)、以及双 SDK 遇到未知变体时的降级路径(未知变体要能降级不崩)。
- 同一事件下多个 hook 返回的决策由 `HookEngine::merge` 按文档 §4.3 的优先级合并;`HookDecision::Combined` 只展平一层。

### 2. shell hook 的执行协议以本 crate 为准

`src/shell.rs` 是外部命令式 hook 的权威实现,协议要点:

- **方向固定**:事件序列化为 JSON 写进子进程 **stdin**,进程以 shell 执行,决策经 **stdout** JSON 回传。
- **fail-closed**:命令超时或非零退出 → 判为 `Deny`(原因取 stderr / stdout 摘要)。外部守卫不可用时宁可拒绝执行,**不得改成放行**。
- **env 卫生**:默认剔除名称命中 `SECRET_ENV_PATTERNS` 的环境变量再传给子进程;hook 可在 `hooks.json` 的 `env` 字段显式列名放行(白名单优先于剔除)。
- **matcher**:仅对携带工具名的事件(`PreToolUse` / `PostToolUse`)生效,支持通配与精确名(大小写不敏感);不匹配则直接 Allow,不启动子进程。

### 3. 内置 hook 与启用语义

- 内置 hook 实现放在 `src/builtins/`;各自的配置段在 `[hooks]` 下(如 `[hooks.search_budget]`),schema 见 `docs/tools-and-hooks.md` §5.5。
- `HooksConfig.enabled` 为 `None` 表示**全部启用**;调用方可收紧这一默认 —— 例如 serve 嵌入场景通过 `hooks_explicit_only`(定义在 `crates/runtime/zhuye-exec/src/headless.rs`)把「未配置 = 全部启用」收紧为「未配置 = 不启用任何内置 hook」,显式列出的照常启用。**这是调用方的选择,不是本 crate 的默认行为**,改默认值前先确认 serve 场景的意图。

## 核心文件导航

| 文件 | 一句话职责 |
|---|---|
| `src/hook.rs` | `Hook` trait 与 `HookError` |
| `src/event.rs` | hook 事件类型与事件种类 |
| `src/decision.rs` | 决策类型、合并与展平 |
| `src/engine.rs` | `HookEngine`:注册、派发、合并 |
| `src/config.rs` | `[hooks]` 配置段 → `HookEngine` 装配 |
| `src/shell.rs` | 外部命令式 hook 执行协议 |
| `src/builtins/` | 内置 hook 实现 |
| `src/file_read_state.rs` | 会话级「已读过该文件」状态 |

## 禁止事项

- 不得改动既有事件 / 决策变体的语义(协议冻结)。
- 不得让 `HookEvent` 携带 `ToolContext` 或任何来自 `zhuye-tools` 的类型(会成环)。
- 不得把 shell hook 的失败从 fail-closed 改成放行,也不得把完整父环境透传给子进程。
- 不得把内置 hook 的默认启用策略在本 crate 内硬改 —— 收紧策略由调用方通过配置表达。

## 深入阅读

- [docs/tools-and-hooks.md](../../../docs/tools-and-hooks.md) —— hook 事件 / 决策协议、内置 hook 与配置 schema、权限模式、审批门(本 crate 的系统级说明)
- [docs/plugin-dev-guide.md](../../../docs/plugin-dev-guide.md) —— 插件 `hooks.json` 与 shell hook 的写法
- [docs/configuration.md](../../../docs/configuration.md) —— `[hooks]` 配置段与环境变量
