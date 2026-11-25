# 能力总览

<p align="center">  <b>中文</b> · <a href="features.en.md">English</a>  </p>

> 本文是烛夜(ZhuYe)的能力说明与已知边界,按「默认启用 / 需配置 / 需插件」
> 逐条标注。快速上手见根目录 [README](../README.md);引擎内部怎么搭见
> [architecture.md](architecture.md);工具与 hook 的字段级规范见
> [tools-and-hooks.md](tools-and-hooks.md)。

## 嵌入的三种用法(默认)

同一套 Rust 引擎有三种用法,全部**默认可用**,选哪种取决于你的宿主:

| 用法 | 入口 | 说明 |
|------|------|------|
| 进程内链接 | `zhuye` 库门面 | `Zhuye::builder(model).workspace(".").build()?` + `Submission::user_input`,在宿主进程里直接驱动,适合把 agent 当模块嵌入 |
| headless 单轮 | `zhuye exec` | 跑一轮就退出;事件 JSONL 走 stdout、tracing 走 stderr,`zhuye exec \| jq` 不被日志污染 |
| 常驻会话 | `zhuye serve` | stdio JSONL 协议入口;Python / TypeScript SDK 都构建在它上面,支持多轮共享状态、远程工具回执、审批等交互 |

离线开发时,examples 与 e2e 用 `ZHUYE_MODEL=mock` 跳过真实 LLM 网络调用,
不配 API key 也能跑。

## 工具留在你自己的进程里(默认)

你在 Python 或 TypeScript 里写的函数不用改写成 Rust。注册一个名字和参数
格式,模型要用的时候引擎把请求经 serve 协议发回你的进程,函数跑完把结果回
执;内置 / MCP / 远程工具都走同一个 `ToolExecutionQueue`。

## 核心内置工具(默认)

引擎自带 **25 个无条件核心内置工具 + 2 个门控插件工具**(`zhuye-tools`
的单一注册清单,exec / serve / lib 门面三条路径共用):

无条件注册(25):

`echo`、`bash`、`background_status`、`read`、`write`、`edit`、`delete_file`、
`grep`、`glob`、`EnterPlanMode`、`ExitPlanMode`、`PlanWrite`、
`EnterWorktree`、`ExitWorktree`、`ast`、`web_fetch`、
`ask_user_question`、`ask_user`、`request_human_input`、`tool_search`、
`image_view`、`brief`、`get_context_remaining`、
`checkpoint`、`rewind`。

门控插件工具(默认开,可整体禁用,见下方「三层工具管控」):

- `web_search` —— `web-search` 插件(Brave Search API;`[web_search]` 段配
  key,审批 Auto);
- `notebook_edit` —— `notebook` 插件(Jupyter cell 级编辑,与
  read_before_edit hook 联动)。

注意:这是**核心注册清单,不是全集** —— 编排、cron、插件可再注册其他工具。
发给 LLM 的工具 schema 会经 PreLoop 裁剪(见 architecture.md §5),未注册
或被裁剪的工具不进 prompt。`bash` 默认跑在系统沙箱里(见「安全闸门」)。

## 三层工具管控

工具的「关」有三个正交入口,粒度从粗到细:

| 层 | 入口 | 效果 |
|---|---|---|
| 装配期·注册门控 | `[plugins].disabled_plugins`(插件级) | 工具**不进注册表**:`tool_search` 搜不到、调用 tool not found;默认开的门控插件(web-search / notebook)关闭走这里 |
| 定义期·可见性过滤 | `[active] tools` / `[active].disallowed_tools`(工具名级) | 工具已注册,仅 **LLM 不可见**(白名单 ∩ / 黑名单 ∪,可与 agent 文件 frontmatter 组合) |
| 执行期·审批 | 权限规则 / floor(操作级) | 决定能否执行;`bash` 默认 Prompt,`web_search` 默认 Auto |

门控插件的启用判定与默认开关表:`zhuye-plugin` 的
`BUNDLED_PLUGIN_DEFAULTS` / `bundled_plugin_enabled`(显式禁用 > 显式启用
> 默认开关)。判定不热重载,变更后重启会话生效。

## 形态判断结论(不插件化清单)

按根 AGENTS.md「形态判断(主循环 vs 插件)」铁律逐项过审后,**保留内置**
的能力与理由:

