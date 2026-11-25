## 项目概览

烛夜（ZhuYe）— Rust 编写的 AI agent 运行时**框架**:4 节点 StateGraph 引擎、内置工具 + hooks、
多 LLM provider、MCP/LSP 集成、JSONL rollout 持久化。本仓库是**纯框架层**:
主接口为 `zhuye` 库门面(Builder + 130+ re-exports)与 Rust / Python 集成入口;
`zhuye-cli` 只是附带产出的薄 headless CLI 入口(单一 `zhuye` 二进制,
**不含** `tui` 子命令)。

- Rust stable,edition 2024,MSRV 1.89(安全依赖面要求)
- 47 crate workspace,按 6 层分组于 `crates/<layer>/<crate>/`

## AGENTS.md 体系导航

本体系按**渐进式披露**组织:根篇常驻上下文,层篇与 crate 篇按需展开,系统级说明在 `docs/`。
**修改任何 crate / 顶层目录前,先读该目录(或其所在层)的 AGENTS.md。**

| 想了解什么 | 读哪个 AGENTS.md |
|---|---|
| 协议契约:Op / EventMsg / Item / Submission | [crates/protocol/](crates/protocol/AGENTS.md) · [zhuye-protocol/](crates/protocol/zhuye-protocol/AGENTS.md) |
| 能力原语:模型 / 工具 / hook / 提示词 / 检索 | [crates/abilities/](crates/abilities/AGENTS.md) · [zhuye-llm/](crates/abilities/zhuye-llm/AGENTS.md) · [zhuye-tools/](crates/abilities/zhuye-tools/AGENTS.md) · [zhuye-hooks/](crates/abilities/zhuye-hooks/AGENTS.md) |
| 资源层:配置 / 权限 / 插件 / 持久化 / 文件 | [crates/resources/](crates/resources/AGENTS.md) · [zhuye-plugin/](crates/resources/zhuye-plugin/AGENTS.md) |
| 多 agent 编排:子代理 / 讨论 / 任务 / 流水线 / 目标 | [crates/orchestration/](crates/orchestration/AGENTS.md) |
| 外部集成:MCP / LSP / computer-use / browser / shell / git / workflow | [crates/integrations/](crates/integrations/AGENTS.md) · [zhuye-workflow/](crates/integrations/zhuye-workflow/AGENTS.md) |
| 运行时与入口:StateGraph / headless / CLI / facade / PyO3 | [crates/runtime/](crates/runtime/AGENTS.md) · [zhuye-core/](crates/runtime/zhuye-core/AGENTS.md) · [zhuye-exec/](crates/runtime/zhuye-exec/AGENTS.md) · [zhuye-cli/](crates/runtime/zhuye-cli/AGENTS.md) · [zhuye-workflow-server/](crates/runtime/zhuye-workflow-server/AGENTS.md) |
| 构建 · 发布 · 冒烟 · 插件打包脚本 | [scripts/](scripts/AGENTS.md) |
| 双 SDK:Python / TypeScript | [sdks/](sdks/AGENTS.md) |
| 测试分层与覆盖矩阵 | [tests/](tests/AGENTS.md) |
| 插件与 WASM 宿主 | [plugins/README.md](plugins/README.md)(外链) |
| WIT 组件接口 | [wit/README.md](wit/README.md)(外链) |
| 架构与系统说明 | [docs/architecture.md](docs/architecture.md) · [README 文档表](README.md)(外链) |

## 常用命令

```bash
# 构建(dev / release)
cargo build -p zhuye-cli                # 产出 target/debug/zhuye
make build                                # release 等价:cargo build --release -p zhuye-cli

# 类型检查
cargo check --workspace                   # 或 make check
make check-fast C=zhuye-core            # 单 crate 快查

# 测试
cargo test --workspace                    # 全量(CI 用 --all-targets)
make test-fast                            # cargo nextest(需已安装 cargo-nextest)
make test-changed                         # nextest 只跑自 HEAD 起受变更影响的测试
cargo test -p zhuye-core <test_name>    # 单个测试
cargo test -p zhuye-tools -p zhuye-hooks    # 多 crate
./tests/run_all.sh functional             # 功能覆盖门禁(能力覆盖层 + Rust 参数面 + 双 SDK 绑定层)
./tests/run_all.sh functional-layer       # 只跑能力覆盖层(Python 打真二进制,原始 wire)

# Lint / 格式(CI 强制,-D warnings)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all

# 文档
cargo doc --no-deps

# 端到端 / 基准
./scripts/e2e.sh                          # 13 步冒烟:构建+examples+headless+test+clippy
./scripts/bench.sh                        # 性能基线(冷启动/首事件/RSS)
./scripts/discussion_smoke.sh             # discussion 编排冒烟

# 打包分发 / 磁盘治理
make dist                                 # DMG / tar.gz / zip 到 dist/
make gc                                   # 清整个 target/ 中 cargo 不 GC 的中间产物(含 release-fast/交叉 target 的 incremental;曾膨胀 30+GB)

# SDK 发布(PyPI / npm,详见 docs/release.md)
./scripts/release_sdk.sh check            # 版本一致性 + 双包构建检查(dry-run,不发布)
```

