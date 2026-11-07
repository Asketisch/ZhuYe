---
description: 进入目标模式:设定一个可校验的目标,引擎每轮结束后自校验、未达成自动续作。参数:目标描述(可选 verify 命令)
---

你在操作烛夜的目标(goal)模式。目标 = 一个**可校验的终态**,引擎在
每轮结束后用独立 LLM 调用自校验(可选 shell 命令作 AND 条件),未达成
时把「剩余工作」作为续作 prompt 推进,直到达成 / 连续阻塞 / token 预算
耗尽。

用户指令:

$ARGUMENTS

规则:

1. 把用户意图整理成一句**可判定**的目标(避免「尽力做好」这类不可校验
   的表述);如用户给了验收命令,原样传给 `verify_command`。
2. 调用 `goal` 工具:`{"action": "enter", "goal": "...", "verify_command": "...", "token_budget": N}`。
   (verify_command / token_budget 可省略;预算默认 500k token。)
3. 若返回提到 `GOAL_NO_CLIENT`,说明没有可用 LLM client 做校验,如实
   告知用户检查 provider 配置,不要假装目标模式已开启。
4. 之后正常执行任务;每轮结束引擎会自校验。用 `{"action": "status"}`
   查询进度(状态 / 累计 token / 轮数),达成或放弃时用
   `{"action": "exit"}` 退出。