- 文件族 / bash 族 / Plan mode 三件 / ask_user 族 / `tool_search` /
  `get_context_remaining` —— 主循环必需(StateGraph 流转、审批 side-channel、
  上下文治理直接依赖);
- `checkpoint` / `rewind` / worktree 工具 —— 依赖引擎 recorder 与 rollout
  截断(经 ToolContext 的引擎内部状态),且 workspace 侧快照已由 git-tools
  插件提供,内置版承载「会话回退」语义,二者不混;
- `brief` / `image_view` —— 纯本地通用便利工具(`image_view` 返回多模态
  ContentBlock,插件工具输出无法承载);
- `ast` —— tree-sitter 重依赖,后续按 feature-gate 演进而非插件化;
- 厂商 quota provider(Kimi / Zhipu / Minimax / Zenmux)—— ModelRegistry
  进程内机制,SDK 门面 re-export 依赖;
- `langfuse_tracker` hook —— 插件 hook 槽(shell / WASM)无法承载
  OTLP/HTTP 上报,需先扩展插件 hook 形态(列路线图);
- `zhuye-eval` —— 独立评测工具(`zhuye-eval-score` 二进制),非 agent
  运行时能力,不插件化。

## 多 LLM provider(默认)

原生实现 **Anthropic、OpenAI、Ollama** 三个 provider,外加离线 `mock`(不配
key 也能跑)与 OpenAI Responses 实现。OpenAI 兼容接口可以服务其他厂商
(通过 `base_url` 指向任意兼容网关),不必为每家写独立原生客户端。每家模型
的提示词按「默认 → 厂商 → 具体模型」分层叠好,换模型自动换用适合它的那套
文案;还能配一条 fallback 候选链,某个 provider 挂了自动切下一个。

## 编排五原语(默认)

多 agent 编排是引擎自带的能力,不是插件。五个原语各自独立成 crate:

| 原语 | crate | 说明 |
|------|-------|------|
| 子代理 | `zhuye-subagent` | 子代理工厂,并发在途 ≤16;可用独立模型 registry |
| 角色讨论 | `zhuye-discussion` | 多角色讨论编排,消息总线 + 收敛判定 |
| 任务/团队 | `zhuye-task` | 声明式任务、团队 / squad 委派 |
| 流水线 | `zhuye-pipeline` | DAG 流水线;`Abort` / `ContinueCollect` 失败策略 |
| 目标模式 | `zhuye-goal` | `EnterGoalMode` 后每轮 `on_turn_end` 自校验 + 续作 |

子代理的 `call_<role>` 支持两种派发模式:`mode="sync"`(默认)父回合阻塞
等子代理结束并直接拿到结果;`mode="async"` 立即返回占位回执,子代理在后台
继续跑 —— 父 agent 期间可以推进别的事,等「没有别的任务」时回合**挂起**
等待子代理;子代理结束后其结果被注入回本轮上下文(与工具回执走同一套
上下文预算,并写入会话记录供后续回合查看),主 agent 在**同一个回合内**
被激活续跑。异步子代理不跨越派发它的回合:打断父回合会一并取消在途的
异步子代理,与同步模式一致。

side-channel 让用户发起的并发 agent 与主轮次并行互不干扰(由 `zhuye-core`
承接)。

## 上下文管理(默认)

快到上下文上限时先在本地裁剪(逐级 escalation),实在压不下去才调用摘要
模型重写一遍;压缩完再把还活跃的文件读回来。相关原语:

- **压缩 escalation**(`zhuye-compact`)—— 按输入 token 阈值逐级升级策略;
- **跨会话记忆**(`zhuye-memory`)—— `memory` 工具让模型把经验写进跨会话
  记忆,按 scope 隔离;
- **skills 目录**(`zhuye-skills`)—— 可复用的技能包;
- **会话笔记**(`zhuye-notes`)—— 每轮 `pre_loop` 注入,`model_call` 追加进
  消息;
- **BM25 相关性排序**(`zhuye-bm25`)—— 本地检索排序,不依赖模型;
- **提示词库外置**(`zhuye-prompt`)—— 提示词按模型匹配外置,见
  [docs/prompts.md](prompts.md)。

## Prompt cache 保温(需配置)

