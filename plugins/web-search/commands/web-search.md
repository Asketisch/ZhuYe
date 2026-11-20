---
description: 联网搜索:用 web_search 工具查最新信息。参数:搜索意图(自然语言)
---

用户想要联网搜索。把用户意图整理成一组精炼的搜索关键词,调用
`web_search` 工具:

```json
{"query": "<关键词>", "count": 5}
```

用户指令:

$ARGUMENTS

规则:

1. 关键词精炼(中英文按内容主体选择);一次不够可换关键词再搜,
   但同一 turn 不超过 3 次。
2. 结果里的 URL 已消毒(query / fragment / userinfo 被剥除);要读
   详情用 `web_fetch`(注意它走 Prompt 审批)。
3. 引用来源时给出结果标题与 URL,不要凭记忆补内容。
4. 若返回 `BRAVE_API_KEY` 相关错误,如实告知用户在
   `[web_search]` 段或环境变量配置 key,不要假装搜索成功。
