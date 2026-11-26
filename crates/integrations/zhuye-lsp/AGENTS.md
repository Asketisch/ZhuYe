# zhuye-lsp AGENTS.md

> **何时读我**:改动 LSP 客户端(连接管理 / JSON-RPC transport / `lsp` 工具 / server 路由 / 文档同步)时,读本篇。

## 本 crate 是什么

LSP(Language Server Protocol)客户端集成:经 JSON-RPC over stdio 与外部语言
服务器子进程通信,把 language capability 统一暴露为**单一** `lsp` 工具给 LLM。
transport 仅 stdio;方法集从只读 MVP(definition / references / hover)起步,
现有 6 个只读 action(再加 documentSymbol / completion / signatureHelp),另有
F15 符号级主动查询扩展与诊断缓存。

## 核心文件导航

| 路径 | 一句话职责 |
|---|---|
| `src/lib.rs` | crate 门面:公开类型 re-export(`LspConnectionManager` / `LspTool` / `LspAction` 等) |
| `src/config.rs` | LSP server 强类型配置;`file_patterns` 在本层就预编译成 `globset::GlobSet` |
| `src/error.rs` | `LspError`:仅用于生命周期操作,execute 路径映射回 `ToolError` |
| `src/manager.rs` | `LspConnectionManager`:start / reload / shutdown(镜像 `zhuye-mcp` manager,去 HTTP 路径) |
| `src/tool.rs` | `LspTool`:唯一对 LLM 暴露的工具,`action` 选 method、`file_path` 定路由 |
| `src/transport.rs` | stdio 上的 JSON-RPC peer + `framing` 分帧层(纯函数、无 IO、增量解析) |
| `src/protocol/methods.rs` | `LspAction` 枚举与各 action 的强类型 `invoke_*` 包装(超时 + cancel 集中于此) |
| `src/protocol/active.rs` | F15 符号级主动查询(find_referencing_symbols / rename / call_hierarchy 等)与 DAP 形状 |
| `src/protocol/diagnostics.rs` | `DiagnosticRegistry`:缓存 `publishDiagnostics` 通知,供被动读取 |
| `src/session/client.rs` | `LspClientInner`:单 server 运行时状态(peer + docs map + cancel token) |
| `src/session/document.rs` | 文档懒同步:首次调用前 `didOpen`,外部修改后按 mtime 走 `didChange` |
| `src/session/matching.rs` | 文件路径 → server 路由(显式 `server` 参数 > glob 匹配 > None) |
| `src/bin/mock_lsp_server.rs` | 极简 LSP stdio mock,仅本 crate 测试用 |

## 本地契约与同步点

- **工具名强制单数 `lsp`,不做 `lsp__<server>__<method>` 前缀**(与 MCP 的约定
  刻意不同,见 `lib.rs`「关键约束」):LLM 不该选 server,`file_path` 决定路由
  才符合 LSP 直觉;破坏了会让模型误以为要自行挑 server。
- **JSON-RPC framing 严格按 LSP spec**(`transport.rs`):`Content-Length: N\r\n\r\n`
  + 紧跟 N 字节 body;漏一字节 server 直接断连。`framing` 单元测试必须覆盖
  `NeedMore` 路径,改分帧层先跑测试。
- **文档走懒同步**(`session/document.rs`):工作区可能有几万文件,不主动
  `didOpen` 全部;文件被 Write/Edit 工具外部修改后按 didOpen 时记录的 mtime
  检测并走 `didChange`(version+1 全量同步)—— 此前 fast path 只查
  `contains_key` 导致 server 永远看到旧文本,是已修复缺陷,别改回去。
- **glob 预编译只在 config 层**(`config.rs`):`CompiledFilePattern.glob` 一次编译,
  `matching::pick_server_for` 直接 `is_match()`,不得在路由路径上反复重编译。
- **env 隔离与 MCP 同口径**:`env_clear()` 只放行 `HOME` / `PATH` + 用户显式 env。
- **错误分类**(见 `error.rs` doc):配置 / 启动阶段错误在 bootstrap 路径 warn +
  跳过整 server;单次 method 调用错误转 `ToolError::*` 返回给模型。

## 禁止事项

1. 不要新增第二个 LSP 工具或改成多工具暴露 —— 单一 `lsp` 工具是既定契约。
2. 不要给 transport 加 HTTP 路径(LSP server 几乎都走 stdio,Phase A 决策)。
3. 不要把 `lsp_types` 的类型直接泄漏进公开 API 之外的工具 schema。

## 深入阅读

- [docs/architecture.md](../../../docs/architecture.md) —— §5 节点上下文与能力装配
- [crates/integrations/AGENTS.md](../AGENTS.md) —— 本层通用规则(env 隔离 / 工具命名)