离线开发:examples 与 e2e 用 `ZHUYE_MODEL=mock` 跳过真实 LLM 网络调用。

## 架构

### Workspace 分层(依赖方向自上而下)

| 层 | Crate | 职责 |
|----|-------|------|
| `protocol/` | zhuye-protocol | Submission / Op / EventMsg / Item + RolloutRecorder trait。所有层的公共契约 |
| `abilities/` | llm, tools, bm25, hooks, skills, memory, agent-def, prompt, compact, recovery, notes, sanitize, eval | 能力原语:ModelClient trait + providers、Tool trait + 内置工具、BM25 检索、HookEngine、压缩 escalation、评测等 |
| `resources/` | config, permissions, plugin, rollout, telemetry, sandbox, audit, secrets, ast, fs, git, wasm | 配置热重载、权限规则、插件 lifecycle、JSONL 持久化、tree-sitter 代码搜索、审计、加密密钥存储、共享文件原语、git 能力族、WASM 组件插件宿主 |
| `orchestration/` | subagent, discussion, task, pipeline, goal | 多 agent 编排:子代理工厂(并发在途≤16)、讨论编排、任务/团队/squad、DAG 流水线、目标模式、side-channel(zhuye-core) |
| `integrations/` | mcp, lsp, integration, stream, computer-use, browser, shell, git-tools, kms, workflow | MCP 客户端(stdio/streamable-http)、LSP、流式会话后端(含 cron / autopilot)、Computer Use、常驻 shell 会话池、git 工作流 MCP server、本地知识库 wiki、工作流队列(插件能力核心不进 SDK facade) |
| `runtime/` | core, exec, cli, zhuye, py, zhuye-workflow-server | AgentThread + StateGraph、headless、CLI 入口、lib facade(百余个 re-exports + Builder)、PyO3 绑定、工作流插件运行时 |

### 核心数据流

- `AgentThread`(zhuye-core)消费 `Submission`(mpsc 通道),`submission_loop` 驱动
  4 节点 `StateGraph`:**PreLoop → ModelCall → (ToolExec → PreLoop)\* → CheckStop**。
  模型不再发工具调用时经 CheckStop 结束 turn(Stop hook 可否决强制续跑)。
- `NodeContext` 携带回合级依赖:model registry、RoutingPolicy、HookEngine、
  ToolExecutionQueue、event 通道、CancellationToken、approval gates。
- 客户端(TUI / exec / lib)统一通过 Submission/Event 协议通信;headless 模式
  Event 以 JSONL 流写 stdout、tracing 日志走 stderr(`zhuye exec | jq` 不被破坏)。
- 关键设计决策:
  - **Protocol v0 已冻结**:新增 Op/EventMsg 变体 = non-breaking,不可改动既有变体
  - `ToolError` 定义在 zhuye-protocol,用于打破 tools ↔ hooks 循环依赖
  - StateGraph 用 enum + match 手写转移,不引入 petgraph
  - 错误处理:库 crate 用 `thiserror`,应用层(exec/cli)用 `anyhow`

## 开发流程检查表(改任何代码前必读)

**新功能**或 **bug 修复**提交前,必须逐条走完:

