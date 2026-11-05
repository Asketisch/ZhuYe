# 烛夜插件开发与使用

烛夜的插件系统是**声明式 / 数据驱动**的:插件是磁盘上一组遵循目录约定
的文件(`plugin.toml` manifest + 五类 capability 资源),由运行时扫描后挂载
到共享 registry。没有动态库加载,插件不携带原生代码 —— 跨进程逻辑通过
shell hook 与 MCP server 表达,权限模型与内置工具一致。

> 兼容说明:manifest 同时支持 `plugin.toml`(主格式)与
> `.claude-plugin/plugin.json`(Claude Code 兼容格式),五类 capability
> 语义对齐 Claude Code 插件,便于现成插件迁移。
>
> 本文覆盖**作者向**(manifest 字段、五类 capability、安全模型)与
> **使用者向**(CLI 命令、状态源、热重载、排障)。marketplace 的发布侧
> 见 [`plugin-marketplace.md`](plugin-marketplace.md)。

## 快速开始

```bash
# 1. 安装(复制进 ~/.zhuye/plugins/cache 并登记 installed_plugins.json)
zhuye plugin install plugins/plugin-demo

# 2. 启用(写入 ~/.zhuye/config.toml 的 [plugins].enabled_plugins)
zhuye plugin enable demo@inline

# 3. 使用(exec 下次启动按 enabled 列表挂载;serve / SDK 会话同)
zhuye exec "/demo:hello 张三"
```

三步对应两个状态源:**install 管「装了什么」,enable 管「启用什么」**,
二者独立(见「两个状态源与作用域」)。`install` / `enable` / `disable`
末尾会提示重启 `zhuye exec` —— CLI 自身不触发已在跑的进程重载;
`exec` / `serve` 会话内改 `config.toml` 则走热重载即时生效。

验证挂载成功:headless / serve 的 JSONL 事件流里应出现
`plugin_loaded`(`msg.type == "plugin_loaded"`,带插件 id)。没有这条
事件 = 没挂上,去「排障」小节查。

## 从零创建第一个插件

下面用五分钟做一个只有一条 slash 命令的最小插件,跑通「声明 → 安装 →
启用 → 使用」全链路。

```bash
# 1. 建目录(目录名与 manifest 的 name 保持一致)
mkdir -p /tmp/my-plugin/commands

# 2. 写 manifest
cat > /tmp/my-plugin/plugin.toml <<'EOF'
name = "my-plugin"
version = "0.1.0"
description = "我的第一个烛夜插件"
commands = "./commands"
EOF

# 3. 写一条命令(md 文件名即命令 basename)
cat > /tmp/my-plugin/commands/hello.md <<'EOF'
---
description: 向指定对象打一声招呼
---
请用一句中文向 $1 打一声招呼,语气友好。
EOF

# 4. 安装 + 启用
zhuye plugin install /tmp/my-plugin
zhuye plugin enable my-plugin@inline

# 5. 使用(exec 每次启动按 enabled 列表挂载)
zhuye exec "/my-plugin:hello 世界"
```

预期行为:提交给模型的不是 `/my-plugin:hello 世界`,而是展开后的正文
「请用一句中文向 世界 打一声招呼,语气友好。」。`$1` 被替换为第一个
位置参数;占位符规则见「commands」小节。命令全名是
`<插件名>:<basename>` —— 插件叫 `my-plugin`,命令就叫
`my-plugin:hello`。

可运行的完整示例见 [`plugins/plugin-demo/`](../plugins/plugin-demo/)。
仓库自带插件统一放在 [`plugins/`](../plugins/) 目录(约定见其 README);
功能型插件实例:[`plugins/harness-import/`](../plugins/harness-import/) ——
把 Claude Code / Codex / ZCode 历史会话导入为烛夜会话(能力核心是
`crates/integrations/zhuye-import`,经 MCP server 工具 + slash 命令接入,
二进制同时支持终端直跑批量导入)。

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

### list / info / show

`list` 输出列:`id` / `scope` / `version` / `status`(`enabled` 或
`installed`)/ `updated` / `install_path`。status 列就是两个状态源的
联表结果。一个都没装时输出空态提示。

