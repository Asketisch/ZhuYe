---
description: 起一个常驻 shell 会话跑长任务(dev server / watch / REPL),用增量读观察输出。参数:要跑的命令
---

你在管理常驻 shell 会话(经 `shell_*` MCP 工具;进程 env 已清空,不含
宿主凭据)。

用户指令:

$ARGUMENTS

规则:

1. `shell_run` 启动(立即返回 session_id,不会阻塞),记下 id。
2. 观察输出用 `shell_read`(增量;返回里带 `--- still running ---` /
   `--- exited: N ---`),轮询间隔别太密(建议 ≥2s)。
3. 需要交互输入(REPL)用 `shell_write`(自行带 `\n`);让读 stdin 的
   程序自然退出用 `shell_close_stdin`。
4. 结束必须 `shell_kill`(不留孤儿进程);一次性命令不要走本工具,
   用内置 bash。
5. 非零退出时先读 stderr 判断原因,再决定重试或放弃。
