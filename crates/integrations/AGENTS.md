**何时读我:** 当你要改动 MCP 客户端、LSP 客户端、流式会话后端(定时 / 自动驾驶 / 队列)、Computer Use、浏览器控制、常驻 shell 会话池、git 工作流 MCP server、本地知识库 wiki 或安全扫描/通知时读本篇 —— 本层契约以本篇及各 crate 篇为准。

# crates/integrations/ —— 外部系统接入层

## 层定位

integrations 是六层分层中的**外部系统接入层**:把本机子进程、远端 HTTP 端点与外部会话文件包装成框架内可用的能力(工具、调度器、导入器)。依赖方向自上而下 —— 本层向下依赖 `protocol` / `abilities` / `resources`,由上方 `runtime` 层消费;`orchestration` 与本层互不依赖。分层与依赖方向的全貌见 [docs/architecture.md](../../docs/architecture.md)。

## crate 职责速览表

本层各 crate 以下表定位,细则见后续小节。

| crate | 一句话职责 | 本地契约在哪 |
|---|---|---|
| `zhuye-mcp` | MCP 客户端:把外部 MCP server 的 tool / resource / prompt 接入工具注册表 | MCP 客户端 |
| `zhuye-lsp` | LSP 客户端:以单一 `lsp` 工具承载 definition / references / hover | LSP 客户端 |
| `zhuye-integration` | 安全扫描(cargo audit 包装)与 Webhook 通知 | 外部集成 |
| `zhuye-stream` | 流式会话后端:cron 定时 / autopilot 自动驾驶 / FIFO 队列 / 工作区同步 | 流式后端 |
| `zhuye-computer-use` | Computer Use:截屏与键鼠输入,含 a11y 寻址与动作后验 | Computer Use |
| `zhuye-browser` | 浏览器控制:CDP 驱动本机浏览器,复用 a11y 寻址与快照绑定 | 浏览器控制 |
| `zhuye-shell` | 常驻 shell 会话池:启动进程 / 增量读 / 写 stdin / kill | 常驻 shell |
| `zhuye-git-tools` | git 工作流 MCP server 协议层:分支摘要 / checkpoint 快照 | git 工作流 |
| `zhuye-workflow` | 工作流插件串行队列及任务、WASM、Agent 节点抽象 | [工作流插件核心](zhuye-workflow/AGENTS.md) |
| `zhuye-kms` | 本地知识库 wiki(grep 检索 + dream 固化),`plugins/kms` 的能力本体 | KMS 知识库 |

## 本层本地契约

> 本篇正文里的文件路径一律相对仓库根目录。

### MCP 客户端(`zhuye-mcp`)

- transport 三态:`McpTransport::Stdio`(默认)/ `Http`(别名 `streamable-http`)/ `Sse`(legacy 兼容别名,实现上委托 streamable-http 客户端)。落点:`crates/integrations/zhuye-mcp/src/config.rs`、`crates/integrations/zhuye-mcp/src/transport.rs`。
- 协议版本由常量 `PROTOCOL_VERSION` 固定(`crates/integrations/zhuye-mcp/src/protocol_version.rs`),判断 wire 兼容性先看它。
- 工具全名强制 `mcp__<server>__<tool>` 前缀(`crates/integrations/zhuye-mcp/src/tool.rs`),不得与内置工具重名。
- 配置走 `[mcp_servers.<name>]` TOML 表。
- `timeout_ms` 同时约束连接握手、首次 `tools/list` 和单次工具调用；初始化超时上报 `McpServerFailed` 并清理连接。
- stdio 子进程 env 用 `env_clear()` 隔离,只注入 `HOME` / `PATH` + 用户显式 env(`crates/integrations/zhuye-mcp/src/transport.rs` 的 `inject_safe_env`)。
- MCP 连接主动关闭或对端断开时必须取消共享 token 并结束后台任务；握手后工具列表获取失败也必须关闭连接，不能遗留子进程。
- MCP 资源读取按服务端顺序保留文本、二进制及来源信息；二进制以 base64 JSON 文本传递，嵌入资源和链接也不得丢失；资源列表与读取响应宿主取消并向服务端发送取消通知。
- 自暴露 MCP server(`crates/integrations/zhuye-mcp/src/server_mode.rs`)默认只读、随机 token 鉴权、仅回环监听、工具白名单。
- **客户端侧 OAuth(http 传输)**:`[mcp_servers.<name>.oauth]` 支持 `token` / `token_env` / `client_credentials` / `refresh_token`;token 端点缺省时按受保护资源元数据(RFC 9728)与授权服务器元数据(RFC 8414)发现。服务端要求授权而无可用凭据时,启动失败并上报 `McpError::AuthRequired`(含可执行提示),**不**含糊报「握手失败」。授权码流程(浏览器)由宿主完成。
- **工具列表动态刷新**:服务端发 `notifications/tools/list_changed` → 重新 `tools/list` → 新工具注册、下线工具反注册(注册表是 LLM 可见面的唯一真相)。刷新与启动注册的先后不可预知,故以**版本化快照 + 注册表现状求差集**收口。
- **Elicitation(server→client `elicitation/create`)**:`HostClientHandler` 把请求转成 `McpLifecycleEvent::Elicitation`,由宿主经 `Op::McpElicitationResponse` 裁决;`form` 形态按服务端 schema 逐字段还原,字段名不得改写;`link` 形态只带 URL。客户端**不声明 sampling 能力**(未实现),服务端不得依赖它。
- 未实现能力(Sampling / OAuth)登记在 `crates/integrations/zhuye-mcp/src/lib.rs` 头部清单,新增实现要同步改那处清单。