`info` 给元数据(含 `enabled_in` 指向 config.toml 条目);
`show` 直接打印 manifest 原文,用来核对装进去的到底是哪份 manifest。
manifest 文件缺失会明确报「cache 可能损坏」。

### enable / disable / uninstall

`enable` 前会校验插件已安装(没装先 install);重复 enable 幂等。
`disable` 只是从启用列表移除,不删安装文件 —— 想彻底删除用
`uninstall`。

```bash
zhuye plugin uninstall demo@inline                    # 交互确认 y/N
zhuye plugin uninstall demo@inline --scope user --yes # 跳过确认
```

`--scope` 必须与安装时一致(删的是该 scope 下的条目);Managed scope
的插件拒绝卸载(企业 pin)。

## 两个状态源与作用域

以下路径使用默认数据目录；设置非空 `ZHUYE_HOME` 时，配置、安装记录、
marketplace 和插件缓存均位于该目录下，CLI 与运行时使用同一位置。
会话 rollout 和导入的会话同样存放在该目录的 `sessions/` 下。

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

| scope | 语义 |
|---|---|
| `managed` | 企业统一下发 pin;`disable` / `uninstall` 被拒(`ManagedLocked`) |
| `user` | 用户级(默认),`~/.zhuye/plugins/cache` |
| `project` | 项目级(⚠️ v0 暂不支持,见「已知限制」) |
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

## 插件目录结构

```
my-plugin/
├── plugin.toml              # 必需:manifest(或 .claude-plugin/plugin.json)
├── commands/*.md            # slash 命令(prompt 模板)
├── agents/*.md              # 子代理定义
├── skills/<name>/SKILL.md   # 技能
├── hooks/hooks.json         # 外部进程 hook(Claude Code 风格)
└── .mcp.json                # MCP server 声明
```

manifest 发现顺序(`PluginManifest::find_in_dir`):`plugin.toml` 优先,
其次 `.claude-plugin/plugin.json`。两者语义等价,前者是 TOML,后者是
Claude Code 兼容的 JSON 形态。

## plugin.toml 字段参考

### schema 语义(先读这段)

- manifest 只保证**合法 TOML + 结构匹配**;字段级语义校验(命名正则等)
  尚未合入,坏字段多在后续 install / scan 阶段才报错。
- **顶层未知键被 serde 静默丢弃**(写错字段名不报错,只是不生效);
  但**嵌套对象**(`user_config`、`mcp_servers` 等)保持 strict,错拼会被
  `ManifestParse` 捕获。

### 元信息字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | String | **是** | kebab-case;install 时归一化为小写;目录名约定与之相同 |
| `version` | String | 否 | 省略时 install 落 `"0.0.0"` |
| `description` | String | 否 | 一句话说明 |
| `author` | 对象 | 否 | `{ name, email?, url? }`,`name` 必填 |
| `license` | String | 否 | SPDX 标识,如 `Apache-2.0` |
| `homepage` | String | 否 | |
| `repository` | 对象 | 否 | `{ url?, ref?, sha? }` |
| `keywords` | String[] | 否 | 检索标签 |
| `dependencies` | String[] | 否 | 依赖的其他插件,见下 |
| `user_config` | 对象 | 否 | 用户配置项声明,见下 |

### 五类 capability 字段

| 字段 | 可填形态 | 指向 |
|---|---|---|
| `commands` | 省略 / `"./commands"` / `["./a", "./b"]` | slash 命令目录 |
| `agents` | 省略 / 路径 / 路径数组 | 子代理 md 目录 |
| `skills` | 省略 / 路径 / 路径数组 | 技能目录 |
| `hooks` | 省略 / 路径 / 内联 JSON / 上述的数组 | hooks.json |
| `mcp_servers` | 省略 / 路径 / 内联 map | `.mcp.json` 或内联 |

`hooks` 的 `Many`(数组)形态用于叠加多个来源,其元素只能是路径或内联
JSON;再嵌套 `None` 或 `Many`(深度限制 1)会报 `Validation` 错误。

### dependencies 语法

