# 烛夜插件使用与运维指南

> 本文面向插件**使用者**:怎么装、怎么管、出了问题怎么查。插件怎么写
> (manifest 字段、五类 capability、安全模型)见
> [`plugins.md`](plugins.md)。

## 快速上手

```bash
# 1. 安装(复制进 ~/.zhuye/plugins/cache 并登记 installed_plugins.json)
zhuye plugin install plugins/plugin-demo

# 2. 启用(写入 ~/.zhuye/config.toml 的 [plugins].enabled_plugins)
zhuye plugin enable demo@inline

# 3. 使用(exec 下次启动按 enabled 列表挂载;serve / SDK 会话同)
zhuye exec "/demo:hello 张三"
```

三步对应两个状态源:**install 管「装了什么」,enable 管「启用什么」**,
二者独立(见下文)。`install` / `enable` / `disable` 末尾会提示重启
`zhuye exec` —— CLI 自身不触发已在跑的进程重载;`exec` / `serve` 会话内
改 `config.toml` 则走热重载即时生效。

验证挂载成功:headless / serve 的 JSONL 事件流里应出现
`plugin_loaded`(`msg.type == "plugin_loaded"`,带插件 id)。没有这条
事件 = 没挂上,去排障小节查。

## CLI 命令参考

`zhuye plugin` 共 7 条子命令 + `marketplace` 4 条:

| 命令 | 参数 / 旗标 | 作用 |
|---|---|---|
| `install` | `<path\|id>` `--scope <managed\|user\|project\|local>` | 安装。本地目录默认 `marketplace = inline`;含 `@` 的 id 走 marketplace 路径 |
| `list` | — | 表格列出已装插件(联表 enabled 状态) |
| `info` | `<id>` | 单插件元数据:scope / version / install_path / 时间戳 / 启用状态 |
| `enable` | `<id>` | 把 id 加入 `config.toml#plugins.enabled_plugins` |
| `disable` | `<id>` | 从 `enabled_plugins` 移除(不在列表里则提示 no-op) |
| `uninstall` | `<id>` `--scope` `--yes` | 双写删除:`installed_plugins.json` + `enabled_plugins`;默认有 `[y/N]` 确认 |
| `show` | `<id>` | 打印该插件安装目录里的 `plugin.toml` 原文 |
| `marketplace ls` | — | 列出已知 marketplace |
| `marketplace add` | `<name>` `--from <github\|git\|url\|file\|directory>` + `--repo/--url/--ref/--sha/--path` | 注册 marketplace 源并拉取 manifest |
| `marketplace remove` | `<name>` | 删除已知 marketplace(真删 cache 目录) |
| `marketplace refresh` | `--name <名>` | 增量刷新;不传 = 刷全部 `auto_update = true` 的 |

`<id>` 一律是 `name@marketplace` 全名(本地安装为 `<name>@inline`)。

### install

```bash
zhuye plugin install ./my-plugin                 # 本地目录,user scope
zhuye plugin install ./my-plugin --scope local   # 指定作用域
zhuye plugin install formatter@my-tools          # 从已注册 marketplace 装(含依赖闭包)
```

分流规则:参数含 `@` 即走 marketplace 路径(要求该 marketplace 已
`marketplace add` 注册过),否则按本地目录安装。marketplace 路径成功
时输出聚合报告(成功 / 失败分块),有失败项进程 exit 1;本地路径成功
输出 `installed <id> -> <安装路径>`。

### list

输出列:`id` / `scope` / `version` / `status`(`enabled` 或
`installed`)/ `updated` / `install_path`。status 列就是两个状态源的
联表结果。一个都没装时输出空态提示。

### info / show

`info` 给元数据(含 `enabled_in` 指向 config.toml 条目);
`show` 直接打印 manifest 原文,用来核对装进去的到底是哪份 manifest。
manifest 文件缺失会明确报「cache 可能损坏」。

### enable / disable

`enable` 前会校验插件已安装(没装先 install);重复 enable 幂等。
`disable` 只是从启用列表移除,不删安装文件 —— 想彻底删除用
`uninstall`。

### uninstall

```bash
zhuye plugin uninstall demo@inline            # 交互确认 y/N
zhuye plugin uninstall demo@inline --scope user --yes # 跳过确认
```

`--scope` 必须与安装时一致(删的是该 scope 下的条目);Managed scope
的插件拒绝卸载(企业 pin)。

### marketplace

```bash
zhuye plugin marketplace add my-tools --from github --repo me/zhuye-plugins
zhuye plugin marketplace add my-tools --from git    --url https://…  --ref v1
zhuye plugin marketplace add my-tools --from url    --url https://…/marketplace.json
zhuye plugin marketplace add my-tools --from file   --path ./marketplace.json
zhuye plugin marketplace add my-tools --from directory --path ./plugins
zhuye plugin marketplace ls
zhuye plugin marketplace refresh --name my-tools
```