### LSP 客户端(`zhuye-lsp`)

- 只暴露**一个**工具 `lsp`(`crates/integrations/zhuye-lsp/src/tool.rs`):`action` 决定 method,`file_path` 决定路由到哪个 server。
- transport 仅 stdio;方法集为只读 MVP(`textDocument/definition` / `references` / `hover`)。
- JSON-RPC framing 严格按 `Content-Length: N\r\n\r\n` + N 字节 body(`crates/integrations/zhuye-lsp/src/transport.rs`),漏字节会直接断连。
- 配置走 `[lsp_servers.<name>]` TOML 表;env 隔离与 MCP 同口径。

### 外部集成(`zhuye-integration`)

- 模块:`notifications`(Webhook 通知)、`security_scan`(cargo audit 包装);消费方是 `zhuye-cli`(`security` 子命令 / `doctor` 探测)。
- 历史上的 stub 模块(F45 gateway / F36 issues / F41 voice / F66-68 ide_and_local / F60 security_scan_v2)零调用方已删除;需要时按「形态判断」以插件形态重新承载,不要回填 stub。
- 本 crate 不依赖任何 `zhuye-*` 内部 crate,只吃 serde / tokio;新增子能力按模块切开,不要堆进 `lib.rs`。

### 流式后端(`zhuye-stream`)

- 模块:`cron`、`autopilot`、`queue`、`workspace_sync`、`postgres_session`、`sse_redis`。
- `queue` 是 session 级 FIFO 任务队列(`QueueScheduler`),以 `plugins/goal-queue` 形态交付;`cron` / `autopilot` 的类型由 `zhuye` 门面再导出(`crates/runtime/zhuye/src/lib.rs`)。
- 依赖 `zhuye-protocol` / `zhuye-config` / `zhuye-fs`;持久化路径沿用 `zhuye-config` 的运行时目录约定。

### Computer Use(`zhuye-computer-use`)

- 工具面:`computer_screenshot` / `computer_click` / `computer_type` / `computer_key` / `computer_scroll`。
- 平台重依赖(xcap 截屏 + enigo 输入)走 `real` feature 且默认关闭;关闭时统一返回 `MediaError::Unavailable`,**不 panic**(`crates/integrations/zhuye-computer-use/src/backend.rs`、`crates/integrations/zhuye-computer-use/src/types.rs`)。
- 权限下限为 `Prompt`(宿主审批),不得降级为静默放行。
- `crates/integrations/zhuye-computer-use/src/a11y.rs` 负责 a11y 树寻址、快照绑定与动作后验;改动作语义要同步改这里的绑定与校验。

### 浏览器控制(`zhuye-browser`)

- 工具面:`browser_navigate` / `browser_snapshot` / `browser_click` / `browser_type` / `browser_press_key` / `browser_scroll` / `browser_screenshot` / `browser_close`(`crates/integrations/zhuye-browser/src/bin/zhuye-browser-server.rs` 的 `tools_json()` 统一产出)。
- 控制通道是 **CDP over loopback**(仅 127.0.0.1/localhost/::1 + 单 target 强制校验,复用 `zhuye-computer-use::BrowserSecurity`);被控端是本机 Chrome/Edge/Chromium,隔离 profile(`zhuye_config::paths` 的 `browser-profile`),`DevToolsActivePort` 文件是桌面 browser_backend 共享的发现契约。
- ws/网络依赖走 `real` feature 且默认关闭;关闭时统一返回 `BrowserError::Unavailable`,**不 panic**(`crates/integrations/zhuye-browser/src/backend.rs`)。
- 快照引用绑定不得回退:`@eN` 只在取到它的快照版本内有效(`check_binding`),导航/重拍快照即失效 —— 这是 F39 的防误点核心语义。
- 权限下限为 `Prompt`(宿主审批),不得降级为静默放行。

### 常驻 shell(`zhuye-shell`)

- 与内置 `bash` 工具的分工:bash 是一次性命令,本 crate 是可交互长驻会话(启动 → 增量读 → 写 stdin → kill)。
- 三条正确性细节不得回退:逐行 `read_until` + `from_utf8_lossy`(非 UTF-8 不能让 reader 提前退出而堵死管道);锁不跨 await(等待退出用 `try_wait` 轮询);输出序号在拿锁后分配,保证 stdout / stderr / exit 序号与写入顺序一致。
- PTY(resize / raw mode)明确不做,不要顺手加。

