# runtime 层 AGENTS.md

> **何时读我**：在 `crates/runtime/` 下工作——改 `AgentThread` / StateGraph 引擎、headless 与 serve 的装配、CLI 子命令分发、库门面的 re-export 面、或 PyO3 绑定时，读本篇。

## 层定位

仓库最顶的入口层：把下层的协议、能力、资源、编排、集成装配成**可运行的入口**（库门面 / headless 二进制 / 常驻 serve / CLI）。完整系统说明见 `docs/architecture.md` §1「框架边界与入口」与 §4「一轮执行与状态转移」。

## crate 速览

| crate | 一句话职责 | 本地契约在哪 |
|-------|-----------|-----------|
| `zhuye-core` | `AgentThread` + 4 节点 StateGraph 引擎、回合级上下文与事件发射 | [zhuye-core/AGENTS.md](zhuye-core/AGENTS.md) |
| `zhuye-exec` | headless 单轮与 serve 常驻会话的共享装配层，产出 JSONL 事件流 | [zhuye-exec/AGENTS.md](zhuye-exec/AGENTS.md) |
| `zhuye-cli` | `zhuye` 二进制的 clap 参数面与子命令分发 | [zhuye-cli/AGENTS.md](zhuye-cli/AGENTS.md) |
| `zhuye` | 库门面：Builder 入口 + 对外扁平 re-export | 契约见本篇 |
| `zhuye-workflow-server` | 工作流插件运行时与 MCP 进程 | [zhuye-workflow-server/AGENTS.md](zhuye-workflow-server/AGENTS.md) |
| `zhuye-py` | PyO3 绑定，把 Rust 侧能力暴露给 Python | 契约见本篇 |

## 层内依赖约定

- 层内以 `zhuye-core` 为底：`zhuye-exec`、`zhuye`（门面）、`zhuye-py` 都建立在 `zhuye-core` 之上；`zhuye-cli` 建立在 `zhuye-exec` 之上。
- 禁止反向依赖：`zhuye-core` 不得引用 `zhuye-exec` / `zhuye-cli` / `zhuye` / `zhuye-py`；`zhuye-exec` 不得引用 `zhuye-cli`。
- `zhuye-cli` 是**薄壳**：子命令的业务逻辑下沉到 `zhuye-exec` 或其它层 crate，CLI 只做参数解析与转发。

## 跨层边界

- **headless 输出不变量**：事件以 JSONL 流写 stdout，tracing 日志走 stderr——`zhuye exec | jq` 不能被日志污染。破坏这条会打断所有下游管道消费。
- **参数结构归属**：exec / serve 的参数结构定义在 `zhuye-exec`（`ExecArgs` / `ServeArgs`），`zhuye-cli` 只是 clap 外壳投影，改参数先改 exec 再改 CLI。
- **门面 re-export 面**：`zhuye` 门面的 re-export 集合是 SDK 的公开面，增删条目时同步更新双 SDK 文档与类型层；用「门面上多了一项 / 少了一项」的定性口径描述变更，不要在文档里写死条目数量。
- **PyO3 绑定**：`zhuye-py` 的对外契约与 Rust 侧同名，协议形状变更需同步落到绑定层。
- **模型工具输出**：`zhuye-core` 对各来源结果统一限制模型侧文本并溢出文件；宿主事件保留工具原始结果，恢复历史沿用模型侧回执。
- **上下文压缩审计**：只截断但未删除消息的有效压缩同样发事件并登记审计；空操作不触发守卫。
- **Linux 后台命令**：后台 bash 必须与前台使用同一个 Landlock/seccomp 策略；启用沙箱时任何安装失败均不得重试为裸执行。
- **后台命令生命周期**：会话取消或后台执行任务被丢弃时，经工具层进程组封装清理 Unix 同组后代并回收直接子进程；不可声称清理主动脱组的进程。

## 本层通用规则

1. 库 crate 用 `thiserror` 表达错误，应用层（exec / cli）用 `anyhow`。
2. 事件发射只经 `zhuye-core` 的事件通道；不要在入口层直接往 stdout 写事件。
3. 新增 CLI 子命令时，参数面与文档总览表要同步——细则见 [zhuye-cli/AGENTS.md](zhuye-cli/AGENTS.md)。
4. 改本层后按根 `AGENTS.md` 的验证矩阵选跑；涉及事件形状或协议时跑 `./tests/run_all.sh functional`。
5. 注释与文档一律中文。

## 深入阅读

- [docs/architecture.md](../../docs/architecture.md) —— §1 框架边界与入口、§4 一轮执行与状态转移、§5 节点上下文与能力装配
- [docs/cli.md](../../docs/cli.md) —— `zhuye` 二进制子命令总览

- 工作流插件通过门面的 `with_permission_resolver` 注入与 headless 同序的配置、项目及用户权限规则;Agent 的工具与 hook 审批由插件服务桥接。

- 工作流服务支持内联 JSON 或 TOML 文件定义;相对节点路径以 TOML 文件所在目录为准,配置模型组通过门面注入模型注册表。
