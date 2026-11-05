# plugins/ —— 烛夜插件统一存放目录

仓库自带的全部插件集中在此,便于管理、审阅与分发。每个子目录是一个
完整插件(以 `plugin.toml` 清单为入口),可直接安装:

```bash
# 从仓库路径安装(user 作用域,装到 ~/.zhuye/plugins/cache/)
zhuye plugin install plugins/<插件名> --scope user

# 启用(写进 ~/.zhuye/config.toml 的 [plugins].enabled_plugins)
zhuye plugin enable <插件名>@inline
```

插件机制说明(清单格式、五类 capability、`${PLUGIN_ROOT}` 展开、热重载)
见 [`docs/plugins.md`](../docs/plugins.md);安装 / 启用 / marketplace /
排障等使用向内容见 [`docs/plugins.md`](../docs/plugins.md)。

## 现有插件

| 插件 | 功能 | 需要构建 |
|---|---|---|
| [`plugin-demo/`](plugin-demo/) | 五类 capability 全量示例(命令 / 子代理 / 技能 / hook / MCP echo) | 否(纯声明 + Python 脚本) |
| [`harness-import/`](harness-import/) | 导入外部 Harness(Claude Code / Codex / ZCode)历史会话为烛夜会话 | 是(`./scripts/build_import_plugin.sh` 构建并装配 `bin/zhuye-import`) |

## 约定

- 插件目录名 = `plugin.toml` 的 `name`(即插件 id 的 `<name>` 部分)。
- 二进制类插件把可执行文件放 `bin/`(`.mcp.json` / hooks 用
  `${PLUGIN_ROOT}/bin/<name>` 引用),`bin/` 内容由构建脚本生成、不入库。
- 纯声明资源(commands / skills / agents / hooks)直接放对应子目录。
