# builtins —— 内置工具实现目录

> **何时读我**：新增 / 修改某个内置工具的实现，或调整工具分组目录时，读本篇；工具清单计数与门控的契约在 crate 篇，本篇不重复。

## 目录是什么

`Tool` trait 的全部内置实现所在，按能力域分六簇（子目录）；`mod.rs` 用一批 `pub use` 把全部工具类型平铺到 `zhuye_tools::builtins` 命名空间。

## 对外路径契约

- **crate 外一律走 `zhuye_tools::builtins::<Type>` 的平铺路径**（如 `builtins::ReadTool` / `builtins::BashTool`）—— `mod.rs` 下方的 `pub use` 是唯一入口，`builtins::fs::…` 这类分组深路径不对外承诺。
- **改分组只动目录结构，不动这批 re-export** —— 平铺名是注册入口与外部调用方的既定接口，改路径 / 改名都属破坏性变更。
- `web/url_safety.rs` 是 `web_fetch` 内部的 SSRF 防护 helper，不是工具，不在平铺 re-export 之列。

## 六簇速览

| 簇 | 能力域 | 工具（平铺名） |
|---|---|---|
| `fs/` | 文件与目录读写 | read / write / edit / delete / glob / grep / notebook_edit / image_view |
| `web/` | 网络取内容 | web_fetch / web_search |
| `human/` | 人机交互 | ask_user / ask_user_question / request_human_input / brief |
| `plan/` | Plan mode | enter_plan_mode / exit_plan_mode / plan_write |
| `exec/` | 命令执行与 worktree 切换 | bash / echo / enter_worktree / exit_worktree |
| `meta/` | 引擎自省与调度基建 | tool_search / deferred_call / context_remaining / background_status |

## 本地契约与同步点

- 工具描述文案从 prompt 资源加载（`mod.rs` 的测试断言各描述非空），新增工具必须带描述资源。
- 新增 / 移除工具走 crate 篇「清单三处同步」：`builtin_tools.rs` 的计数常量与注册断言 + `docs/tools-and-hooks.md` §2 权威清单。
- 门控工具（web_search / notebook_edit）的注册行在 `register_builtin_tools_with_gates` 内由 `BundledToolGates` 包裹，本目录不自行判门控。
- 工具执行记录严格模式仅放过明确声明无副作用的工具;read / glob / grep 声明只读,并发安全不能替代只读声明。
- grep / glob 在阻塞线程遍历文件,取消或执行 future 被丢弃时通知遍历退出,不得在异步运行时线程直接扫描整个目录。
- grep 两种模式均逐行扫描,单行超过 32 MiB 时显式报错;BM25 用两遍扫描统计和评分,不能把整份大文件或全部候选行加载进内存。

## 禁止事项

1. 不要在 crate 外引用 `builtins::<簇>::<Type>` 分组深路径 —— 只承诺平铺名。
2. 不要为绕开平铺 re-export 而在别处再造一份工具类型导出。
3. 不要在本目录内读配置或插件开关决定工具是否注册 —— 门控统一在 `builtin_tools.rs`。

## 深入阅读

- [crate 篇](../../AGENTS.md) —— 清单三处同步、门控与审批契约
- [docs/tools-and-hooks.md](../../../../../docs/tools-and-hooks.md) —— 内置工具契约与输出上限表
