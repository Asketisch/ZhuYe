# 工作流插件

串行执行任务节点、WASM 任务节点和烛夜 Agent 节点。工作流在插件进程内运行,与现有 DAG pipeline 分开;暂停、阻塞和执行记录只在进程存活期间保留。

## 安装

```bash
./scripts/build_workflow_plugin.sh
zhuye plugin install plugins/workflow --scope user
zhuye plugin enable workflow@inline
```

工具以 `mcp__plugin:workflow@inline:workflow__` 为前缀注册,走现有 MCP 工具权限与审批。`workflow_start` 接收 `config_path` 指向 TOML 文件,或者接收 `definition` JSON 对象,两者必须且只能提供一个;返回 `run_id`;使用 `workflow_status` 查询快照,以及 `workflow_pause`、`workflow_resume`、`workflow_cancel` 控制运行。`workflow_forget` 只移除已结束的运行。服务至多保留 128 个运行,可清理终态运行后继续启动。

## TOML 配置

可直接使用 `tests/live/workflow_e2e.example.toml` 作为模板,修改 `node_types` 的类型参数、模型、指令和上下文选择,以及 `nodes` / `finally_nodes` 的实例、配置、次数和超时。通过 MCP 调用启动:

```json
{"config_path":"/工作区/workflow.toml"}
```

配置文件中的 WASM `path` 和 Agent `workspace` 若为相对路径,均相对于 TOML 文件所在目录;`config_path` 自身为相对路径时相对于服务进程工作目录。节点结果中的空值在 TOML 中没有字面量,可根据字段契约省略可选项;必须提供的 `output` 可使用空表 `{}`。TOML 与 JSON 使用相同字段和校验规则。

实际模型的 `model` 可使用已配置模型组的 `组名/模型名`;服务会加载该模型组的端点配置。可参考 `tests/live/README.md` 运行包含多个任务和 Agent 的实模型演练。

## 定义示例

```json
{
  "variables": {"message": "你好"},
  "node_types": {
    "seed": {"kind": "task"},
    "echo": {"kind": "wasm", "path": "/绝对路径/zhuye-workflow-echo.wasm"},
    "review": {
      "kind": "agent", "model": "mock", "workspace": "/工作区绝对路径",
      "instructions": "检查前面的结果并返回工作流节点结果 JSON",
      "context_keys": ["message", "echoed"], "output_keys": ["echo"]
    }
  },
  "nodes": [
    {"id": "prepare", "node_type": "seed", "config": {"status": "success", "output": {"ready": true}}},
    {"id": "echo", "node_type": "echo"},
    {"id": "review", "node_type": "review", "timeout_ms": 120000}
  ],
  "finally_nodes": [
    {"id": "cleanup", "node_type": "seed", "config": {"status": "success", "output": {"cleaned": true}}}
  ]
}
```

节点实例 ID 在主队列和 finally 队列中唯一。`enabled` 默认 true,`max_runs` 默认 1,`timeout_ms` 默认 30000;`skip_if` 可写为 `{"variable":"key","equals":值}`。回退前应为所有可能重跑的实例设置足够的 `max_runs`;每次工作流最多执行 10000 次节点尝试。

`task` 是内置纯数据节点:其 `config` 必须符合下面的节点结果格式。Rust 宿主也可直接实现 `zhuye_workflow::Node`。WASM 节点使用 `zhuye:plugin/zhuye-workflow-node@0.1.0` world;示例源码和组件位于 `plugins/plugin-demo/wasm/rust/zhuye-workflow-echo/` 与 `plugins/plugin-demo/bin/zhuye-workflow-echo.wasm`。组件默认没有文件系统、网络或环境变量,单次调用由宿主限制时长。可用语言取决于对应组件工具链;仓库验证的是 Rust 样例。

Agent 节点只接收 `context_keys` 列出的共享变量和 `output_keys` 列出的节点输出,不自动接收全部历史。`model` 按门面支持的模型和环境变量配置;生产使用需配置相应 provider。示例中的 `mock` 仅供离线测试,需提供符合节点结果格式的 mock 回复。

## 节点结果与流转

每个节点返回 JSON 对象,至少包含 `status` 和 `output`:

```json
{"status":"success", "output":{"result":"完成"}, "updates":{"key":"value"}, "action":{"type":"continue"}}
```

`status` 为 `success`、`failed`、`skipped`、`blocked` 或 `cancelled`。`updates` 显式更新共享变量;`action` 可为 `continue`、`{"type":"back","target":"节点ID"}` 或 `{"type":"wait","reason":"原因"}`。失败停止主队列;阻塞保留原因、结果与上下文,等待显式继续或取消。节点可通过 `insertions` 请求插入预先注册的类型,每项含 `id`、`node_type`、`after` 和可选 `config`;服务先校验全部请求,再应用插入和变量更新。

暂停在节点边界生效;取消会取消正在执行的 Agent,或停止等待 WASM 结果。WASM guest 即便被丢弃也受组件 deadline 限制。主队列结束、失败或取消后依次执行 finally 节点;清理失败单独记入 `finally_history`,不会阻止后续清理节点。`workflow_status` 包含节点尝试 ID、输入变量键、结果、错误和可用的 Agent 运行 ID。

## Agent 审批

Agent 使用烛夜权限规则和工具执行机制。工具或 hook 需要审批时,`workflow_status` 的 `approvals` 返回请求 ID、对象和参数;经用户授权后,调用 `workflow_approval` 并传入 `run_id`、`request_id`、`approve` 布尔值和可选拒绝 `reason`。审批只处理一次;节点取消或超时后,等待中的审批失效。审批等待计入节点超时。当前不支持 Agent 的自由文本提问和计划审批,遇到这类交互时节点失败并报告原因。
