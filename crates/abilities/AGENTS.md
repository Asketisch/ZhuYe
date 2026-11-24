**何时读我**:你要改动本层的任何 crate(模型客户端 / 内置工具 / hooks / 提示词 / 检索 / 记忆),或要判断「工具、hook、模型之间谁可以依赖谁」时,先读本篇;改单个 crate 前再进入该 crate 的独立篇。

# crates/abilities —— 能力原语层

## 本层是什么

能力原语层:提供**可被上层组合的原子能力** —— 模型调用、工具执行、hook 拦截、提示词装配、检索、记忆、压缩、评测。
本层不含会话编排、不含 CLI 入口、不含持久化落盘策略;这些分别由 `crates/orchestration/` 与 `crates/runtime/` 承担。
Unix 前后台 shell 共用 `zhuye-tools::child_process` 管理独立进程组的取消与丢弃清理。

## crate 速览

| crate | 一句话职责 | 本地契约在哪 |
|---|---|---|
| `zhuye-llm` | `ModelClient` trait、多 provider 实现、路由与凭据池 | [zhuye-llm/](zhuye-llm/AGENTS.md) |
| `zhuye-tools` | `Tool` trait、内置工具注册表、工具执行队列 | [zhuye-tools/](zhuye-tools/AGENTS.md) |
| `zhuye-hooks` | `HookEngine`、hook 事件与决策协议、shell hook | [zhuye-hooks/](zhuye-hooks/AGENTS.md) |
| `zhuye-bm25` | BM25 相关性排序内核 | 并入本篇 |
| `zhuye-skills` | SKILL.md 扫描、加载与技能激活;skill 市场安装(GitHub 单仓 + skills.sh 目录生态 tarball 整目录)与 skills.sh 目录索引/本地搜索 | 并入本篇 |
| `zhuye-memory` | 三 scope 持久记忆与检索 | 并入本篇 |
| `zhuye-agent-def` | Markdown + frontmatter 的 agent 定义解析 | 并入本篇 |
| `zhuye-prompt` | 提示词模板装配、provider 提示词资源、三态方案选择（default / auto / manual）与上游来源维护；v1.7 起两级目录（工具 → 模型变体，`catalog.rs`）与 auto 按模型匹配 | 并入本篇 |
| `zhuye-compact` | 上下文压缩策略 + v1.x rapid-refill 熔断器(`refill.rs`:压缩后 ≤3 步再触发 = refill,连续 3 次 Abort 收口,force 绕过) | 并入本篇 |
| `zhuye-recovery` | 上下文恢复(元消息收集与回填) | 并入本篇 |
| `zhuye-notes` | 会话级增量笔记 | 并入本篇 |
| `zhuye-sanitize` | 密钥脱敏叶 crate | 并入本篇 |
| `zhuye-eval` | 固定评测集与自动打分 | 并入本篇 |

「并入本篇」= 该 crate 规模不足以独立成篇,它的本地契约留在本篇(见下方通用规则)。

## 层内依赖方向

- 叶 crate 不依赖任何其他能力 crate:`zhuye-bm25`(仅 `rankfns`)、`zhuye-sanitize`(仅 `regex` / `serde` / `sha2` / `once_cell` 等基础依赖)。
- `zhuye-bm25` 被 `zhuye-tools`、`zhuye-memory`、`zhuye-notes` 三方共用 —— **检索能力全层只此一份**。
- `zhuye-tools` → `zhuye-hooks` 是**单向依赖**;`zhuye-hooks` 不得反向依赖 `zhuye-tools` —— hook 事件不携带 `ToolContext`,因此能绕开成环。
- `zhuye-prompt` 处在依赖链上游:`zhuye-tools` 依赖它,它自身依赖 `zhuye-llm` 与 `zhuye-config` 做 provider 相关的提示词装配。

## 跨层边界

- 本层的公共契约一律取自 `zhuye-protocol`;本层不定义协议类型。
- 本层向 resources 层取**少量、方向明确**的依赖:配置解析(`zhuye-config`:hooks / notes / prompt)与文件原语(`zhuye-fs`:llm / memory / tools),以及 `zhuye-tools` 用到的 `zhuye-git` / `zhuye-permissions` / `zhuye-rollout` / `zhuye-sandbox`。
- 反向(resources → abilities)只出现在少数特例:`zhuye-ast`、`zhuye-wasm`、`zhuye-plugin` 分别依赖 `zhuye-tools` / `zhuye-hooks`。**因此本层不得反向依赖这三个 crate**,否则成环 —— 已知反依赖约束见 [zhuye-tools/](zhuye-tools/AGENTS.md)。
- 本层不感知 runtime / orchestration:编排顺序、headless 生命周期、CLI 参数面都在上层。
- 压缩管线汇总 microcompact 与 smart-prune 的实际修改；已清除的工具占位符不得重复计作修改，所有阶段均未改动时返回 Noop。
- 有同一次模型上报时，阶段升级和快速回填登记使用「上报值 / 本地估算」校准水位，比例限制在 1–3；没有上报时比例为 1。校准后仍不低于触发阈值的裁剪可以回写，但不登记快速回填锚点。
- 工具图片与用户图片共用同一个可替换估算器。默认固定图片成本只用于压缩触发，不是 provider 的精确计费值。
- 近期大结果由压缩的最近消息窗口保留，并扩展窗口以保持工具调用与结果配对；更早的大结果落盘或替换占位。

