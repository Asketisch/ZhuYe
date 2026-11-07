---
description: 队列模式:把一批任务排成 FIFO 队列串行执行(前一条 turn 结束后自动跑下一条)。参数:任务列表或操作指令
---

你在操作烛夜的队列(queue)模式:一个 session 级 FIFO 任务队列,driver
**串行**消费 —— 每次出队一条任务作为新 turn 注入,等它的 turn 结束
(TurnComplete)再把下一条出队。任务跨 turn / 跨重启持久
(~/.zhuye/queue/<session>.json,中断的任务重启后自动重新排队)。

用户指令:

$ARGUMENTS

规则:

1. 把用户的批量需求拆成**独立可执行**的任务(每条是一个完整 prompt,
   出队后就是一轮新对话的输入;注意它们不共享本轮上下文,任务里要自带
   必要背景)。
2. 逐条入队:`{"action": "add", "prompt": "...", "name": "可选任务名"}`。
3. 开跑:`{"action": "start"}`;暂停(保留任务)用 `pause`;查看进度用
   `{"action": "status"}` / `{"action": "list"}`(含 done/failed 计数)。
4. 删除单条用 `remove`(执行中的任务删不掉);清空未执行的用 `clear`。
5. 汇报时给出:总数 / 已完成 / 失败 / 排队中,以及失败任务的 note。
