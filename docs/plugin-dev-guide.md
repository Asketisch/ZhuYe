# 烛夜插件开发指南(社区向)

> 本指南面向**想为烛夜开发插件的所有开发者**,不要求熟悉烛夜内部实现。
> 从零开始跑通第一个插件,再按需深入各类能力与 WASM 组件。
> 字段级参考(完整 manifest 字段表、API 索引、错误速查)见
> [`plugins.md`](plugins.md);本文聚焦「怎么做」。

---

## 0. 30 秒了解插件是什么

烛夜插件 = 一个目录,里面放一个 `plugin.toml` manifest + 若干**能力
声明**(slash 命令 / 子代理 / 技能 / hook / MCP server / WASM 组件)。
装进烛夜后,这些能力挂进引擎:

- slash 命令变成用户可输入的 `/命令`;
- 子代理变成模型可调用的工具;
- 技能进入渐进披露目录(模型按需 `load_skill`);
- hook 在工具调用等事件点上做守卫或注入;
- MCP server 的工具并入模型可见工具集;
- WASM 组件以**进程内**方式提供同样的 hook / 工具(零子进程)。

运行中启用 / 禁用插件**不需要重启**(`[plugins]` 配置段热重载)。

---

## 1. 快速开始:5 分钟跑通第一个插件

### 1.1 写 manifest 与一个命令

```bash
mkdir -p my-first-plugin/commands
cd my-first-plugin

cat > plugin.toml <<'EOF'
name = "my-first"
version = "0.1.0"
description = "我的第一个烛夜插件"
commands = "./commands"
EOF

cat > commands/hello.md <<'EOF'
---
description: 打招呼
---
你好,$ARGUMENTS!这是来自插件的消息。
EOF
```

### 1.2 安装、启用、验证

```bash
# 安装(复制到 ~/.zhuye/plugins/cache 并登记;本地目录装为 my-first@inline)
zhuye plugin install /path/to/my-first-plugin

# 启用(写进 ~/.zhuye/config.toml 的 [plugins].enabled_plugins)
zhuye plugin enable my-first@inline

# 验证
zhuye plugin list
zhuye exec "/my-first:hello 烛夜"     # → 你好,烛夜!这是来自插件的消息。
```

> **调试技巧**:安装到开发目录可以用 `zhuye plugin install ./my-first-plugin`
> 直接指向工作目录;命令**正文**在每次展开时实时读盘,改 `hello.md`
> 立即生效,无需重新安装(增删命令文件需要重挂,见 §6)。

### 1.3 热重载

会话进行中直接编辑 `~/.zhuye/config.toml` 的
`[plugins].enabled_plugins` 列表(增删 id),运行中的会话会 diff 同步
挂载 / 卸载,无需重启。CLI 的 `plugin enable` / `disable` 只改 config,
对已运行的会话同样由这条热重载链路生效。

---

## 2. 选择能力:决策表

| 你想做的事 | 用哪种能力 | 备注 |
|---|---|---|
| 给用户一个 `/命令` 快捷方式(prompt 模板) | **commands** | 最简单;也可用「用户级命令目录」免插件(§3.1) |
| 给模型一个可调用的**子代理**(独立上下文) | **agents** | 一个 md 定义角色,自动生成 `call_<角色>` 工具 |
| 给模型**按需查阅的资料 / 操作手册** | **skills** | 模型经 `load_skill` 激活,支持 `tools` 标注 |
| 在**工具调用前后做守卫 / 审计 / 注入** | **hooks** | shell 脚本或 WASM 组件;fail-closed |
| 给模型**新工具**(需要外部进程 / 网络服务) | **mcp_servers** | 任意 MCP server,stdio / HTTP |
| 给模型**新工具**(纯计算、任何语言、进程内) | **wasm_tools** | WASM 组件,确定性超时 + 沙箱(§5) |

经验法则:**能用文件解决的就别写代码**(commands / skills / agents 都是
纯声明);需要执行逻辑时,优先 shell hook 或 MCP server;追求分发简洁、
跨语言、进程内性能时上 WASM。

---

## 3. 各能力教程

### 3.1 commands —— slash 命令

