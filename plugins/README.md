# plugins/ —— 烛夜插件统一存放目录

<p align="center">  <b>中文</b> · <a href="README.en.md">English</a>  </p>

仓库自带的全部插件集中在此,便于管理、审阅与分发。每个子目录是一个
完整插件(以 `plugin.toml` 清单为入口),可直接安装:

```bash
# 从仓库路径安装(user 作用域,装到 ~/.zhuye/plugins/cache/)
zhuye plugin install plugins/<目录名> --scope user

# 启用(写进 ~/.zhuye/config.toml 的 [plugins].enabled_plugins;
# 注意 id 的 <name> 部分取清单的 name,不是目录名)
zhuye plugin enable <清单名>@inline
```

插件机制说明(清单格式、六类 capability、`${PLUGIN_ROOT}` 展开、热重载)
见 [`docs/plugins.md`](../docs/plugins.md);安装 / 启用 / marketplace /
排障等使用向内容见 [`docs/plugins.md`](../docs/plugins.md)。

## 现有插件

| 目录名 | 清单名(id) | 定位 |
|---|---|---|
| [`plugin-demo/`](plugin-demo/) | `demo` | 全能力示例:六类 capability(commands / 子代理 / 技能 / hook / MCP / WASM)各配一个最小可运行样例 |
| [`computer-use/`](computer-use/) | `computer-use` | 屏幕 / 鼠标 / 键盘操作(xcap + enigo;需先构建二进制) |
| [`browser/`](browser/) | `browser` | 受控浏览器(CDP 仅 loopback + 隔离 profile;F39 元素寻址;需先构建二进制) |
| [`git-tools/`](git-tools/) | `git-tools` | git 工作流:分支 / PR / diff 探测 + checkpoint 快照 + Stop 自动 checkpoint(需先构建二进制) |
| [`shell/`](shell/) | `shell` | 常驻 shell 会话池:启动进程 / 增量读输出 / 写 stdin / kill(需先构建二进制) |
| [`harness-import/`](harness-import/) | `harness-import` | 导入外部 Harness(Claude Code / Codex / ZCode)历史会话;一键导入其他本地 agent 的模型 / MCP 配置(需先构建二进制) |
| [`goal-queue/`](goal-queue/) | `goal-queue` | 目标模式(goal 工具)+ FIFO 队列模式(queue 工具;门控型,默认关) |
| [`kms/`](kms/) | `kms` | 本地 Markdown 知识库:多 wiki 分页 + grep 检索 + dream 固化(需先构建二进制) |
| [`web-search/`](web-search/) | `web-search` | `web_search` 工具(Brave API;门控型,**默认开**) |
| [`notebook/`](notebook/) | `notebook` | `notebook_edit` 工具(Jupyter cell 编辑;门控型,**默认开**) |
| [`verify-hooks/`](verify-hooks/) | `verify-hooks` | verification / test_runner 校验双 hook 的插件启用路径(门控型,默认关,双路径) |
| [`lsp/`](lsp/) | `lsp` | LSP 集成粗粒度开关与文档(门控型;激活由 `[lsp_servers]` 配置驱动) |

⚠️ **目录名与清单名不必相同**:manifest 的 `name` 必填,并用于构成
插件 id(`name@marketplace` 的 `<name>` 部分)。以 `plugin-demo` 为例,
目录名是 `plugin-demo`、清单名是 `demo`,启用时写
`zhuye plugin enable demo@inline`。

## 约定

- 二进制类插件把可执行文件放 `bin/`(`.mcp.json` / hooks 用
  `${PLUGIN_ROOT}/bin/<name>` 引用),`bin/` 内容由构建脚本生成、不入库。
- 纯声明资源(commands / skills / agents / hooks)直接放对应子目录。
