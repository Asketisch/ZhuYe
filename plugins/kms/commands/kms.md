---
description: 用本地知识库(KMS)查资料或固化洞见。参数:查询词,或 "save <wiki> <page>" + 正文提示
---

你在操作用户的本地知识库(KMS,存储于 `~/.zhuye/kms/`,经 `kms_*` MCP 工具访问)。

用户输入:

$ARGUMENTS

规则:

1. 如果输入是查询意图 → 先 `kms_list_wikis` 看有哪些库,再 `kms_search` 检索;
   命中后用 `kms_get_page` 读全文,引用时注明 `wiki/page`。
2. 如果输入以 `save <wiki> <page>` 开头 → 把对话中的相关结论整理成
   Markdown,用 `kms_save_page`(wiki 不存在先 `kms_create_wiki`;
   首行 frontmatter 写 title/tags)。
3. 如果用户说「记住这些经验」→ 把要点提炼成洞见列表,用 `kms_dream` 固化。
4. 找不到就明说,不要编造知识库内容。