## 两个状态源:安装事实表 vs 启用权威表

```
~/.zhuye/config.toml                    ← 启用权威表(热重载唯一输入源)
  [plugins].enabled_plugins = ["demo@inline"]
~/.zhuye/plugins/
  ├── installed_plugins.json            ← 安装事实表(v2: id → scope 条目数组)
  ├── marketplaces.json                 ← 已知 marketplace
  └── cache/<mkt>/<name>/<version>/     ← 插件实体文件
```

- `installed_plugins.json`:同 id 可在多个 scope / 多 version 共存,
  按 scope 优先级取最高者;
- `enabled_plugins`:只有这里的 id 会被挂载;
- 两表独立维护:手工删了 cache 文件,挂载时会在读 manifest 处报
  `ManifestIo`(`PluginError::Drift` 变体已定义,但当前无触发点)。

## 作用域 scope

| scope | 语义 |
|---|---|
| `managed` | 企业统一下发 pin;`disable` / `uninstall` 被拒(`ManagedLocked`) |
| `user` | 用户级(默认),`~/.zhuye/plugins/cache` |
| `project` | 项目级(⚠️ v0 暂不支持,见已知限制) |
| `local` | 本机临时 |

优先级 `Local(4) > Project(3) > User(2) > Managed(1)`:同 id 多 scope
共存时,启用 / 挂载取最高优先级的那份安装。

## config.toml 的 `[plugins]` 段

```toml
[plugins]
enabled_plugins = ["demo@inline", "harness-import@inline"]

# 声明式 marketplace 形态(注意:当前无运行时消费点,见下)
[plugins.marketplaces.my-tools]
type = "github"          # directory | file | github | url
repo = "me/zhuye-plugins"
ref = "main"             # 可选
auto_update = true       # marketplace refresh 不带 --name 时刷它
```

`enabled_plugins` 是**启用权威表**,启动装配与热重载都读它。
`[plugins.marketplaces]` 是 schema 里声明的声明式形态,但当前版本
**没有消费点** —— marketplace 的实际状态由 CLI 读写
`~/.zhuye/plugins/marketplaces.json` 维护,管理 marketplace 以
`zhuye plugin marketplace ...` 为准。该段变更也不触发已挂载插件的
任何动作(marketplace 只在 install 时用)。

## marketplace:注册、安装与刷新

1. `marketplace add` 拉取并缓存 `.claude-plugin/marketplace.json`;
2. `install <name>@<mkt>` 从缓存里找 entry,按 entry 的 `source`
   拉插件本体,**依赖闭包按 DFS 后序安装**(依赖先、根插件后);
3. 单个依赖失败只记入报告的 failed 块,不阻断整树;已装算幂等成功;
4. `refresh [--name]` 增量更新(覆盖已知 cache);不带 `--name` 只刷
   `auto_update = true` 的。

本地路径作为依赖源只支持 `file` / `directory` 两种 source;其他
source 的依赖需先单独 fetch。

## 热重载:行为与限制

`exec` / `serve` 会话运行中,`~/.zhuye/config.toml` 的 `[plugins]` 段
变更会触发 diff 同步:新加进 `enabled_plugins` 的挂载,移除的反注册,
无需重启。

限制(重要):

- **`serve --plugin X` 不回写 config**:显式旗标只作用于本次启动
  装配;运行中 config 热重载仍按 config 列表同步 —— 如果 config 里
  没有 X,X 会在下次 `[plugins]` 段变动时被卸掉。
- 只有 `enabled_plugins` 列表的增删会同步;`[plugins.marketplaces]`
  段变更不触发挂卸。
- 单插件加载失败只 `tracing::warn!`,不影响其他插件与整体 reload;
  且 `config_reloaded` 事件**不含**插件错误字段 —— 失败只能从
  「缺失 `plugin_loaded` 事件 + stderr 日志」推断。

## SDK 与编程接入

| 入口 | 写法 |
|---|---|
| `zhuye serve` | `--plugin <id>`,可多次(覆盖 config 的 enabled 列表) |
| TypeScript SDK | `ZhuyeAgent.spawn({ plugins: ["demo@inline"] })`(`ZhuyeAgentOptions.plugins`,展开为 `--plugin` 对) |
| Python SDK | `ZhuyeAgent.spawn(ZhuyeAgentOptions(plugins=["demo@inline"]))`(同上) |
| Rust 门面 | `ZhuyeBuilder::with_plugins(Some(vec![...]))` = 显式覆盖;`None` = 按 config |
| Python 绑定 | `zhuye_py.ZhuyeBuilder(...).plugins().build()`(内部走 build_async) |

注意:Rust 门面**同步** `build()` 与 `from_thread` 不挂载任何插件
(保持最小语义);slash 命令展开在 serve / SDK 路径由宿主完成
(单条 Text 的 UserInput;展开失败原样转发)。

## 排障 FAQ

### 错误速查表

