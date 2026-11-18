# kms —— the local knowledge base plugin

<p align="center">  <a href="README.md">中文</a> · <b>English</b>  </p>

Multi-wiki Markdown knowledge management:
`~/.zhuye/kms/<wiki>/{index.md,pages/*.md}`. On unix, directory
operations use `openat` semantics (rustix) to prevent TOCTOU /
directory-substitution attacks.

## Install (three steps)

```bash
./scripts/build_kms_plugin.sh                       # build the binary into plugins/kms/bin/
zhuye plugin install plugins/kms --scope user       # copy into the plugin cache
zhuye plugin enable kms@inline                      # write config.toml#plugins.enabled_plugins
```

## MCP tools (full name `mcp__plugin:kms@inline:kms__<tool>`)

| Tool | Description |
|---|---|
| `kms_list_wikis` | list all knowledge bases |
| `kms_list_pages` | list all pages of a base |
| `kms_get_page` | read a page's full text |
| `kms_search` | substring search across bases |
| `kms_create_wiki` | create a base |
| `kms_save_page` | create / overwrite a page (`create_only` optional) |
| `kms_delete_wiki` | delete a base |
| `kms_dream` | insight consolidation (the `dream-insights` base) |

## slash command

`/kms <query>` or `/kms save <wiki> <page> <key points>` —— see
`commands/kms.md`.
