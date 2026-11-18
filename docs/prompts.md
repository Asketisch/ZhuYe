# 提示词选择与维护

烛夜默认使用内置精简提示词。`[prompts]` 配置控制新会话的默认选择。

```toml
[prompts]
mode = "default" # default / auto / manual
# mode = "manual" 时设置 profile = "claude"
```

`default` 使用内置文案以及用户 `default.md` 和旧版 `prompts.md` 覆盖。
`auto` 按实际请求的模型匹配通过评测的方案，未命中时回落默认并报告原因。
`manual` 使用指定的内置方案或用户层文件，切换模型后继续使用同一方案。
当前内置方案尚无真实模型成对评测结果，因此 `auto` 会回落默认。

用户可在 `~/.zhuye/prompts/`、`<workspace>/.zhuye/prompts/` 下放置
`<方案名>.md`。路径中的用户目录遵循 `ZHUYE_HOME` 配置。文件以 `## key`
分节，未提供的 section 使用默认文案。同名工作区文件优先于用户文件，
自定义 agent 的系统提示词继续优先。旧版 `~/.zhuye/prompts.md` 仍可使用。

常用 section 包括 `system.main`、`system.facade-main`、`system.plan-mode`、
`runtime.active-mode`、`tool.<工具名>`、`compact.full`、`compact.recent`、
`compact.memory-extract`、`coordinator.system`、`team.lead`、`goal.verifier`、
`discussion.judge`、`discussion.participant` 和 `subagent.explorer`。
`system.plan-mode` 与 `runtime.active-mode` 可设为 `off`、`none`、`disabled`
或 `false` 来关闭对应段落。空 section 会被忽略。

`/prompt` 与 `/prompt list` 查询方案，`/prompt reload` 重新读取目录，
`/prompt default`、`/prompt auto` 和 `/prompt use <id>` 修改当前会话。
`/prompt follow` 恢复跟随共享默认。选择保存在会话 rollout 中，恢复时
重新应用。单次 headless 调用可传入 `zhuye exec --prompt-profile <id>`。

协议使用 `Op::PromptCommand`，`select` 设置会话覆盖，`save_default` 保存
共享默认并令当前会话跟随默认。`prompt_list` 和 `prompt_switched` 返回
选择策略、实际方案和回落原因。原 `switch` 动作继续用于重扫。

内置方案与上游来源版本、SHA-256 和评测状态记录在
[`sources.json`](../crates/abilities/zhuye-prompt/prompts/sources.json)。
`python3 scripts/check_prompt_sources.py` 对比上游变化并验证固定版本摘要，
GitHub Actions 每周生成报告。维护者审核材料并映射到实际工具 schema，
确保方案和工具描述不会引用不存在的工具或调用格式。

使用 `tests/eval/run_quality.sh` 在同一模型和固定任务集上成对比较默认和
候选方案。将 `ZHUYE_EVAL_BASELINE` 与 `ZHUYE_EVAL_CANDIDATE` 指向同一个
二进制，设置 `ZHUYE_EVAL_BASELINE_PROFILE=default` 与
`ZHUYE_EVAL_CANDIDATE_PROFILE=<id>`。报告记录通过率、工具调用错误、
耗时和 token 使用量。只有经过人工审核且有真实模型评测证据的型号才能
在来源清单标为 `passed`，加入智能匹配。
