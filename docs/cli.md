# CLI 参考

> `zhuye` 二进制的子命令一览与用法。快速开始见根目录
> [README](../README.md)。

## 常用子命令

| 命令 | 干什么 |
|------|--------|
| `exec` | 跑一轮就走，事件以 JSONL 写 stdout |
| `serve` | 常驻的会话服务，SDK 走的就是它 |
| `config` | 看 / 改 `~/.zhuye/config.toml`，改完热重载 |
| `session` | 列会话、看内容、删、fork、跨会话搜索 |
| `plugin` / `mcp` / `lsp` | 装扩展、接 MCP server、接语言服务器 |
| `models` | 看当前 provider 有哪些模型可用，清单实时取自接口 |
| `discussion` / `pipeline` / `task` | 跑多 agent 讨论、DAG 流水线、任务与团队 |

完整列表看 `zhuye --help`，参数看 `zhuye <命令> --help`。

## zhuye-import：导入其他 agent 的会话与配置

独立二进制 `zhuye-import` 把 Claude Code / Codex / ZCode 的历史会话导入成烛夜会话，导入后就能 `zhuye exec --resume` 接着跑。它还能**一键导入其他本地 agent 的配置**（v1.10）：

```bash
zhuye-import config            # 扫描 Claude Code / Codex / Gemini / opencode / crush
                                # 的模型 provider 与 MCP server 配置,增量合并进
                                # ~/.zhuye/config.toml(已有配置不覆盖,重复执行幂等)
zhuye-import config --from claude-code --dry-run   # 只看会导入什么
```
