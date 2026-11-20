---
description: 用 LSP 做代码导航:定义/引用/诊断。参数:目标符号与操作意图
---

用户想用语言服务器(LSP)做代码导航。调用 `lsp` 工具(single tool,
`action` 分派):

用户指令:

$ARGUMENTS

规则:

1. 需要会话已配置 `[lsp_servers]`(zhuye lsp add 可代写)且对应
   server 已启动(`LspServerStarted` 事件可查);未启动时如实告知,
   不要降级假装成功。
2. 常用 action:definition / references / diagnostics / hover ——
   参数带文件路径与光标位置(行/列,0 基)。
3. LSP 结果是结构化的(位置列表 / 诊断区间);引用时给出文件与行号,
   结合 read 工具核对上下文后再下结论。
4. 诊断(diagnostics)带 severity;error 级优先处理,不要被 warning
   噪声带偏。