```toml
dependencies = [
  "formatter",            # 裸名:在本插件所在 marketplace 内解析
  "formatter@anthropic-tools",  # 限定 marketplace
  "formatter@anthropic-tools@^1.0",  # 三段式:版本后缀被静默 strip
]
```

解析规则(`PluginDependency::from_str`):

| 字面量 | 解析结果 |
|---|---|
| `"p"` | `Bare("p")` |
| `"p@mkt"` | `Qualified { name: "p", marketplace: "mkt" }` |
| `"p@^1.0"` | 第二段以 `^ ~ = > <` 开头 → 视为版本约束,降级为 `Bare("p")` |
| `"p@mkt@^1.0"` | `Qualified`,版本后缀 strip(数字开头的段不误判,`name@1password` 安全) |
| 超过 3 段 | `Validation` 报错 |

**注意**:版本约束目前只 strip 不真解析(见「已知限制」),`^1.0` 不会
拦截装到不兼容版本。Qualified 依赖要求目标 marketplace 已注册
(`known.marketplaces`),否则报 `DependencyCrossMarketplace`。

### user_config(已定义,运行时尚未消费)

```toml
[user_config.api_token]
type = "string"          # string | number | boolean | directory | file
title = "API Token"
description = "服务端 API 令牌"
required = true
sensitive = true         # 设计意图:敏感值进 OS keychain,不进配置文件
```

| 字段 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `type` | 是 | — | 五种类型见上 |
| `title` / `description` | 是 | — | 展示用 |
| `required` / `sensitive` / `multiple` | 否 | `false` | |
| `default` | 否 | — | 缺省值 |
| `min` / `max` | 否 | — | number 类型的取值范围 |

⚠️ 当前版本 `user_config` **只有 schema,没有运行时消费点**:装插件时
不会弹配置询问,值也不会注入 hook / MCP 的 env。先按纯声明写,行为以
后续版本为准。

## 五类 capability

### commands —— slash 命令(prompt 模板)

`commands/` 下的 `*.md` 文件即命令,文件名(去 `.md`)是命令 basename,
子目录作为命名空间,全名为 `<插件名>:<命名空间>:<basename>`
(如 `demo:hello`、`demo:utils:lint`)。

frontmatter 支持 `description`。正文是 prompt 模板,支持参数占位符:

| 占位符 | 展开为 |
|---|---|
| `$ARGUMENTS` | 命令名之后的全部参数原文 |
| `$1` .. `$9` | 按空白切分的位置参数(缺失替换为空串) |

展开规则:

- 用户输入 `/demo:hello 张三 你好` → 查 `demo:hello` 命令 → 读 md、
  剥 frontmatter、替换占位符 → 用展开文本替换原输入提交;
- 正文没有任何占位符且参数非空时,参数以空行追加在正文末尾;
- 单遍扫描替换,`$ARGUMENTS` 的内容不会二次替换;`$12` 这类字面量
  不会被吃成 `$1` + `2`;
- 未命中任何命令的 `/xxx` 输入原样透传,不影响普通 prompt;
- 命令只是 **prompt 模板替换**:不注册工具、不改变模型可见面。
  展开后的 Submission 在 `source_command` 字段标注来源命令,
  随 rollout 持久化。正文在展开时才读 —— 改 md 即生效,无需重装。

### agents —— 子代理

`agents/*.md` 的 frontmatter 解析为 `SubAgentSpec` 并注册
`call_<role>` 工具(模型可委派):

```markdown
---
name: My Reviewer
description: 你是一名代码评审子代理……(当前版本 system prompt 取自 description)
tools: [read, grep]
---
```

`role` 由插件全名派生(`:` 替换为 `_`,截断 32 字符)。工具权限下限为
`Prompt`(见「安全模型」)。

### skills —— 技能

`skills/<name>/SKILL.md`,frontmatter 至少包含 `description`。挂载后进入
`SkillsCatalog`,模型可经内置 `load_skill` 工具按需激活(渐进披露)。
没有 `SKILL.md` 的子目录被跳过(不报错)。

### hooks —— 外部进程 hook