空闲期 / 回合内以一条廉价重放请求(`max_tokens=1`)刷新 provider prompt
cache 的 TTL,让交互间隙(读输出、跑验证)之后的第一条真实消息命中缓存。
决策是纯函数(`zhuye-llm::cache_warming`,与 pi 同构):期望节省
`= 延续概率 × miss成本 − 保温成本 ≥ 阈值` 才发,ZhuYe 用**真实命中回执**
(`cached_tokens` 观测)做门控与成本精化。默认关闭 —— 保温是引擎自发
花钱的行为,`[cache_warming] enabled = true` 显式开启。调度在
`zhuye-core::cache_warming`:同凭据重放、代际失效(rewind / compact /
model / effort / tools / prompt 切换任一变更即弃)、零污染不变式(不写
rollout、不进 session_usage、不发 TokenCount、不喂 CacheBreakDetector)。
状态经 `cache_warming` 事件可见,`warm_cached_tokens` 可与后续
`token_count.cached_tokens` 对照验证收益。详见
[docs/configuration.md](configuration.md) 的 `[cache_warming]` 段。

## 可靠性与可观测性增强(v1.x)

- **流锚点恢复** —— 可重试流错误(断流 / 网络 / 5xx)已有可见增量时,
  从安全锚点重开而非整段重来;丢弃的尾部以 `StreamRecoveryDiscarded`
  留痕(resume 不回放),恢复过程经 `stream_recovery_*` 四事件可观察;
- **压缩 rapid-refill 熔断** —— 只有校准水位降到触发阈值以下才登记锚点。「压缩 → 立即填满」连续 3 次即收口
  (`compact_rapid_refill`),窗口内前几次跳过并注入收敛提醒;
- **空结果防护** —— provider 自证零产出的正常结束判为可疑
  (`suspicious_empty_model_result`),details 持久化原始归因;
- **工具结果预算** —— 宿主接收原始输出,模型上下文超限时按配置截断或在工作区落盘供读回;
- **stop_turn 级联** —— 工具可请求结算后收口当前 turn;
- **输入守恒** —— 每个进入引擎的 turn 终态显式落盘
  (`InputDisposition`),崩溃窗口在 resume 时显式告知,**永不自动重放**;
- **TTFT 拆解** —— telemetry 记录 prepare / first_byte 毫秒;
- **models.dev 目录后台刷新** —— exec/serve 启动按 24h TTL 后台刷新
  (mock / `ZHUYE_MODELS_DEV_REFRESH=0` 跳过)。

## 安全闸门(默认)

三层闸门叠加,默认全部生效:

- **权限模式**(`zhuye-permissions`)—— 决定哪些操作要问、哪些直接放行、
  哪些直接禁止;`bash` 默认 Prompt 权限,`web_search` 默认 Auto;
- **hooks**(`zhuye-hooks`)—— 12 类事件、7 种决策,能在工具执行前后改参数、
  拦调用、插话、否决 turn 完成;
- **OS 沙箱**(`zhuye-sandbox`)—— macOS 用 Seatbelt、Linux 用 Landlock
  限写和 seccomp 限网;Linux 沙箱启用时,限制无法安装即失败关闭。
  macOS 断网仍放行本机与 Unix socket;Linux 默认阻止创建和连接套接字。

另有 **prompt injection guard**(F01 起默认开启)与**输出脱敏**
(`zhuye-sanitize`,见 [docs/sanitize.md](sanitize.md)),对工具输出做密钥脱
敏。**审计**(`zhuye-audit`)与**密钥加密存储**(`zhuye-secrets`,AES-256-GCM
+ machine key)提供不可绕过的审计记录与密钥落盘保护。注意:审计可按
`[telemetry].activity` 配置关闭,不是无条件每轮必落。字段级说明见
[tools-and-hooks.md](tools-and-hooks.md)。

## 集成(需配置)

- **MCP 客户端**(`zhuye-mcp`)—— stdio / streamable-http 两种传输;
  http 传输支持客户端侧 OAuth(`[mcp_servers.<name>.oauth]`:静态 token /
  `token_env` / 客户端凭据 / refresh token,token 端点缺省时按受保护资源
  元数据自动发现),服务端要求授权而无可用凭据时明确上报「需要授权」;
  服务端增删工具( `notifications/tools/list_changed`)时工具注册表同步刷新;
