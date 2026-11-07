---
description: 一键导入其他本地 agent(Claude Code / Codex / Gemini / opencode / crush)的模型与 MCP 配置
---

请帮我把其他本地 agent 的配置导入烛夜。参数:`$ARGUMENTS`(可省略;省略 =
全部来源一键导入)

执行步骤:

1. 调用 `import_config` 工具:`source_kind` 默认 `auto`(扫描全部已知
   来源);用户在参数里点名了某个 agent(claude-code / codex / gemini /
   opencode / crush)就只传那一个。用户只想预览时把 `dry_run` 置为 true。
2. 把报告逐条汇报:新增的 provider 凭证(label / base_url / 模型)、
   新增的 MCP server 名单、因已存在而跳过的条目、以及警告(如 gemini
   的模型暂无对应 provider、坏文件被跳过等)。
3. 导入是**增量合并**:不覆盖用户已有的 provider / MCP 配置,重复执行
   幂等。提醒用户:导入的凭证在 `~/.zhuye/config.toml` 的
   `[[anthropic.credentials]]` / `[[openai.credentials]]`,可用
   `zhuye config set active.provider <名字>` 切换激活 provider。