`hooks.json` 采用 Claude Code 风格:

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "*",
        "hooks": [
          { "type": "command", "command": "echo done >&2", "timeout": 10 }
        ]
      }
    ]
  }
}
```

支持的事件(大小写不敏感):`PreToolUse` / `PostToolUse` /
`PostToolUseFailure` / `Stop` / `SessionStart` / `UserPromptSubmit` /
`PreCompact` / `PostCompact` / `SessionEnd` / `TaskCreated` /
`TaskCompleted` / `TaskUpdated`。每条声明包装为 `ShellHook`:事件 JSON
写入子进程 stdin,子进程用 stdout 返回决策 JSON(`allow` / `deny` /
`ask`);**超时或非零退出一律 fail-closed(Deny)**。

两条容错语义:未知事件名 → warn 后跳过;缺 `command` 字段 → warn 后
跳过;都不阻断同插件其他能力挂载。

**env 白名单**:hook 子进程默认拿不到敏感环境变量
(如 `ANTHROPIC_API_KEY`)。确需放行时在声明里显式列出:

```json
{ "type": "command", "command": "./notify.sh", "env": ["MY_TOKEN"] }
```

`env` 数组里的变量名优先于 secret 剔除。装插件已等同授权任意代码
执行,但没必要顺手把宿主凭据也递过去 —— 默认最小授予。

### mcp_servers —— MCP server

`.mcp.json` 为扁平 map(`{"<server名>": {"command": ..., "args": [...]}}`,
`command` 为 stdio 传输、`url` 为 streamable-http 传输)。两者**必须
且只能有一个**:同时含 `command` 与 `url` 报 transport 二义错误,都缺
报缺 transport 错误。可选字段:`env` / `headers` / `timeout_secs`。

挂载后工具以 `mcp__plugin:<插件id>:<server名>__<工具名>` 全名注入
`ToolRegistry`,对模型可见;插件禁用时 server 停止、工具反注册。
注意全名中段是**插件 id(`name@marketplace` 全名)**,如本地安装的
demo 插件其工具形如
`mcp__plugin:demo@inline:echo__echo`。

stdio server 以干净 env 启动(只透传 HOME / PATH 与显式声明的
`env`),不继承宿主密钥。

## `${PLUGIN_ROOT}` 占位符

插件安装后会被复制到 `~/.zhuye/plugins/cache/<marketplace>/<插件>/<版本>/`,
manifest 里的相对路径不再指向原目录。hook 命令与 MCP server 的
`command` / `args` / `env` 值中的 `${PLUGIN_ROOT}` 会在挂载时展开为插件
安装目录的绝对路径:

```json
{
  "echo": {
    "command": "python3",
    "args": ["${PLUGIN_ROOT}/servers/echo_server.py"]
  }
}
```

## 生命周期与热重载

- **安装**:`zhuye plugin install <目录|plugin-id>` → 复制进 cache 并
  登记;支持 marketplace 五种源与依赖闭包安装(marketplace 侧见
  [`plugin-marketplace.md`](plugin-marketplace.md))。
- **启用 / 禁用**:权威表是 `~/.zhuye/config.toml` 的
  `[plugins].enabled_plugins`(条目为 `name@marketplace` 全名);
  `zhuye plugin enable demo@inline` 直接改写该列表。
- **挂载**:进程启动时按 enabled 列表挂载;运行中改动 config 的
  `[plugins]` 段会 diff 同步(新启用的挂载、移除的反注册),无需重启。
- **scan / register 分离**:`scan` 是纯函数(发现 → 解析 → 校验,
  无全局副作用),`register` 才碰共享 registry。每类 capability 独立
  收敛错误,一类坏了不阻断其他类;**register 是部分成功语义** ——
  失败项记录后继续,已注册项保留,不回滚。
- **反注册顺序**(确定性清理,按 plugin_id 整体挂卸):先停 MCP server
  并反注册 `mcp__*` 工具 → 按 `ToolSource::Plugin` 清工具 → 移除
  skills / commands → 取回 subagent specs → 按 `plugin:{id}:` 前缀
  清 hooks。
- **作用域**:`installed_plugins.json` 支持 Managed / User / Project /
  Local 四级 scope,同 id 多 scope 共存时按优先级取用
  (Local > Project > User > Managed)。

热重载的限制(重要):

- **`serve --plugin X` 不回写 config**:显式旗标只作用于本次启动
  装配;运行中 config 热重载仍按 config 列表同步 —— 如果 config 里
  没有 X,X 会在下次 `[plugins]` 段变动时被卸掉。
- 只有 `enabled_plugins` 列表的增删会同步;`[plugins.marketplaces]`
  段变更不触发挂卸。
- 单插件加载失败只 `tracing::warn!`,不影响其他插件与整体 reload;
  且 `config_reloaded` 事件**不含**插件错误字段 —— 失败只能从
  「缺失 `plugin_loaded` 事件 + stderr 日志」推断。

## 宿主支持矩阵

| 宿主 | 插件挂载 | slash 命令展开 |
|---|---|---|
| `zhuye exec`(普通) | ✅ 启动挂载 + 热重载 | ✅ `/plugin:ns:name args` |
| `zhuye exec --resume` / `-c` | ✅(与普通路径同权) | —(resume prompt 为合成文本) |
| `zhuye serve`(SDK 入口) | ✅(与 exec 共享装配) | ✅(单条 Text 的 UserInput;展开失败原样转发) |
| Rust 门面 `ZhuyeBuilder::build_async` | ✅(`with_plugins(None)` 按 config) | 调用方用 `ZhuYe::plugin_runtime()` + `expand_user_input` 自行展开 |
| Rust 门面 `ZhuyeBuilder::build`(同步) | ❌(保持最小语义) | ❌ |
| Python `zhuye_py.ZhuyeBuilder(...).plugins().build()` | ✅(内部走 build_async) | 调用方自行展开 |

## 安全模型

- **外部工具权限下限**:插件注册的所有工具(agents / MCP)即使声明
  `Auto` 也会被 `FloorEnforcingTool` 抬升到 `Prompt` —— 调用前需用户批准。
- **hook fail-closed**:插件 hook 是任意可执行命令,超时 / 崩溃 / 非零
  退出都按 Deny 处理,不会因插件故障放行危险操作;敏感 env 默认剔除,
  白名单显式放行。
- **MCP 进程隔离**:stdio MCP server 以干净 env 启动(只透传
  HOME / PATH 与用户显式声明的 env),不继承宿主密钥。
- **commands 无副作用**:命令展开是纯文本替换,发生在提交前、由宿主
  而非模型触发。
- 路径安全:capability 路径不允许逃出插件目录(canonicalize 后校验
  前缀);`PluginId` 严格 kebab-case 校验,`inline` / `builtin` 为保留
  marketplace 名。

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

## API 索引(`zhuye-plugin` crate)

- 数据模型:`PluginManifest` / `PluginId` / `PluginStatus` /
  `InstalledPluginsFile` / `MarketplaceManifest`
- 管理:`PluginManager`(install / uninstall / list)、`DepResolver`、
  marketplace fetcher(`MarketplaceFetcher` / `MarketplaceFetchRouter`)
- 挂载:`loader::{LoaderRegistries, register, unregister}`、
  `CommandRegistry`、`expand_user_input`
- 运行时装配:`runtime::{PluginRuntime, bootstrap_plugins, reload_plugins}`
  (exec / serve / 门面 / Python 四条路径共用)
- 错误:`errors::PluginError`(各变体场景与处理见下方「排障」)

## 排障

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
| `ChecksumMismatch` | sha256 不一致 | 换源;当前只在 load 时 warn 不阻断(见「已知限制」) |
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
加 `"env": ["NAME"]` 白名单放行(写法见「hooks」小节)。

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
| [`plugin-marketplace.md`](plugin-marketplace.md) | marketplace:manifest 格式、来源类型、发布与刷新流程 |
| [`tools-and-hooks.md`](tools-and-hooks.md) | 内置工具与 hooks 设计契约(12 事件 / 7 决策) |
| [`architecture.md`](architecture.md) | 六层架构与 `zhuye-plugin` 定位 |
| [`../plugins/README.md`](../plugins/README.md) | 仓库自带插件目录约定 |
