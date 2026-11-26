//! CLI 子命令实现 —— 一件文件对应一个 `zhuye` 子命令。
//!
//! 分发逻辑在 [`crate::main`]；本模块只放各子命令的实现。
//!
//! | 模块 | 子命令 | 职责 |
//! |---|---|---|
//! | [`config`] | `config` | 查看 / 修改配置项 |
//! | [`doctor`] | `doctor` | 环境体检与配置导出 |
//! | [`hooks`] | `hooks` | 内置 hook 的启用与停用 |
//! | [`init`] | `init` | 初始化工作区 |
//! | [`login`] | `login` | 账号登录与凭据管理 |
//! | [`lsp`] | `lsp` | LSP server 列表与状态 |
//! | [`mcp`] | `mcp` | MCP server 的增删查与连通性测试 |
//! | [`models`] | `models` | 模型列表与交互选择 |
//! | [`pipeline`] | `pipeline` | DAG 流水线运行 |
//! | [`secrets`] | `secrets` | 加密密钥的存取 |
//! | [`security`] | `security` | 安全策略配置 |
//! | [`session`] | `session` | 会话列表 / 查看 / 分叉 / 导出 / 搜索 |
//! | [`task`] | `task` | 任务列表 / 查看 / 创建 |
//! | [`traces`] | `traces` | 链路追踪记录 |
//! | [`update`] | `update` | 自更新检查 |
//! | [`workspace`] | `workspace` | 工作区探测与信息 |

pub(crate) mod config;
pub(crate) mod doctor;
pub(crate) mod hooks;
pub(crate) mod init;
pub(crate) mod login;
pub(crate) mod lsp;
pub(crate) mod mcp;
pub(crate) mod models;
pub(crate) mod pipeline;
pub(crate) mod secrets;
pub(crate) mod security;
pub(crate) mod session;
pub(crate) mod task;
pub(crate) mod traces;
pub(crate) mod update;
pub(crate) mod workspace;