**目录约定**:`commands/` 下任意层级的 `*.md`。命令名 =
`<插件名>:<相对路径段>`,如 `my-first:hello`。

```markdown
---
description: 按语言生成项目骨架
---
请为 $1 语言创建项目骨架,参数总览:$ARGUMENTS
```

- `$ARGUMENTS` = 命令名后的全部参数;`$1`..`$9` = 逐个位置参数。
- 用户输入:`/my-first:skeleton rust 附加要求`。
- frontmatter 的 `description` 显示在命令列表。

**不想写插件?** 用户级命令目录更简单:把 `<stem>.md` 放进
`<workspace>/.zhuye/commands/` 或 `~/.zhuye/commands/`,命令名就是文件
名(`/review args` 直达,无前缀)。启动挂载,支持热重扫。

### 3.2 agents —— 子代理

`agents/<名字>.md`(frontmatter + 正文 = system prompt):

```markdown
---
name: 代码评审员
description: 对当前 diff 做严格评审,输出阻塞项与建议
model: anthropic/claude-sonnet-4-5
tools: [read, grep]
---

你是资深评审。输出分两节:## 阻塞项、## 建议。
```

挂载后模型获得 `call_<角色>` 工具(role 由插件名派生,snake_case)。
`tools` 是**白名单**:子代理只能用列出的父级工具,空 = 纯文本子代理。

### 3.3 skills —— 技能

`skills/<名字>/SKILL.md`:

```markdown
---
name: zhuye-release
description: 烛夜 SDK 发版流程(双端版本联动)
tools: [bash]
---

发版步骤:…(正文在模型 load_skill 后进入其上下文)
```

- frontmatter 至少要有 `description`;`triggers`(触发关键词)、
  `tools`(激活时追加的工具)、`when_paths`(关联文件)可选。
- **SKILL.md 不合规的插件技能会降级**(只用 manifest 元数据,无
  tools/triggers)并 warn —— 发布前请自查格式。
- 用户级 skills 目录 `<workspace>/.zhuye/skills/` 与
  `~/.zhuye/skills/` 无需插件,直接放目录。

### 3.4 hooks —— 事件守卫

`hooks.json`(Claude Code 风格):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "bash",
        "hooks": [
          { "type": "command", "command": "./guard.sh", "timeout": 10,
            "env": ["MY_POLICY_URL"] }
        ]
      }
    ]
  }
}
```

**执行协议**(权威定义:`crates/abilities/zhuye-hooks/src/shell.rs`):

- 事件 JSON(含 `kind` / 工具名 / 参数 / 结果)写入子进程 **stdin**;
- 子进程 **stdout** 返回决策 JSON,`decision` 字段:

| decision | 效果 |
|---|---|
| `allow` / 空输出 / 非 JSON(exit 0) | 放行(信息性输出不阻断) |
| `deny`(别名 `block`) | 拒绝,`reason` 展示给用户 |
| `ask` | 转人工确认,`reason` 作为提问 |
| `inject`(别名 `message`) | 注入系统提醒(`reason` 即内容) |

- **fail-closed**:超时 / 非零退出 → Deny(守卫不可用宁可拒绝)。
- `matcher` 仅对 `PreToolUse` / `PostToolUse` 生效,支持 `*` / `?` 通配。
- **环境变量卫生**:默认剔除名称含 secret 模式的 env
  (`*_TOKEN` / `*_API_KEY` 等);确需放行用 `env` 显式列名。

### 3.5 mcp_servers —— MCP server

`.mcp.json`(stdio)或内联声明(HTTP):

```json
{
  "my-tools": {
    "command": "${PLUGIN_ROOT}/bin/my-server",
    "args": ["--mode", "strict"],
    "timeout": 60
  }
}
```

- 工具以 `mcp__plugin:<插件id>:<server>__<工具>` 命名注入模型可见集。
- **权限下限**:MCP 工具即使声明 `Auto` 也被抬到 `Prompt`(调用前需批准)。
- **进程隔离**:子进程只拿到 HOME / PATH 与显式声明的 env,不继承密钥。
- 运行中修改 `[mcp_servers]` 配置段支持字段级热重载(只重启真变化的
  server)。

---

## 4. WASM 组件(多语言插件)

WASM 是面向社区的**进程内插件形态**:任何语言按一份契约产出单个
`.wasm` 文件,宿主进程内实例化 —— 零子进程、零 JSON-RPC 样板、确定性
超时、无文件系统 / 无网络 / 无环境变量的纯计算沙箱。

### 4.1 两种世界

`wit/plugin.wit`(`package zhuye:plugin@0.1.0`)定义两个世界,组件按需
实现其一:

- **`zhuye-hook`**:`handle(event) -> result<string, string>` —— 事件
  JSON 进,决策 JSON 出(`decision` 协议与 shell hook 完全一致,§3.4);
- **`zhuye-tool`**:`describe() -> string`(返回工具声明 JSON)+ 
  `invoke(args) -> result<string, string>`(返回协议层 ToolOutput 完整
  wire 形态:`content` / `is_error` / `metadata` / `elapsed_ms` 缺一不可)。

ABI 刻意选 **JSON 字符串**:动态语言作者无需生成强类型绑定。

### 4.2 Rust 最小组件

```
my-wasm-plugin/
├── Cargo.toml          # 独立项目(加 [workspace] 空表防误入主 workspace)
└── src/lib.rs
```

```toml
[package]
name = "my-hook"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = "0.40"

