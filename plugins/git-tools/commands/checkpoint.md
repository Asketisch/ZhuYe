---
description: 给当前工作区打 checkpoint 快照(可用 zhuye 的 rewind 回退)。参数:快照标签(可选)
---

用 `git_checkpoint_create` MCP 工具给工作区打快照。label 用 $ARGUMENTS
(缺省 auto)。返回 no-changes 时告知用户工作区本就干净;成功时把
checkpoint 的 sha / 标签报给用户,并提示「可用 rewind 回退到该快照」。
