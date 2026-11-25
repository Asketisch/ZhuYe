# 工作流插件运行时

本 crate 将 `zhuye-workflow` 节点内核与烛夜 Agent、插件 MCP 进程装配。不得关闭 Agent 审批或将节点执行成功等同业务成功;取消节点时必须取消对应 Agent 会话。节点编排内核不反向依赖本 crate。

`workflow_start` 可接收内联 JSON 或 TOML 文件路径;配置内的相对 WASM 路径及 Agent 工作区以文件所在目录为准。模型组通过门面注入配置注册表,不得忽略自定义端点。