[workspace]
```

```rust
wit_bindgen::generate!({
    // path 相对本文件到仓库根的 wit/ 目录;分发时拷贝一份同结构即可
    path: "path/to/wit",
    world: "zhuye:plugin/zhuye-hook@0.1.0",
});

struct MyHook;

impl Guest for MyHook {
    fn handle(event: String) -> Result<String, String> {
        Ok("{\"decision\":\"deny\",\"reason\":\"周末禁止执行写操作\"}".to_string())
    }
}

export!(MyHook);
```

构建与声明:

```bash
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
# 产物 target/wasm32-wasip2/release/my-hook.wasm → 放进插件 bin/
```

```toml
# plugin.toml
[[wasm_tools]]
path = "bin/my-tool.wasm"
name = "my_tool"
```

```json
// hooks.json 条目(command 对象里用 wasm 字段替代 command)
{ "type": "wasm", "wasm": "${PLUGIN_ROOT}/bin/my-hook.wasm", "timeout": 10 }
```

### 4.3 其他语言

| 语言 | 工具链 | 状态 |
|---|---|---|
| Rust | wit-bindgen 0.40(锁定,见下) | 生产可用(本仓样例) |
| TypeScript / JS | jco(componentize-js) | 路线图 |
| Python | componentize-py | 路线图 |
| Go | tinygo + wasm-tools | 实验性 |

契约语言无关 —— 你的语言只要能产出组件模型 `.wasm` 即可参与;
`wit/README.md` 有版本策略详情。

### 4.4 安全与超时(务必理解)

- **确定性超时**:每次调用默认 30s(epoch 中断强制截断),hooks.json
  `timeout` 可覆盖;超时 → hook **Deny** / tool `is_error`(fail-closed)。
- **沙箱**:组件拿不到文件系统 / 网络 / 环境变量(纯计算;要 IO 请用
  MCP server 或 shell hook)。
- **权限**:WASM 工具与 MCP 工具同基线 —— 声明 `Auto` 不生效,一律
  `Prompt`(调用前需用户批准)。
- **决策 fail-closed**:组件返回 `Err` / 非法 JSON / 未知 `decision`
  → Deny。
- **工具链矩阵锁定**:宿主 wasmtime 30.0.1 ↔ guest wit-bindgen 0.40。
  组件模型的 WASI 接口小版本要求精确匹配,升级必须宿主与组件同步
  (见 `wit/README.md`)。

---

## 5. 调试

### 5.1 插件没生效?

按顺序检查:

1. `zhuye plugin list` —— id 是否在列、`enabled` 是否为 true;
2. `~/.zhuye/config.toml` 的 `[plugins].enabled_plugins` 是否含
   `name@marketplace` 全名;
3. stderr 找 `plugin` 相关 warn(单类能力失败不阻断其他类);
4. `plugin.toml` 的 `name` 必须 = 目录名;路径声明必须存在
   (预留路径 warn 不阻断);
5. WASM:`scripts/build_plugin_demo_wasm.sh` 的产物在
   `plugins/plugin-demo/bin/`;加载失败会以 RegisterError 显式报错。

### 5.2 常见错误速查

| 现象 | 原因 |
|---|---|
| 命令输入 `/demo:hello` 无展开 | 命令名漏了插件前缀;或插件未启用 |
| hook 每次都 Deny 但脚本没毛病 | 非 JSON / 非零退出 fail-closed;先手跑脚本核对输出与退出码 |
| MCP 工具没出现 | server 启动失败(stdio 初始化超时 / 命令不存在),看 stderr warn |
| WASM 工具加载失败 | 非 zhuye 世界 / 工具链矩阵不匹配 / 文件损坏;挂载期显式报错 |
| SKILL.md 被降级 | frontmatter 缺 `name` / `description` |
| rescan 后内置技能消失 | 升级到含 bundled 补挂的版本(见 CHANGELOG) |

### 5.3 本地开发工作流

```bash
# 安装指向工作目录,命令正文实时读盘
zhuye plugin install ./my-plugin
# hooks.json / plugin.toml 变更 → 重新 install(或直接改
# ~/.zhuye/plugins/cache/inline/<name>/<ver>/ 内的文件)+ enable
```

---

## 6. 发布到 Marketplace

marketplace = 一个仓库里的 `.claude-plugin/marketplace.json`(JSON),
列出多个插件条目与来源:

```json
{
  "name": "my-tools",
  "owner": { "name": "me", "url": "https://example.com" },
  "plugins": [
    {
      "name": "my-formatter",
      "description": "格式化插件",
      "source": { "source": "github", "repo": "me/my-formatter", "ref": "v1" },
      "category": "devtools"
    }
  ]
}
```

用户侧:

```bash
zhuye plugin marketplace add my-tools --from github --repo me/my-tools
zhuye plugin install my-formatter@my-tools
zhuye plugin enable my-formatter@my-tools
```

来源支持 `github` / `git` / `url` / `file` / `directory` 五种;字段级
说明见 [`plugin-marketplace.md`](plugin-marketplace.md)。

---

## 7. 测试建议(插件作者)

插件仓库的 Rust 部分建议:

- 纯函数(参数替换、状态机、解析)用 **property-based 测试**
  (`proptest`)探索手写用例盲区 —— 本仓 `zhuye-protocol` / `zhuye-tools`
  / `zhuye-compact` 各有现成范例(`mod proptests`);
- 覆盖率用 `cargo llvm-cov --summary-only`(基线 80%+);
- hook 决策 JSON 请按 §3.4 的字段表写**正反用例**(缺 `decision` →
  fail-closed 是协议行为,不是 bug)。

## 8. 版本兼容须知

- **wire 协议 v0 冻结**:引擎与 SDK / 客户端之间只做 additive 演进,
  旧插件与旧客户端不因新增能力破坏。
- **插件 manifest 向后兼容**:旧版烛夜对新字段(如 `[[wasm_tools]]`)
  静默忽略;你的插件在新旧版本上都安全。
- **WASM 工具链矩阵锁定**(wasmtime ↔ wit-bindgen,见 `wit/README.md`):
  宿主升级会同步声明,组件按锁定版本构建即可。
- **`user_config`** 字段已定义、运行时尚未消费 —— 声明不影响行为,
  请勿依赖。

---

## 9. 参考与样例

- 参考手册(字段级):[`plugins.md`](plugins.md)
- marketplace 发布:[`plugin-marketplace.md`](plugin-marketplace.md)
- 完整样例插件:`plugins/plugin-demo/`(五类目录能力 + WASM 组件,
  `scripts/build_plugin_demo_wasm.sh` 一键构建)
- WASM 契约:`wit/plugin.wit` + [`wit/README.md`](../wit/README.md)
- hook 决策协议权威:`crates/abilities/zhuye-hooks/src/shell.rs`
