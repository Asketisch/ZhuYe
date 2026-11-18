# 能力总览

<p align="center">  <b>中文</b> · [English](features.en.md)  </p>

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

引擎自带 **27 个核心内置工具**(`zhuye-tools` 的单一注册清单,exec / serve /
lib 门面三条路径共用):

`echo`、`bash`、`background_status`、`read`、`write`、`edit`、`delete_file`、
`grep`、`glob`、`EnterPlanMode`、`ExitPlanMode`、`PlanWrite`、
`EnterWorktree`、`ExitWorktree`、`ast`、`web_fetch`、`web_search`、
`ask_user_question`、`ask_user`、`request_human_input`、`tool_search`、
`image_view`、`notebook_edit`、`brief`、`get_context_remaining`、
`checkpoint`、`rewind`。

注意:这是**核心注册清单,不是全集** —— 编排、cron、插件可再注册其他工具。
发给 LLM 的工具 schema 会经 PreLoop 裁剪(见 architecture.md §5),未注册
或被裁剪的工具不进 prompt。`bash` 默认跑在系统沙箱里(见「安全闸门」)。

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

## 安全闸门(默认)

三层闸门叠加,默认全部生效:

- **权限模式**(`zhuye-permissions`)—— 决定哪些操作要问、哪些直接放行、
  哪些直接禁止;`bash` 默认 Prompt 权限,`web_search` 默认 Auto;
- **hooks**(`zhuye-hooks`)—— 12 类事件、7 种决策,能在工具执行前后改参数、
  拦调用、插话、否决 turn 完成;
- **OS 沙箱**(`zhuye-sandbox`)—— macOS 用 Seatbelt、Linux 用 Landlock;沙箱
  起不来就直接失败,**不会偷偷退回裸 shell**。macOS 沙箱默认断网,Linux 只
  限制写、不切网络。

另有 **prompt injection guard**(F01 起默认开启)与**输出脱敏**
(`zhuye-sanitize`,见 [docs/sanitize.md](sanitize.md)),对工具输出做密钥脱
敏。**审计**(`zhuye-audit`)与**密钥加密存储**(`zhuye-secrets`,AES-256-GCM
+ machine key)提供不可绕过的审计记录与密钥落盘保护。注意:审计可按
`[telemetry].activity` 配置关闭,不是无条件每轮必落。字段级说明见
[tools-and-hooks.md](tools-and-hooks.md)。

## 集成(需配置)

- **MCP 客户端**(`zhuye-mcp`)—— stdio / streamable-http 两种传输;
- **LSP**(`zhuye-lsp`)—— 语言服务器集成;
- **computer-use**(`zhuye-computer-use`)—— 截屏 / 键鼠,`real` feature
  门控(默认 mock,真实操作需显式启用 feature);
- **git 能力族**(`zhuye-git` / `zhuye-git-tools`)—— checkpoint / rewind(打
  tag 的快照,回退之后依然可达)+ 分支 / PR 探测;
- **WASM 组件插件宿主**(`zhuye-wasm`)—— Wasmtime 组件模型,多语言插件运行
  时;30s deadline,无 fs / 网络 / env 继承。

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
- 沙箱在 macOS 上默认断网,Linux 上只限制写、不切网络。
- 同一个会话里同时在途的子代理最多 16 个,子任务结束就释放名额。
- 协议 v0 已冻结:新增 `Op` / `EventMsg` 变体属 non-breaking,但**不可改动
  既有变体**;客户端必须容忍未知 `"type"`(详见
  [sdks/PROTOCOL.md](../sdks/PROTOCOL.md))。
