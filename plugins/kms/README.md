# kms —— 本地知识库插件

多 wiki Markdown 知识管理:`~/.zhuye/kms/<wiki>/{index.md,pages/*.md}`。
unix 下目录操作走 `openat` 语义(rustix),防 TOCTOU / 目录替换攻击。

## 安装(三步)

```bash
./scripts/build_kms_plugin.sh                       # 构建二进制到 plugins/kms/bin/
zhuye plugin install plugins/kms --scope user       # 复制进插件 cache
zhuye plugin enable kms@inline                      # 写 config.toml#plugins.enabled_plugins
```

## MCP 工具(全名 `mcp__plugin:kms@inline:kms__<tool>`)

| 工具 | 说明 |
|---|---|
| `kms_list_wikis` | 列全部知识库 |
| `kms_list_pages` | 列某库全部页面 |
| `kms_get_page` | 读页面全文 |
| `kms_search` | 跨库子串检索 |
| `kms_create_wiki` | 建库 |
| `kms_save_page` | 建 / 覆盖页面(`create_only` 可选) |
| `kms_delete_wiki` | 删库 |
| `kms_dream` | 洞见固化(`dream-insights` 库) |

## slash 命令

`/kms <查询>` 或 `/kms save <wiki> <page> <要点>` —— 见 `commands/kms.md`。
