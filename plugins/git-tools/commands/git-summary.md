---
description: 探测当前 git 仓库状态(分支 / diff 统计 / 打开的 PR)并汇总
---

用 `git_branch_summary` MCP 工具(工作目录 $ARGUMENTS,缺省当前目录)
探测仓库,向用户汇总:当前分支、相对默认分支的已提交增删行、当前分支
打开的 PR。探测缺失(无 gh / 未认证 / 非 fork)如实说明,不要猜。