1. **形态判断(主循环 vs 插件)** —— **所有功能开发**(新能力、既有能力的
   扩展 / 行为变更、bug 修复引入新行为)动手实现前,必须先回答两问,
   答案写进 commit message 或 PR 描述:
   - **第一问:是否属于主循环?** 该能力是否为 Submission 消费 →
     StateGraph(PreLoop → ModelCall → ToolExec → CheckStop)流转必需的
     引擎机制(协议 Op/EventMsg/Item、状态转移、审批与权限、持久化回放、
     上下文治理、ToolExecutionQueue)?非主循环而允许内置的例外仅两类:
     SDK 门面必需的原语、性能关键的通用工具(先例:多 agent 编排,
     见 `docs/features.md`);
   - **第二问:能否以插件形式存在?** 插件能力槽:commands / 子代理 /
     hooks / skills / MCP servers / WASM 工具(manifest 权威在
     `crates/resources/zhuye-plugin/src/manifest.rs`,开发指南见
     `docs/plugin-dev-guide.md`);
   - 决策矩阵:

     | 第一问(主循环?) | 第二问(槽可承载?) | 形态 |
     |---|---|---|
     | 是(或属两类内置例外) | — | 内置,理由写进 commit / PR |
     | 否 | 能 | **必须插件**(外部接入 / 领域特定 / 可启停 / 第三方可定制);先例:`zhuye-computer-use` / `zhuye-shell` / `zhuye-git-tools` / `zhuye-kms` 作为 `plugins/*` 能力本体挂载、不进 SDK facade |
     | 否 | 能,但须保持原工具名 / 审批 / 默认行为(外部用法零影响) | **门控型插件**:能力本体留引擎 crate,注册由插件启用状态门控(判定单一事实源 `zhuye-plugin::bundled_plugin_enabled`,默认开关表 `BUNDLED_PLUGIN_DEFAULTS`);先例:`goal-queue`、`web-search`、`notebook`、`verify-hooks`、`lsp` |
     | 否 | 不能(需引擎内部状态) | goal-queue 式引擎侧特例装配,或先扩展插件系统再承载;理由必须写明 |

   - 拿不准时先做插件。
   - **门控型与三层过滤的关系**:注册门控(插件级,`[plugins]
     .disabled_plugins`,工具不进注册表)→ 可见性过滤(工具名级,
     `[active] tools` / `disallowed_tools`,LLM 不可见)→ 执行期审批
     (权限规则 / floor);三层正交,不要混用入口。
   判断口径与 `docs/features.md` 的「默认启用 / 需配置 / 需插件」三段结构对齐。
2. **品牌名门禁** —— 仓库内(代码 / 文档 / 配置 / **提交信息**)不得出现
   其它产品的品牌名:
   - **禁**:归属表述(「借鉴某产品的某机制」)与功能性引用(依赖别家私有
     存储格式 / 凭据 / 会话格式的能力)—— 前者误导读者以为本项目与那些产品
     有隶属关系,后者在对方一次格式迭代即断裂;
   - **不处理**:厂商 API 名与模型 id(anthropic / openai / gemini / qwen /
     deepseek / kimi / grok / minimax / glm、`claude-sonnet-*` / `gpt-*` 等),
     以及外部生态的**格式标识符**(目录名 / 环境变量 / 上游数据集路径);
   - **门禁**:`python3 scripts/check_brand_names.py`(扫工作树 + 全部可达
     提交信息);豁免字面量登记在脚本的 `EXEMPT_PATTERNS`,**必须窄到具体
     字面量**。品牌名一旦进入提交历史就永久可见,只能改写历史清除 ——
     所以提交前必须过这一关。
3. **能力覆盖脚本同步** —— 判断本次改动是否需要新增 / 更新能力覆盖用例:
   - 改了 agent 能力(serve 协议 Op / EventMsg / 内置工具 / 审批 / 编排 /
     持久化 / 重连恢复 / CLI 能力面)
     → 必须在 `tests/functional/` 有对应用例,断言到**字段级**
       (不接受"退出码 0"或"不抛错"作为能力断言);
   - 已有相关用例 → 检查是否需要**更新**(断言口径变了 / 新增字段 / 语义变化);
   - 能力覆盖用例**只写在 `tests/functional/`** —— 不要在 SDK 测试里重复写
     协议/能力断言;`tests/rust/*.sh` 是 CLI **参数面**的老套件,新增能力
     不往里加(能力面统一用 Python 写)。
4. **SDK 测试与发布文档同步** —— 改了协议 / SDK 公开行为 / 事件形状时,**两个 SDK
   都要看**:
   - Python SDK:`sdks/python/tests/`(纯逻辑,mock serve)与
     `tests/sdk-python/`(真二进制绑定层);
   - TS SDK:`sdks/typescript/tests/`(纯逻辑,mock serve)与
     `tests/sdk-ts/`(真二进制绑定层);
   - 协议变更是 additive 也必须同步 —— SDK 遇到未知变体要能降级不崩;
   - **发布文档同步**:`sdks/python/README.md` 与 `sdks/typescript/README.md`
     是 PyPI / npm 页面渲染的正式文档 —— Op / EventMsg 参考表、API 参考
     必须随协议与公开行为一起更新(additive 新变体也要进表);wire 语义
     变化同步改 `sdks/PROTOCOL.md`;
   - **文档门禁**:`./scripts/release_sdk.sh check_docs` 从 `zhuye-protocol`
     源码提取 Op 全集,自动校验双 README 与双类型层(`zhuye/protocol.py` /
     `src/protocol.ts`)收录齐全 —— 文档落后协议会在 `check` / `tag`
     发布前直接失败。