### git 工作流(`zhuye-git-tools`)

- 派发逻辑放 lib(可单测);`crates/integrations/zhuye-git-tools/src/bin/zhuye-git-tools-server.rs` 只做 stdio I/O,`crates/integrations/zhuye-git-tools/src/bin/zhuye-cp-hook.rs` 是 Stop hook 入口(永远退出 0,失败只写 stderr,不拦截 turn)。
- 工具清单与 schema 由 `tools_json()` 统一产出;新增工具只改这一处。
- 协议版本同样用常量 `PROTOCOL_VERSION` 声明。

### KMS 知识库(`zhuye-kms`)

- 本地 Markdown 知识库 wiki:grep 检索(`search`)+ dream 固化(`dream`);数据落 `~/.zhuye/kms/`。
- 交付形态是独立二进制 `zhuye-kms-server`(MCP stdio),由 `plugins/kms` 挂载;不进 SDK facade(与其他 plugins/* 能力本体同款)。
- 自 abilities 层归位而来(2026-11):crate 定位与 computer-use / shell / git-tools / import 一致 —— integrations 层的能力本体 crate。

## 层内依赖约定

- 本层 crate 原则上**互不依赖**:要组合能力由上层(runtime)接线,不要在层内互相 import。已知例外(复用安全基线,不引入业务耦合):`zhuye-browser` → `zhuye-computer-use`(复用 `BrowserSecurity` 回环校验)、`zhuye-git-tools` → `zhuye-git`(resources 层能力族)。
- 统一向下依赖:`zhuye-protocol`(协议类型)、`zhuye-config`(配置与运行目录)、`zhuye-tools`(工具注册表),以及按需的 `zhuye-fs` / `zhuye-git` / `zhuye-rollout`。
- 依赖只允许从上往下流;本层不得依赖 `runtime` 层的任何 crate(含 `zhuye` 门面与 `zhuye-cli`)。
- `orchestration` 与本层互不依赖,只共同依赖 `abilities` / `resources` / `protocol`。

## 跨层边界

- wire 变体只在 `crates/protocol/zhuye-protocol` 定义;本层不得自定义协议变体,需要新能力先在协议层加变体。
- 对外暴露的能力走 `Tool` trait 并注册进工具注册表,不要绕过注册表直接调用。
- `zhuye-computer-use` / `zhuye-shell` / `zhuye-git-tools` / `zhuye-kms` 是 `plugins/*` 的能力本体:按插件形态挂载,不进 SDK facade。
- `zhuye-stream` 的 `cron` / `autopilot` / `queue` 属引擎内建能力(由 `zhuye` 门面再导出),与插件形态分开处理。

## 本层通用规则

1. 子进程 env 一律 `env_clear()` 隔离,只放行 `HOME` / `PATH` + 用户显式 env;Windows 另透传运行时候选变量。改任一 transport 的 env 逻辑时与 `zhuye-lsp::transport::inject_safe_env` 同口径。
2. 新增工具名必须遵守本层既定命名(见上各小节),避免与内置工具冲突。
3. 第三方库的错误类型不外泄到本层公开 API:在 crate 内收敛为带上下文的错误变体再上抛。
4. crate 头部注释里的「已实现 / 未实现」清单是范围声明,增删能力时同步维护,不要让清单与实现漂移。
5. 改动本层能力面时,按根 `AGENTS.md` 的开发流程检查表同步能力覆盖用例与覆盖矩阵。
6. **新增外部集成默认走插件形态** —— 接入新的外部系统时,先按根 `AGENTS.md` 检查表第 1 步做形态判断:默认路径是独立二进制或 MCP server + `plugins/*` 挂载(先例见「跨层边界」一节的 computer-use / shell / git-tools);只有判断确认内置(进 SDK facade / 全用户必需)才直接新增 integrations crate。

## 深入阅读

- [docs/architecture.md](../../docs/architecture.md) —— 六层分层、依赖方向与本层职责
- [docs/plugins.md](../../docs/plugins.md) —— 插件机制:本层能力本体如何挂载
- [docs/cli.md](../../docs/cli.md) —— CLI 子命令边界与独立二进制
- [crates/protocol/zhuye-protocol/src/op.rs](../../crates/protocol/zhuye-protocol/src/op.rs) —— wire 变体的唯一定义处

## 工作流插件核心

- `zhuye-workflow` 是插件侧串行节点调度核心,不替代 DAG pipeline。节点通过统一 trait 接收上下文快照并返回结果;控制句柄负责暂停与取消。
- 烛夜 Agent 后端与 MCP 控制入口位于 `zhuye-workflow-server`,由 `plugins/workflow` 安装。
