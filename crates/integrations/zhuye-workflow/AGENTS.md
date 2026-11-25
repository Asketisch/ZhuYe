# 工作流核心

本 crate 是工作流插件的串行调度内核,不替代现有 DAG pipeline。节点通过 `Node` 接收执行快照并返回 `NodeResult`;主流程及 finally 报告分开。

烛夜 Agent 后端与 MCP 服务装配在 `crates/runtime/zhuye-workflow-server`,插件位于 `plugins/workflow`。运行状态仅存在于服务进程中。更改状态机或取消语义时,必须覆盖取消与结果竞争、暂停、阻塞恢复、回退上限和 finally 行为。

Agent 结果必须是显式节点结果 JSON;可剥离完整 JSON 代码围栏,不得从自由文本推断成功。
