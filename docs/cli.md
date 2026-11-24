# CLI 参考

<p align="center">  <b>中文</b> · <a href="cli.en.md">English</a>  </p>

> `zhuye` 二进制的子命令全量一览与用法。单一 `zhuye` 二进制由
> `zhuye-cli` crate 产出,共 22 个顶层子命令,**不含 `tui` 子命令**
> (TUI 已拆至独立仓库 [Reflect-TUI](https://cnb.cool/Demon1019/Reflect-CLI)),
> **也没有 `import` 子命令**。
> 每个命令的完整参数以 `zhuye <命令> --help` 为准。

## 总览

| 命令 | 干什么 |
|------|--------|
| `exec` | 跑一轮 headless 对话就走,事件以 JSONL 写 stdout |
| `serve` | 常驻 stdio JSONL 会话服务(Python / TS SDK 的协议入口) |
| `discussion` | 按 TOML 配置文件运行多 agent 讨论 |
| `task` | 结构化任务 + 团队管理(TaskCreate / TaskList / TeamCreate 等) |
| `pipeline` | 运行 DAG 流水线(team-plan → team-prd → team-exec → team-verify) |
| `plugin` | 安装 / 列出 / 启用 / 禁用 / 卸载插件 + marketplace 管理 |
| `mcp` | 列出 / 新增 / 删除 / 测试 / 显示 MCP server 配置 |
| `lsp` | 列出 / 查看 LSP server 配置 |
| `hooks` | 内置 hook 启停管理(状态持久化在 `[hooks].disabled`) |
| `skill` | Skill 市场:从 GitHub 仓库安装技能到 `~/.zhuye/skills/` |
| `config` | 查看或编辑 `~/.zhuye/config.toml`(show / set / unset / edit / ls) |
| `session` | 列出 / 查看 / 删除持久化 session |
| `traces` | 查看本地 LLM 调用记录(`~/.zhuye/traces/model-io/`) |
| `login` | 配置 provider 凭据(写入 `~/.zhuye/config.toml`) |
| `models` | 列出 / 挑选 provider 当前可用的模型(清单实时取自接口) |
| `secrets` | 加密密钥存储(set / get / list / delete;AES-256-GCM + machine key) |
| `security` | 安全扫描(cargo audit) |
| `workspace` | workspace 的 git clone / sync |
| `doctor` | 环境自检(config / rollout / mcp / rustc;`--dump-config` 打印生效配置快照) |
| `update` | 打印升级说明(不联网) |
| `init` | 在当前目录生成 AGENTS.md 项目说明(agent 上下文锚点) |
| `version` | 打印版本号并退出 |

## exec —— headless 单轮

跑一轮就退。位置参数 `prompt` 可选(给了 `--resume` / `-c` / `-r` 时
prompt 被忽略,恢复优先)。

| 参数 | 说明 |
|------|------|
| `prompt` | 待执行的用户输入(可选) |
| `--resume <UUID>` | 按 thread id 恢复历史 session(重放 Message 与 Compaction 记录) |
| `-c` / `--continue` | 续最近一次 session |
| `-r <N>` | 按序号恢复(1-indexed,newest first) |
| `--agent <NAME>` | Agent 定义名(`~/.zhuye/agents/` 与 `{workspace}/.zhuye/agents/`,默认 `default`) |
| `--prompt-profile <PROFILE>` | 本次会话的提示词方案:`default`(烛夜默认文案)/ `auto`(按模型选已评测方案)/ 方案标识(固定用指定层,如 `claude`);等价会话内 `/prompt select`,方案库见 [prompts.md](prompts.md) |
| `--plan-mode` | 启动即进入 Plan mode(只读;与 `--full-auto` 互斥) |
| `--full-auto` / `--yolo` | 完全托管:以 `PermissionMode::Bubble` 启动,非高危工具自动批准,高危仍强制人工确认 |
| `--ephemeral-tasks` | 任务不持久化(内存 store) |
| `--ephemeral-teams` | 团队不持久化(内存 store) |
| `--auto-root` | 从 cwd 向上探测项目根(首个含 `.git` / `Cargo.toml` / `.zhuye/` 的目录) |
| `--image <PATH>` | 图片附件,可多次传入(png/jpeg/gif/webp/bmp,单张 ≤10MiB;与恢复参数互斥) |

```bash
ZHUYE_MODEL=mock ./target/debug/zhuye exec "重构这个模块"
./target/debug/zhuye exec --resume <UUID> "继续"
./target/debug/zhuye exec -c          # 续最近一次
```

## serve —— 常驻会话(SDK 协议入口)

stdin 逐行读 `Submission`,stdout 逐行写 `Event`,stderr 是日志;
多轮共享同一 session。Python / TS SDK 都在其之上封装(见
[sdk.md](sdk.md))。

| 参数 | 说明 |
|------|------|
| `--agent <NAME>` | 同 exec |
| `--resume` / `-c` / `-r` | 恢复三选一,同 exec |
| `--full-auto` / `--yolo`、`--plan-mode` | 同 exec,两者互斥 |
| `--ephemeral-tasks` / `--ephemeral-teams` / `--auto-root` | 同 exec |
| `--plugin <ID>` | 显式启用的插件 id,可多次;传入时**覆盖** config 的 `[plugins].enabled_plugins`,SDK 嵌入方据此精确加载 |

与 `exec` 的差异:serve **没有** prompt 位置参数与 `--image`(输入由
stdin Submission 驱动)。

## plugin / marketplace

`zhuye plugin install <路径或 name@marketplace>`(本地安装 ID 为
`name@inline`,支持 `--scope`)、`list`、`info`、`enable`、`disable`、
`uninstall --yes`、`show`。

`zhuye plugin marketplace` 管理插件市场:`add`(五种 source:
github / git / url / file / directory)、`ls`、`refresh`、`discover`
(按主题发现)、`remove`。

⚠️ **directory 源危险行为**:以源目录本身作为安装位置,`remove` 会对
该路径执行 `remove_dir_all`。完整使用与发布见
[plugins.md](plugins.md) 与 [plugin-marketplace.md](plugin-marketplace.md)。

## 其余子命令

- **discussion**:按 TOML 配置文件运行多 agent 讨论,见
  [features.md](features.md) 的编排章节。
- **task / pipeline**:结构化任务、团队与 DAG 流水线,持久化在
  `~/.zhuye/tasks/`、`~/.zhuye/teams/`(`--ephemeral-*` 可改用内存)。
- **mcp**:`ls` / `add`(`--command` stdio 与 `--url` http 互斥)/
  `remove` / `test` / `show`;配置在 `[mcp_servers]`。
- **lsp**:列出 / 查看 LSP server 配置。
- **hooks**:内置 hook 启停,状态持久化在 `[hooks].disabled`。
- **skill install <repo>**:从 GitHub 仓库(根含 SKILL.md)安装技能到
  用户级 `~/.zhuye/skills/`(`--dir` 可改)。
- **config**:`show` / `set` / `unset` / `edit` / `ls`,操作
  `~/.zhuye/config.toml`(改完热重载,见
  [configuration.md](configuration.md))。
- **session** / **traces**:持久化会话管理与本地 LLM 调用记录查看。
- **login**:配置 provider 凭据(`anthropic` / `openai` / `ollama`,
  `local` 是 `ollama` 别名);`--api-key` 直传、`--force` 跳过 key
  过短告警。
- **models**:模型清单实时取自 provider 接口。
- **secrets**:`set`(明文从 stdin 读,argv 不落密钥)/ `get` /
  `list` / `delete --yes`;AES-256-GCM + machine key。
- **security**:cargo audit 安全扫描。
- **workspace**:git clone / sync。
- **doctor**:显示可解析模型的上下文窗口、压缩阈值及配置来源；`--check-network` 对当前 provider 发 1 token 探测;
  `--dump-config` 打印当前生效配置的 JSON 快照(密钥只出 label)。
- **update**:`--check-only` 仅打印检查说明,不联网。
- **init**:`--force` 覆盖已存在的 AGENTS.md(默认跳过)。

## 独立二进制

- **Reflect-TUI**:独立仓库提供的 TUI 二进制,核心仓库的 `zhuye`
  不含 `tui` 子命令。

## 全局参数与环境

- `-C` / `--cwd <DIR>`:切换工作目录(所有子命令可用)。
- 常用环境变量:`ZHUYE_MODEL`(模型 spec,`mock` 为离线)、
  `ZHUYE_BIN`(SDK 二进制路径,优先级见
  [sdk.md](sdk.md))、`ZHUYE_MOCK_SCRIPT`(脚本化 mock
  回复)、`ZHUYE_HOME`(数据目录重定向)、`ZHUYE_REMOTE_TOOL_TIMEOUT_SECS`
  (远程工具回执上限,默认 120s)。完整列表见
  [configuration.md](configuration.md)。