- **LSP**(`zhuye-lsp`)—— 语言服务器集成;激活由 `[lsp_servers]` 配置驱动
  (语义不变),另有 `lsp` 门控插件承载**粗粒度开关**(显式禁用即整体
  关闭,见 [plugins/lsp/](../plugins/lsp/));
- **computer-use**(`zhuye-computer-use`)—— 截屏 / 键鼠,`real` feature
  门控(默认 mock,真实操作需显式启用 feature);
- **git 能力族**(`zhuye-git` / `zhuye-git-tools`)—— checkpoint / rewind(打
  tag 的快照,回退之后依然可达)+ 分支 / PR 探测;
- **WASM 组件插件宿主**(`zhuye-wasm`)—— Wasmtime 组件模型,多语言插件运行
  时;30s deadline,无 fs / 网络 / env 继承;
- **校验双 hook**(`verification` / `test_runner`)—— 默认关;启用走双路径
  (等价):config `[hooks] enabled` 显式列出,或安装启用 `verify-hooks`
  门控插件(见 [plugins/verify-hooks/](../plugins/verify-hooks/))。

## 持久化与恢复(默认)

全过程写成 JSONL rollout(`~/.zhuye/sessions/YYYY/MM/DD/<thread>.jsonl`),支
持按 ID、最近会话或序号恢复:`--resume <uuid>` / `-c`(续最近一次)/ `-r N`
(按序号,1-indexed newest first),三者互斥。可回看、接着跑、分叉出新分
支、跨会话搜索,还能导出成带用量统计的 Markdown。

## 常驻 shell 会话池(需配置)

`zhuye-shell` 维护一组常驻 shell 会话,避免每条 `bash` 调用都新建进程,适
合频繁短命令的场景。

## cron / autopilot(需配置)

`zhuye-stream` 提供真实定时调度(cron,5 字段表达式解析)与 autopilot(自动
推进);任务跨重启持久化。仅在 serve 上配置 cron 表达式 / autopilot 策略后
生效。

## Plan Mode(默认)

进入 Plan mode 后 `write` / `edit` 被强制移除,LLM 只能用路径受限的
`PlanWrite`;审批经 `PlanApprovalGate` 与会话级扇出。设计见
[docs/PLAN_MODE.md](PLAN_MODE.md)。

## 边界与限制

- 这个仓库的二进制没有 TUI 子命令。交互界面在
  [Reflect-TUI](https://cnb.cool/Demon1019/Reflect-CLI) 仓库。
- Linux 的 Landlock 当前不限制敏感读取；**可写根内的嵌套敏感路径也无法
  强制只读** —— Landlock 的规则按位合并,父级可写根授予的写权会传播到
  其下的嵌套路径,内核没有按路径撤销(deny)规则的能力(已在 Linux
  容器实测:`allowed_access = 0` 的规则被内核以 `ENOMSG` 拒绝,嵌套规则
  与父规则按位或)。因此这类路径在 Linux 上以「报告 + 按需失败关闭」
  处理:bash 输出的 `metadata.path_protection` 逐条列出已强制与无法
  强制的路径;宿主要求某路径必须只读时(`OsSandbox::with_protected_paths`
  + `strict_paths`,或 `ZHUYE_SANDBOX_STRICT_PATHS=1`),Linux 会因无法
  强制而拒绝执行,而不是假装保护已生效。macOS Seatbelt 支持 deny 规则,
  可写根内的 `.git` / `.agents` / `.aws` 真正只读。
- 默认 seccomp 禁止创建和连接套接字,但允许供子进程内部通信的
  `socketpair`。断网过滤当前支持 x86_64 / aarch64,并要求 Landlock ABI v3；
  不支持或无法安装时拒绝执行。
- **Windows 属未验证平台**:Job Object(进程树 kill-on-close)+ AppContainer
  后端已实现,但尚未接入 bash 的 spawn 路径 —— Windows 上命令当前**不受**
  文件 / 网络约束,且这些行为**未在真机验证**。`status_line()` 与工具
  metadata 如实报告这一点;需要强隔离的平台请用 macOS / Linux。
- 同一个会话里同时在途的子代理最多 16 个,子任务结束就释放名额。
- 协议 v0 已冻结:新增 `Op` / `EventMsg` 变体属 non-breaking,但**不可改动
  既有变体**;客户端必须容忍未知 `"type"`(详见
  [sdks/PROTOCOL.md](../sdks/PROTOCOL.md))。
