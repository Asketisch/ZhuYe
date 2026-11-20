# 插件使用

<p align="center">  <b>中文</b> · <a href="plugins.en.md">English</a>  </p>

> 本文面向**使用者**:仓库内置了哪些插件、如何安装与启用、marketplace
> 怎么管、状态文件在哪、出了问题怎么排查。manifest 字段、capability
> 声明与安全模型等**作者向**内容见 [plugin-dev-guide.md](plugin-dev-guide.md);
> marketplace 的发布侧见 [plugin-marketplace.md](plugin-marketplace.md)。

## 插件是什么(30 秒)

插件是一个**目录**,入口是 `plugin.toml` manifest(同时兼容
`.claude-plugin/plugin.json` 的 Claude Code 格式)。manifest 声明该插件
向引擎挂载哪一类或多类能力,六类如下:

| 能力 | 声明字段 | 形态 |
|---|---|---|
| slash 命令 | `commands` | `commands/*.md` prompt 模板 |
| 子代理 | `agents` | `agents/*.md`,注册 `call_<role>` 工具 |
| 技能 | `skills` | `skills/<name>/SKILL.md`,`load_skill` 按需激活 |
| hooks | `hooks` | Claude Code 风格 `hooks.json`(shell 子进程或 WASM 组件) |
| MCP server | `mcp_servers` | `.mcp.json`,stdio / streamable-http 工具注入 |
| WASM 组件工具 | `wasm_tools` | 实现 `zhuye:plugin` WIT 世界的 `.wasm` 文件 |

一个插件可以只挂载其中一类,也可以全部挂载(示例插件
[`plugins/plugin-demo/`](../plugins/plugin-demo/) 六类都有)。

安装与启用是两个独立动作:**install 把插件目录复制到
`~/.zhuye/plugins/cache` 并登记,enable 把插件 id 写进启用列表**;
引擎启动(或运行中热重载)时按启用列表挂载 —— 装了不等于生效,
必须 enable。

## 仓库内置插件

仓库自带 11 个插件,集中在 [`plugins/`](../plugins/) 目录(约定见其
README)。下表中的「id」是启用 / 卸载时用的 `name@marketplace` 全名;
本地安装时 marketplace 固定为 `inline`:

| 目录 | 清单名 | id | 定位 |
|---|---|---|---|
| [`plugin-demo/`](../plugins/plugin-demo/) | `demo` | `demo@inline` | 全能力示例:六类 capability 各配一个最小可运行样例 |
| [`computer-use/`](../plugins/computer-use/) | `computer-use` | `computer-use@inline` | 屏幕 / 鼠标 / 键盘操作(xcap + enigo,需先构建二进制;macOS 需授予屏幕录制与辅助功能权限) |
| [`git-tools/`](../plugins/git-tools/) | `git-tools` | `git-tools@inline` | git 工作流:分支 / PR / diff 探测 + checkpoint 快照 + Stop 自动 checkpoint(需先构建二进制) |
| [`shell/`](../plugins/shell/) | `shell` | `shell@inline` | 常驻 shell 会话池:后台启动进程、增量读输出、写 stdin、kill,适合 REPL / dev server(需先构建二进制) |
| [`harness-import/`](../plugins/harness-import/) | `harness-import` | `harness-import@inline` | 导入外部 harness(Claude Code / Codex / ZCode)历史会话;一键导入其他本地 agent 的模型 / MCP 配置(需先构建二进制) |
| [`goal-queue/`](../plugins/goal-queue/) | `goal-queue` | `goal-queue@inline` | 目标模式 + FIFO 队列模式:启用后注册 `goal` / `queue` 工具并启动队列 driver(门控型,默认关) |
| [`kms/`](../plugins/kms/) | `kms` | `kms@inline` | 本地 Markdown 知识库:多 wiki 分页 + grep 检索 + 洞见固化(dream)(需先构建二进制) |
| [`web-search/`](../plugins/web-search/) | `web-search` | `web-search@inline` | `web_search` 工具(Brave API)。**门控型,默认开**;disable 即工具不注册(区别于 `[active].disallowed_tools` 仅隐藏) |
| [`notebook/`](../plugins/notebook/) | `notebook` | `notebook@inline` | `notebook_edit` 工具(Jupyter cell 级编辑)。**门控型,默认开** |
| [`verify-hooks/`](../plugins/verify-hooks/) | `verify-hooks` | `verify-hooks@inline` | `verification` / `test_runner` 校验双 hook 的插件启用路径。**门控型,默认关**(双路径:与 config `[hooks] enabled` 等价) |
| [`lsp/`](../plugins/lsp/) | `lsp` | `lsp@inline` | LSP 集成的**粗粒度开关**与文档载体:激活始终由 `[lsp_servers]` 配置驱动;disable 即整体关闭。**门控型** |

### 门控型插件