5. **覆盖矩阵同步** —— `tests/COVERAGE.md` 的「能力 × 覆盖层」映射表随改动
   更新:新增能力补一行;移除能力移到「未覆盖项」并写明原因;发现的 SDK 缺陷
   记入「已知缺陷」。
6. **AGENTS.md 分层索引同步** —— 修改或新增代码时,对应的 AGENTS.md(该 crate 篇、
   其所在层篇、根导航表)必须同步更新;契约 / 同步点 / 禁止事项变化即改对应篇,
   导航目标增删即改根表。按下表定位:

   | 代码改动 | 同步更新 |
   |---|---|
   | 新增 / 移除 crate 或层 | 新建 / 删除该篇 + 更新所在层篇速览表 + 更新根导航表 |
   | 协议 Op / EventMsg / 变体 | `crates/protocol/` + `zhuye-protocol/` |
   | 内置工具 / provider / mock / hook 事件与决策 | `crates/abilities/` + `zhuye-tools/` `zhuye-llm/` `zhuye-hooks/` |
   | 插件 manifest / 能力 / 安装模型 | `crates/resources/` + `zhuye-plugin/` |
   | StateGraph / NodeContext / side-channel | `crates/runtime/` + `zhuye-core/` |
   | exec/serve 参数 / 退出码 / 恢复 | `crates/runtime/` + `zhuye-exec/` |
   | CLI 子命令 / 参数面 | `crates/runtime/` + `zhuye-cli/` |
   | 编排原语(子代理 / 讨论 / 流水线 / 任务 / 目标) | `crates/orchestration/`(+ 对应 crate 篇若有) |
   | 外部集成(MCP / LSP / computer-use / browser / shell / git) | `crates/integrations/`(+ 对应 crate 篇若有) |
   | 脚本新增 / 移除 / 行为变更 | `scripts/` 速览表 |
   | SDK 公开 API / wire 语义 | `sdks/` |
   | 测试分层 / 覆盖矩阵结构 | `tests/` |

   拿不准改哪篇时,至少更新该改动所在层篇与根导航表,确保索引不失真。
7. **验证** —— 按下方「验证矩阵」跑对应命令,全绿再提交。
8. **注释与提交信息** —— 注释一律中文;commit 用 `type(scope): 描述`
   conventional 风格。

## 验证矩阵(按改动范围选跑)

| 改动范围 | 必跑命令 |
|---|---|
| 任何改动 | `cargo fmt --all` + `cargo clippy --workspace --all-targets -- -D warnings` |
| 触及品牌名 / 归属表述 / 外部生态互操作 | `python3 scripts/check_brand_names.py`(工作树 + 提交信息) |
| Rust 核心 / crate | `cargo test --workspace` |
| serve 协议 / 事件 / 持久化 / 工具 / 编排 / 重连 | `./tests/run_all.sh functional`(能力覆盖层 + Rust 参数面 + 双 SDK 绑定层) |
| 仅 SDK 绑定逻辑 | `./tests/run_all.sh sdk-python` + `./tests/run_all.sh sdk-ts` |
| SDK 发布文档(PyPI/npm README / 协议表) | `./scripts/release_sdk.sh check_docs`(文档落后协议直接失败) |
| CLI 子命令参数面 | `./tests/run_all.sh rust` |
| 文档 / 注释 | 无需跑测试,自查链接与命令是否仍有效 |
| 覆盖率检查(可选) | `cargo llvm-cov --workspace --lib --summary-only`(基线 83% 行覆盖;纯函数不变式另有 proptest) |

## 约定

- **强制:注释一律使用中文** —— 新增或修改代码时,行注释(`//`)、doc-comment
  (`///`、`//!`)、测试说明等必须用中文书写,不得使用英文注释;commit message
  亦用中文,遵循 `type(scope): 描述` conventional 风格
- **强制:修复必须留字条** —— 非显然的防御性代码(守卫、熔断、降级、兜底
  路径、反直觉常量)必须用中文注明「修复原因/依据」:当初怎么坏的(触发
  场景)、现在的机制为什么能防住。让人能靠搜索定位历史缺陷,不当无字据
  的暗知识
- 约一半 lib.rs 顶部有 `#![allow(clippy::...)]` 宽松头(clippy.toml 中
  `avoid-breaking-exported-api = false`);新代码仍以 `-D warnings` 全绿为门槛
- `[profile.dev]` 刻意关闭 incremental、`[profile.release-fast]` 供本地快速迭代
  (见根 Cargo.toml 注释;发布分发用默认 release,thin LTO)