| 错误(`PluginError` 变体) | 典型场景 | 处理 |
|---|---|---|
| `ManifestIo` / `ManifestParse` | manifest 缺失 / TOML(或 JSON)语法错 | 按报错路径修文件;`zhuye plugin show <id>` 看装进去的原文 |
| `Validation` | 字段不合 schema:MCP transport 二义、capability 路径逃逸、`HookSpec::Many` 嵌套过深、dependency 段过多 | 按消息逐条修;嵌套对象错拼也会在这里暴露 |
| `InvalidPluginId` / `InvalidMarketplaceName` | id 不合 kebab-case、撞保留名 | 改名;`inline` / `builtin` 保留 |
| `AlreadyInstalled` | 同 (id, scope) 重复安装 | 换 scope 或先 uninstall |
| `InstallDirConflict` | `cache/<mkt>/<name>/<version>/` 已存在 | 换版本号或清该目录 |
| `NotInstalled` | enable / uninstall / info 未装的插件 | 先 install |
| `ManagedLocked` | 改企业 pin 的 managed 插件 | 联系下发方 |
| `Drift` | (变体已定义,当前无触发点) | — |
| `DependencyCrossMarketplace` | Qualified 依赖的 marketplace 未注册 | 先 `marketplace add` 对应源 |
| `DependencyCycle` | 依赖成环 | 打断环 |
| `MissingDependency` | 依赖未满足 | 装依赖或补 marketplace |
| `MarketplaceFetch` / `Git` / `Http` | 拉取失败 | `stderr` 字段有底层输出;查网络 / 权限 / ref |
| `MarketplaceManifestNotFound` | fetch 后找不到 `.claude-plugin/marketplace.json` | 检查目录布局 |
| `ChecksumMismatch` | sha256 不一致 | 换源;当前只在 load 时 warn 不阻断(见已知限制) |
| `StateIo` / `StateParse` | `installed_plugins.json` 读写 / 解析失败 | 查文件权限与 JSON 合法性 |
| `CapabilityLoad` | 单类能力挂载失败 | 不阻断整体;按 capability 名定位是哪类资源 |

### 常见场景

**Q:enable 了但能力没出现?**
按序查:① JSONL 有没有 `plugin_loaded` 事件(没有 = 没挂上);②
stderr 日志(单插件失败只 warn,如 hook 未知事件名、缺 command 都会
在这里);③ `zhuye plugin list` 的 status 列是不是 `enabled`;
④ CLI 改完后是否重启了进程(CLI 不跨进程热重载)。

**Q:插件 hook 拿不到环境变量(如 `ANTHROPIC_API_KEY`)?**
默认设计如此 —— 敏感 env 对 hook 子进程剔除。在 `hooks.json` 声明里
加 `"env": ["NAME"]` 白名单放行(写法见
[`plugins.md`](plugins.md) hooks 小节)。

**Q:插件 MCP server 起不来 / 工具不出现?**
① `command` 与 `url` 必须二选一(都填或都不填直接报错);② stdio
server 是干净 env(只有 HOME / PATH + 显式 env),依赖宿主变量会炸;
③ 工具全名是 `mcp__plugin:<插件id>:<server>__<tool>`,中段含
`@marketplace`,数全名时别漏;④ server 二进制用 `${PLUGIN_ROOT}` 引用
安装目录,别写相对路径。

## 已知限制

以下都有源码依据,不是猜测;用前知悉:

- **Project scope v0 不支持**(`state.rs` 标注),CLI 可传但语义未闭合;
- **`dependencies` 版本约束只 strip 不真解析**(`manifest.rs` 注明
  phase F 才做),`p@mkt@^1.0` 不会拦截不兼容版本;
- **`checksum_sha256` 只 warn 不阻断**(load 时校验失败仅告警);
- **`config_reloaded` 事件无 `plugins_errors` 字段**,插件加载失败
  只能从缺失事件 + 日志推断;
- **`user_config` schema 已定义、运行时未消费**,装插件不弹配置
  询问;
- **`[plugins.marketplaces]` config 段同样无消费点**,marketplace
  实际以 CLI / `marketplaces.json` 为准;
- **manifest 无字段级校验**(`validate_plugin.rs` 未合入),顶层未知
  键静默丢弃 —— 字段写错不报错,只是没效果;
- **CLI 改动不触发跨进程热重载**,末尾统一提示重启 `zhuye exec`;
- 交互式 TUI(可视化插件管理)在独立仓库,本仓库 CLI 只有命令面。

## 延伸阅读

| 文档 | 内容 |
|---|---|
| [`plugins.md`](plugins.md) | 插件开发:manifest 字段、五类 capability、安全模型 |
| [`sdk.md`](sdk.md) | serve 模式与双 SDK 接入 |
| [`tools-and-hooks.md`](tools-and-hooks.md) | 内置工具与 hooks 契约 |
| [`../plugins/README.md`](../plugins/README.md) | 仓库自带插件目录约定 |