goal-queue / web-search / notebook / verify-hooks / lsp 是**门控型**插件:
能力本体留在引擎 crate(保持原工具名 / 审批 / 行为,外部用法零影响),
注册与否由插件启用状态决定,判定单一事实源是 `zhuye-plugin` 的
`bundled_plugin_enabled`(默认开关表 `BUNDLED_PLUGIN_DEFAULTS`)。

- **默认开**(web-search / notebook):空配置下与插件化之前完全一致;
  关闭走 `zhuye plugin disable <id>@inline` 或 config
  `[plugins].disabled_plugins`(工具不进注册表,`tool_search` 搜不到);
- **默认关**(goal-queue / verify-hooks / lsp):enable 后等价于原有
  config 路径显式启用(lsp 的激活另需 `[lsp_servers]` 定义);
- 判定不热重载,变更后重启会话生效。

⚠️ 注意 `plugin-demo` 的目录名是 `plugin-demo`,但 manifest 的
`name = "demo"` —— **目录名与清单名可以不同,插件 id 以清单
`name` 为准**,所以启用时写 `zhuye plugin enable demo@inline` 而不是
`plugin-demo@inline`。

## 安装与启用

完整命令面(`zhuye plugin` 共 7 条子命令 + `marketplace` 5 条,
定义见 `crates/runtime/zhuye-cli/src/main.rs`):

| 命令 | 参数 / 旗标 | 作用 |
|---|---|---|
| `install` | `<路径或 name@marketplace>` `--scope <managed\|user\|project\|local>` | 安装。本地路径复制到 cache 并登记(默认 `user` scope,本地安装的 id 为 `name@inline`);含 `@` 的 id 走已注册 marketplace 路径(含依赖闭包) |
| `enable` | `<id>` | 把 id 加入启用列表(要求**先安装**;未装报 `NotInstalled`);若此前在 `disabled_plugins` 里会一并移除 |
| `disable` | `<id>` | 从启用列表移除并写入 `disabled_plugins`(不删安装文件)—— 对默认开的门控插件(web-search / notebook)这是唯一有效关闭方式 |
| `uninstall` | `<id>` `--scope` `--yes` | 双删:`installed_plugins.json` 与启用列表;默认 y/N 交互确认,`--yes` 跳过;`--scope` 须与安装时一致;managed scope 拒绝卸载(企业 pin) |
| `list` | — | 表格列出已装插件,联表启用状态(`enabled` / `installed`) |
| `info` | `<id>` | 单插件元数据:scope / version / install_path / 时间戳 / 启用状态 |
| `show` | `<id>` | 打印该插件安装目录里的 `plugin.toml` 原文(核对装进去的是哪份 manifest) |

```bash
# 1. 安装:本地目录(或从已注册 marketplace 装 name@marketplace)
zhuye plugin install plugins/plugin-demo          # 本地安装,id 为 name@inline
zhuye plugin install plugins/plugin-demo --scope local   # 指定作用域(默认 user)
zhuye plugin install formatter@my-tools           # 从已注册 marketplace 装(含依赖闭包)

# 2. 启用:写进 ~/.zhuye/config.toml 的 [plugins].enabled_plugins
zhuye plugin enable demo@inline

# 3. 查看
zhuye plugin list                                  # 表格:id / scope / version / status
zhuye plugin info demo@inline                      # 单插件元数据
zhuye plugin show demo@inline                      # manifest 原文

# 4. 停用 / 卸载
zhuye plugin disable demo@inline                   # 只移除启用,保留安装
zhuye plugin uninstall demo@inline --yes           # 彻底删除(--scope 须与安装时一致)
```

启用状态的**权威源**是 `~/.zhuye/config.toml` 的
`[plugins].enabled_plugins`(条目为 `name@marketplace` 全名):
启动装配与运行中热重载都只读它。`enable` / `disable` 只是改写
该列表的便捷入口,直接编辑文件等价生效。

### scope(安装作用域)

| scope | 语义 |
|---|---|
| `managed` | 企业统一下发 pin;`disable` / `uninstall` 被拒(`ManagedLocked`) |
| `user` | 用户级,**默认**(`~/.zhuye/plugins/cache`) |
| `project` | 项目级(⚠️ v0 暂不支持,CLI 可传但语义未闭合) |
| `local` | 本机临时 |

同一 id 可在多个 scope 下共存;查找 / 挂载的优先级
`local > project > user > managed`(取最高优先级的那份安装)。
设置非空 `ZHUYE_HOME` 时,上述所有路径都改落在 `ZHUYE_HOME` 下。

## 市场(marketplace)

`zhuye plugin marketplace` 共 5 条子命令:

```bash
# 注册一个 marketplace 源(五种 source 类型,均真接通)
zhuye plugin marketplace add my-tools --from github --repo me/zhuye-plugins
zhuye plugin marketplace add local-mkt --from directory --path /path/to/mkt

# 列出 / 刷新 / 发现 / 删除
zhuye plugin marketplace ls
zhuye plugin marketplace refresh --name my-tools   # 不传 --name = 刷全部 auto_update 的
zhuye plugin marketplace discover --limit 20       # 按 GitHub topic(默认 zhuye-plugin)发现仓库
zhuye plugin marketplace remove local-mkt
```

注册后即可用 `zhuye plugin install <name>@<marketplace>` 安装其中的
插件(含依赖闭包)。manifest 格式、source 类型细节与发布流程深入
[plugin-marketplace.md](plugin-marketplace.md)。

> ⚠️ **directory 源的危险**:directory 类型的 marketplace 以**源目录
> 本身作为安装位置**(不做复制,见
> `crates/runtime/zhuye-cli/src/plugin/marketplace.rs:120`),因此
> `marketplace remove` 会对该路径执行 `remove_dir_all`
> (`marketplace.rs:216`)—— **可能直接删除你的原始目录**。不要用
> 有价值的目录注册 directory marketplace;删除前先确认
> `marketplace ls` 里记录的 install_location。

## 状态文件与热重载

```
~/.zhuye/
├── config.toml
│   └── [plugins].enabled_plugins = ["demo@inline", ...]   ← 启用权威表
└── plugins/
    ├── installed_plugins.json      ← 安装事实表:id → 各 scope 条目(路径 / 版本 / scope)
    ├── marketplaces.json           ← 已知 marketplace(name / source / install_location)
    └── cache/<marketplace>/<plugin>/<version>/   ← 插件实体文件
```

两表独立维护:**install 管「装了什么」,enable 管「启用什么」**。

- **启动挂载**:进程启动时按 `enabled_plugins` 列表逐个读 cache 里的
  manifest 并挂载;挂载成功的插件在 JSONL 事件流里产出
  `plugin_loaded`(`msg.type == "plugin_loaded"`,带插件 id)——
  没有这条事件 = 没挂上,去「排障」查。
- **运行中热重载**:exec / serve 会话内改动 `config.toml` 的
  `[plugins]` 段会 diff 同步 —— 新启用的挂载、移除的反注册,无需重启。
  注意 CLI(`zhuye plugin ...`)自身**不**触发已在跑进程的重载,
  改完需重启 `zhuye exec`(命令末尾有提示);`serve --plugin X`
  旗标只作用于本次启动,不回写 config。
- **部分成功语义**:单类能力失败只 warn 并跳过,不阻断其他能力;
  单插件失败不影响其他插件。

## 排障

插件「装了但没生效」的常见原因,按命中率排:

1. **没 enable** —— 安装只登记不挂载。确认 `~/.zhuye/config.toml` 的
   `[plugins].enabled_plugins` 里有该 id;`zhuye plugin list` 的
   status 列应为 `enabled`(而非 `installed`)。
2. **装到了别的 scope** —— 同 id 多 scope 共存时按
   `local > project > user > managed` 取最高者,且 `uninstall` 的
   `--scope` 须与安装时一致。`zhuye plugin info <id>` 查实际 scope。
3. **manifest name 与期望 id 不符** —— 插件 id 的 `<name>` 部分取
   manifest 的 `name` 字段,**不是目录名**(见「仓库内置插件」的
   `plugin-demo` / `demo` 例子)。enable / uninstall 写错 id 会报
   `NotInstalled`。
4. **manifest 字段写错** —— 当前 schema 不校验未知字段:顶层写错的
   键被静默丢弃,不报错、只是不生效(嵌套对象错拼才会报
   `ManifestParse`)。用 `zhuye plugin show <id>` 对照原文,必要时
   回 [plugin-dev-guide.md](plugin-dev-guide.md) 查字段表。

进一步排查:① JSONL 事件流找 `plugin_loaded`(没有 = 挂载失败);
② stderr 日志 —— 单插件 / 单能力失败只 `tracing::warn!`
(如 hook 未知事件名、缺 command、MCP server 启动失败都在这里);
③ 二进制类插件(computer-use / git-tools / shell / harness-import /
kms)确认已跑对应构建脚本(`./scripts/build_*_plugin.sh`),否则
`${PLUGIN_ROOT}/bin/...` 指向缺失文件;④ CLI 改完 enable / disable
后重启 `zhuye exec`(CLI 不跨进程热重载)。

## 延伸阅读

| 文档 | 内容 |
|---|---|
| [plugin-marketplace.md](plugin-marketplace.md) | marketplace:manifest 格式、来源类型、发布与刷新流程 |
| [plugin-dev-guide.md](plugin-dev-guide.md) | 插件开发:manifest 字段、capability 声明、安全模型、WASM 多语言 |
| [architecture.md](architecture.md) | 六层架构与 `zhuye-plugin` 定位 |
| [../plugins/README.md](../plugins/README.md) | 仓库自带插件目录与约定 |
