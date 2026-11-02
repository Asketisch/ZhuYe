---
description: 导入外部 Harness(Claude Code / Codex / ZCode)历史会话
---

请帮我把外部 Harness 的历史会话导入为烛夜会话。参数:`$ARGUMENTS`

执行步骤:

1. 若参数给出了具体路径,直接进入第 2 步;否则先调用
   `list_importable` 工具(source_kind 依次尝试 claude-code、codex、
   zcode)列出本机可导入的会话来源,把结果汇总给用户确认导入范围。
2. 调用 `import_session` 工具执行导入:`path` 传文件或目录路径,
   `source_kind` 传 `auto`(自动嗅探);用户要求预览时把 `dry_run`
   置为 true。
3. 把导入结果逐条汇报:每条包含来源 id、导入后的 thread_id、
   turn 数与警告数;失败的条目原样列出并说明原因。
4. 最后提示:导入产物是普通烛夜会话,可用
   `zhuye exec --resume <thread_id>` 直接续跑,`zhuye session ls`
   查看列表。
