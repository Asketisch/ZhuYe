# commands —— CLI 工具类子命令目录

> **何时读我**：新增 / 修改 `zhuye` 的工具类子命令实现，或排查某个子命令行为时，读本篇；参数面权威与全 crate 契约在 crate 篇。

## 目录是什么

`zhuye` 二进制的 16 个工具类子命令实现所在，一文件对应一个子命令；`mod.rs` 只放模块登记，分发逻辑在 `src/main.rs`。

## 子命令速览

| 文件 | 子命令 | 职责 |
|---|---|---|
| `config.rs` | `config` | 查看 / 修改配置项 |
| `doctor.rs` | `doctor` | 环境体检与配置导出 |
| `hooks.rs` | `hooks` | 内置 hook 的启用与停用 |
| `init.rs` | `init` | 初始化工作区 |
| `login.rs` | `login` | 账号登录与凭据管理 |
| `lsp.rs` | `lsp` | LSP server 列表与状态 |
| `mcp.rs` | `mcp` | MCP server 的增删查与连通性测试 |
| `models.rs` | `models` | 模型列表与交互选择 |
| `pipeline.rs` | `pipeline` | DAG 流水线运行 |
| `secrets.rs` | `secrets` | 加密密钥的存取 |
| `security.rs` | `security` | 安全策略配置 |
| `session.rs` | `session` | 会话列表 / 查看 / 分叉 / 导出 / 搜索 |
| `task.rs` | `task` | 任务列表 / 查看 / 创建 |
| `traces.rs` | `traces` | 链路追踪记录 |
| `update.rs` | `update` | 自更新检查 |
| `workspace.rs` | `workspace` | 工作区探测与信息 |

## 本地契约

- **一文件一子命令**：实现文件名 = 子命令名，`pub(crate)` 可见性，只被 main.rs 分发，不向外暴露库接口。
- **分发逻辑在 `src/main.rs`**：`enum Command` 变体、clap 参数与 dispatch match 都在那里；本目录只放实现函数，子命令之间不互相调用。
- **新增子命令必须两处登记**：① 本目录新增文件并在 `mod.rs` 声明；② `src/main.rs` 加 `enum Command` 变体与 dispatch 分支。之后按 crate 篇同步 `docs/cli.md` 与 `tests/functional/` 用例。

## 边界

- `exec` / `serve` 不在本目录 —— 参数结构与业务逻辑下沉在 `zhuye-exec`，本 crate 只投影转发。
- `plugin` 族（含 marketplace、`skill` 安装）在 `src/plugin/`，不迁入本目录。

## 深入阅读

- [crate 篇](../../AGENTS.md) —— 子命令同步点与禁止事项
- [docs/cli.md](../../../../../docs/cli.md) —— `zhuye` 二进制子命令全量一览