## 本层通用规则

1. **错误类型统一在协议层** —— `ToolError` / `ToolOutput` 定义在 `zhuye-protocol`,目的就是打破 tools ↔ hooks 的循环依赖。tools 与 hooks 之间新增能力时直接复用协议层类型,不要在两个 crate 之间另造错误类型,也不要新开一条依赖边。
2. **提示词文案外置** —— 面向模型的文案不写死在 Rust 代码里:按 provider 拆成 `crates/abilities/zhuye-prompt/prompts/` 下的 Markdown 文件;v1.7 起工具×模型变体在其 `prompts/tools/<tool>/<variant>.md` 子树(英文策展,与 `catalog.rs` 的 `EMBEDDED_TOOL_PROMPTS` 注册表双向同步 —— 有测试锁定)。改文案改文件,不动逻辑;新增 provider 提示词同时在该目录补文件。上游来源与固定版本登记在同目录 `sources.json`(repository / revision / sha256 / 适配模型;工具目录用独立的 `tools_collection` 基准);`scripts/check_prompt_sources.py` 对比固定版与上游分支并产出审核报告(profiles + tools 两段),由 CI `.github/workflows/prompt-sources.yml` 调用 —— 更新文案时同步登记来源,否则审核报告会出现指纹漂移。
3. **检索只此一份** —— BM25 相关性排序由 `zhuye-bm25` 提供,`zhuye-tools`(grep 的 `rank=bm25`)、`zhuye-memory`、`zhuye-notes` 共用。新增消费方直接依赖它,不要在消费方另写一套打分逻辑。
4. **记忆存储契约**(zhuye-memory 无独立篇,故留在本篇)—— 持久记忆分 project / user / session 三 scope:project 落 `{workspace}/.zhuye/agent-memory/{agent_type}/MEMORY.md`(可入 VCS 共享),user 落 `~/.zhuye/agent-memory/{agent_type}/MEMORY.md`,session 仅在内存中。注入记忆的字符上限由 `MAX_MEMORY_INJECT_CHARS` 定义;改存储位置或 scope 语义要同步调用方。
5. **新能力先过形态判断** —— 向本层新增工具 / hook / 能力原语前,先走根 `AGENTS.md` 开发流程检查表第 1 步「形态判断(内置 vs 插件)」:能用插件槽承载的(MCP server / WASM 工具 / skill / command / hook)不加进本层,本层只收引擎必需的原子能力。
6. **溢出文本的读写** —— 工具层提供安全的工作区溢出文件写入与 `read` 的 UTF-8 字节分页；模型上下文预算由运行时执行，不覆盖各工具对宿主的输出上限。
7. **按需工具转发必须保留门控和审批** —— `deferred_call` 仅在入口已注册时展开 MCP 目标,目标权限与 hook 按真实工具执行;禁止嵌套转发及绕过队列的直接转发。

## 核心文件导航

| 文件 | 一句话职责 |
|---|---|
| `zhuye-tools/src/lib.rs` | `Tool` trait 与内置工具注册入口 |
| `zhuye-llm/src/lib.rs` | `ModelClient` 与 provider 注册入口 |
| `zhuye-hooks/src/lib.rs` | `HookEngine` 与事件 / 决策类型导出 |
| `zhuye-prompt/src/lib.rs` | 提示词模板装配入口 |
| `zhuye-prompt/src/catalog.rs` | v1.7:工具×模型两级目录解析 + 变体内嵌注册表(`EMBEDDED_TOOL_PROMPTS`)+ auto 按模型匹配 |
| `zhuye-prompt/prompts/sources.json` | 提示词方案的上游来源登记(revision + sha256;v1.7 起含 `tools` 目录段 + 独立 `tools_collection` 基准),`check_prompt_sources.py` 的解析输入 |
| `zhuye-memory/src/scope.rs` | 三 scope 路径解析 |
| `zhuye-bm25/src/lib.rs` | BM25 检索内核 |
| `zhuye-sanitize/src/lib.rs` | 脱敏规则与提示词守卫 |

## 深入阅读

- [docs/tools-and-hooks.md](../../docs/tools-and-hooks.md) —— 工具与 hook 的完整系统说明(内置工具契约、事件 / 决策协议、队列模型)
- [docs/prompts.md](../../docs/prompts.md) —— 提示词体系与外置资源
- [docs/configuration.md](../../docs/configuration.md) —— 与本层能力相关的配置段
- [docs/architecture.md](../../docs/architecture.md) —— 全局分层与数据流
