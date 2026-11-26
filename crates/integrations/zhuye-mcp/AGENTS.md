# zhuye-mcp AGENTS.md

> **何时读我**:改动 MCP 客户端(连接生命周期 / transport / 工具适配)、MCP 协议形状或 OAuth / 自暴露 server 扩展时,读本篇。

## 本 crate 是什么

MCP(Model Context Protocol)客户端集成:把每个外部 MCP server 暴露的 tool 经
`Tool` trait 包装后注册进 `ToolRegistry::Runtime`,另含 resources / prompts 两类
原语工具与自暴露 server(F30)。协议版本固定 `2025-06-18`;transport 三态:
stdio / streamable-http / legacy SSE。

## 核心文件导航

| 路径 | 一句话职责 |
|---|---|
| `src/lib.rs` | crate 门面:公开类型扁平 re-export + `McpError`(生命周期错误,不进 execute 路径) |
| `src/config.rs` | MCP server 强类型配置(`McpServerConfig` / `McpTransport` / `McpOAuthConfig`) |
| `src/protocol/` | 协议基础形状:`content.rs`(MCP Content ↔ ContentBlock 转换)、`version.rs`(`PROTOCOL_VERSION` 常量) |
| `src/protocol_ext/` | 协议完整性扩展:`oauth.rs`(F29 OAuth / elicitation / sampling)、`server_mode.rs`(F30 自暴露 server) |
| `src/client/manager.rs` | 连接生命周期:`start_server` / reload 增量 diff(added/removed/changed)/ shutdown |
| `src/client/tool.rs` | `McpToolAdapter`:单个 MCP tool → `zhuye_tools::Tool` 适配器 |
| `src/client/transport.rs` | stdio / streamable-http / SSE transport 构造 + `env_clear()` 安全注入 |
| `src/tools/prompts.rs` | `ListMcpPromptsTool` / `GetMcpPromptTool`(v1.6 接线) |
| `src/tools/resources.rs` | `ListMcpResourcesTool` / `ReadMcpResourceTool`(v1.2 起真接线) |
| `src/catalog/registry.rs` | 精选常用 MCP server 目录(条目 + 安装提示,stub 性质) |
| `src/bin/mock_mcp_server.rs` | 极简 MCP stdio mock,仅本 crate 测试用 |

## 本地契约与同步点

- **外部只走 `zhuye_mcp::<Type>` 扁平路径**:公开类型全部在 `src/lib.rs` 根级
  re-export(`McpConnectionManager` / `McpToolAdapter` / `McpServerConfig` 等),
  调用方不得深入 `client::*` / `tools::*` 子模块路径 —— 否则 restructure 时
  下游跟着断。唯一例外:`transport` 模块保留旧路径别名
  (`pub use client::transport`,见 `lib.rs`),维持 `zhuye_mcp::transport::build_stdio`
  等既有调用点;新代码一律用扁平 re-export 的类型,不得再新增旧路径引用。
- **工具名强制 `mcp__<server>__<tool>` 前缀**(定义在 `client/tool.rs`):与内置
  工具不冲突;破坏了会顶掉内置工具或让审批规则失配。
- **`Tool::execute` 失败一律映射回 `zhuye_protocol::ToolError`**,`McpError` 只用于
  `start_server` / `reload` 等生命周期操作(见 `lib.rs` `McpError` doc)。
- **stdio 子进程 env 用 `env_clear()` 隔离**(`client/transport.rs`):只注入
  `HOME` / `PATH` + 用户显式 env,防止 `OPENAI_API_KEY` 等凭据泄入第三方子进程;
  `kill_on_drop(true)` 防 zombie。改 env 逻辑须与 `zhuye-lsp` 同口径。
- **能力清单登记在 `src/lib.rs` 头部**(已实现 / 未实现),新增实现要同步改那处
  清单;Sampling 与授权码流程的浏览器交互明确未实现,sampling 能力**不声明**。
- **OAuth 硬规则**(`protocol_ext/oauth.rs`):server 请求授权而无凭据时报
  `McpError::AuthRequired`(含可执行端点提示),不代跑浏览器;sampling 请求必须走审批。

## 禁止事项

1. 不要绕过扁平 re-export 从下游直接 `use zhuye_mcp::client::manager::...`。
2. 不要在 `McpError` 之外为 execute 路径另造错误类型;也不要把 rmcp 的错误类型
   泄漏到公开 API。
3. 不要改动既有 `McpTransport` 变体的 wire 语义(`Sse` 是 legacy 别名,委托
   streamable-http,只兼容不扩展)。

## 深入阅读

- [docs/architecture.md](../../../docs/architecture.md) —— §5 节点上下文与能力装配
- [crates/integrations/AGENTS.md](../AGENTS.md) —— 本层通用规则(env 隔离 / 工具命名)
