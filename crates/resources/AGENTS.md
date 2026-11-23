**何时读我**:你要改动本层的任何 crate(配置 / 权限 / 插件 / 持久化 / 审计 / 密钥 / 文件原语 / 代码搜索 / WASM 宿主),或要弄清「用户数据目录、配置文件、插件状态分别由谁定义」时,先读本篇;改单个 crate 前再进入该 crate 的独立篇。

# crates/resources —— 资源层

## 本层是什么

资源层:提供**宿主环境相关的落地能力** —— 读配置、管权限、装插件、持久化 rollout、记审计与遥测、加密密钥、操作文件、跑 git 能力、跑 tree-sitter 代码搜索、托管 WASM 组件。
本层不决定「何时调用」这些能力,调用时机由 `crates/runtime/` 与 `crates/orchestration/` 负责。

## crate 速览

| crate | 一句话职责 | 本地契约在哪 |
|---|---|---|
| `zhuye-config` | 统一 TOML 配置加载与热重载 | 并入本篇 |
| `zhuye-permissions` | 持久化工具权限规则与审批 resolver | 并入本篇 |
| `zhuye-plugin` | 插件 manifest、lifecycle 与能力 loader | [zhuye-plugin/](zhuye-plugin/AGENTS.md) |
| `zhuye-rollout` | 带轮转与脱敏的 JSONL rollout 持久化 | 并入本篇 |
| `zhuye-telemetry` | 本地 Langfuse 式日志(两层 JSONL) | 并入本篇 |
| `zhuye-sandbox` | OS 级沙箱(Seatbelt / Landlock) | 并入本篇 |
| `zhuye-audit` | 审计语义日志(区别于可观测性) | 并入本篇 |
| `zhuye-secrets` | 密钥加密存储 | 并入本篇 |
| `zhuye-fs` | 共享文件原语(原子写等) | 并入本篇 |
| `zhuye-git` | git 能力族(checkpoint / 探测 / 有界命令) | 并入本篇 |
| `zhuye-ast` | tree-sitter 代码搜索与重写工具 | 并入本篇 |
| `zhuye-wasm` | WASM 组件插件宿主(wasmtime 组件模型) | 并入本篇 |

「并入本篇」= 该 crate 规模不足以独立成篇,它的本地契约留在本篇(见下方通用规则)。

## 层内依赖方向

- `zhuye-fs` 是本层最底座,无层内依赖;`zhuye-permissions`、`zhuye-rollout` 依赖它。
- `zhuye-config` 依赖 `zhuye-fs` 与 `zhuye-permissions`;`zhuye-telemetry` 依赖 `zhuye-config`。
- `zhuye-sandbox`、`zhuye-secrets`、`zhuye-git` 无层内依赖,是可直接被上层调用的独立能力。
- 跨层依赖是**允许且已知**的:`zhuye-ast` 依赖 `zhuye-tools`、`zhuye-wasm` 依赖 `zhuye-hooks` 与 `zhuye-tools`、`zhuye-plugin` 依赖工具 / hook / 技能 / 子代理等能力 crate;同时 abilities 层的多个 crate 也依赖本层(配置解析与文件原语)。**两个方向都存在,成环要按 crate 对判断** —— 见下方「跨层边界」。

## 跨层边界

- 本层不定义公共协议:`Submission` / `Op` / `EventMsg` / `Item` 一律在 `zhuye-protocol`。
- 本层不编排会话:工具调用顺序、hook 派发时机由 `crates/runtime/` 决定。
- **跨层方向不是单向的**:abilities 层也依赖本层的 `zhuye-config` / `zhuye-fs` / `zhuye-git` / `zhuye-permissions` / `zhuye-rollout` / `zhuye-sandbox`。所以成环风险**必须按 crate 对判断,不能按层判断**:
  - `zhuye-ast` 依赖 `zhuye-tools` → **`zhuye-tools` 不得依赖 `zhuye-ast`**;AST 工具以调用方注入方式进入工具层。
  - `zhuye-plugin` 依赖 `zhuye-tools` / `zhuye-hooks` → **`zhuye-tools` 与 `zhuye-hooks` 不得依赖 `zhuye-plugin`**。
  - `zhuye-wasm` 依赖 `zhuye-tools` / `zhuye-hooks` → 同上约束。

## 本层通用规则

1. **数据目录只有一个解析点** —— 用户数据目录由 `zhuye_fs::data_dir()` 解析:优先取环境变量 `ZHUYE_HOME`(非空),否则取 `HOME`,再兜底为 `~/.zhuye`;解析逻辑集中在 `resolve_data_dir` 一个函数里。新增需要落盘的资源一律用它取根目录,**不要在别处自己拼 `~/.zhuye`**。(zhuye-fs 无独立篇,故留在本篇)
2. **写文件走原子替换** —— 需要「完整内容替换式写入」的地方统一走 `zhuye-fs` 的原子写:先写同目录临时文件再 rename,写入任一步失败保留原文件、临时文件不残留;目标是 symlink 时写它指向的真实目标而非替换链接本身,只读目标直接拒绝。**不要引入第二份「原子写」实现**。
3. **配置 schema 只有一个定义处** —— 配置字段结构集中在 `zhuye-config` 的 `schema/` 子模块;新增或改动配置段时,schema 与 `docs/configuration.md` 同步更新,不要在消费 crate 里另写一份解析逻辑。(zhuye-config 无独立篇,故留在本篇)
4. **插件状态三文件各司其职** —— `installed_plugins.json` 描述「装了什么」(插件 cache 目录内),`~/.zhuye/config.toml` 的 `enabled_plugins` 描述「启用什么」,`~/.zhuye/managed-settings.json` 由企业策略 pin 住不可改的 managed 插件;三者不一致时报配置漂移。插件自身契约见 [zhuye-plugin/](zhuye-plugin/AGENTS.md)。
5. **skill 扫描目录清单只有一个定义处** —— `zhuye-config/src/paths.rs::default_skill_dirs` 是 skill 目录链(`.zhuye/skills` ×2 + 业界通用 `.agents/skills` ×2,顺序即同名优先级)的唯一出口:两处 bootstrap(exec / m4)与 GUI 列表命令都消费它,不要在消费方自拼 skill 目录名。`.agents/skills` 是外部生态通用目录(非烛夜私有),故不走 `ZHUYE_HOME` 重定向。
6. **shell 放行规则不能解释动态语法** —— `zhuye-permissions` 的 shell glob 命中含展开、重定向或其他未解析语法的命令时,将 Allow 降为 Ask;默认策略仍使 Deny 优先。不得将 glob 命中当作 shell 语义解析。
7. **只读命令分类必须保守** —— 支持脚本执行、网络写入或文件输出的程序与参数不得自动放行；`find`、`sort`、`rg`、`diff` 和 Git 的副作用选项必须按具体参数检查。

## 核心文件导航

| 文件 | 一句话职责 |
|---|---|
| `zhuye-fs/src/lib.rs` | 数据目录解析与原子写实现 |
| `zhuye-config/src/schema/mod.rs` | 配置数据结构定义处 |
| `zhuye-config/src/schema/model_group.rs` | 模型组(`[[model_group]]`)schema:**唯一的模型接入单元**,组 id 即模型 spec 前缀与池名;`endpoint` 支持 openai / anthropic / ollama |
| `zhuye-config/src/schema/legacy.rs` | v1 固定接入段(`[anthropic]` / `[openai]` / `[ollama]`)schema:仅用于读入旧配置并迁移,不参与序列化 |
| `zhuye-config/src/load.rs` | 配置加载 + **v1→v2 迁移**(旧固定段 → 等价模型组,幂等;`load_from_str` / `load_from_file` / `load_default` 全跑迁移) |
| `zhuye-config/src/watch.rs` | 配置热重载监听 |
| `zhuye-config/src/builder/` | 配置 → ModelRegistry 的装配(只认模型组,池名 = 组 id;`pools.rs` 建池 + `shapes.rs` MCP/LSP 形状;非法/重复组 id warn 跳过) |
| `zhuye-config/src/migration.rs` | v1→v2 迁移:旧固定段(`[anthropic]`/`[openai]`/`[ollama]`)→ 等价模型组,`[active].provider` → `[active].group`,`[subagent_providers]` → `[subagent_models]`;幂等 |
| `zhuye-plugin/src/manager/mod.rs` | 插件安装 / 卸载与状态记账 |
| `zhuye-rollout/src/lib.rs` | JSONL rollout 落盘与轮转 |
| `zhuye-rollout/src/index/mod.rs` | 会话索引:`list_sessions` 只读每文件首行(按 session 去重);首行非 SessionMeta 时在前缀窗口与轮转副本中恢复。`message_count` 把同回合同角色的增量落盘分片计为一条消息 |
| `zhuye-rollout/src/index/stats.rs` | 会话统计聚合(总量 + 按模型拆分 + 缓存分段)。聚合 `TokenCount` 时用 `TokenUsage::repair_unfolded_cache_segments` 兼容旧口径记录(未折入缓存分段 → 补齐并重算 total;幂等),保证缓存命中率不 > 100% |
| `zhuye-sandbox/src/os_sandbox.rs` | OS 沙箱(Seatbelt / Landlock):Seatbelt 限写并拒绝敏感读,关网时仍放行 localhost 与 Unix socket;Linux Landlock ABI v3 限写和截断但不限制敏感读,默认经 seccomp 禁止套接字连接,只要沙箱启用安装失败即拒绝执行(不受 strict 和网络开关影响)。状态报告须区分后端及不可用情况 |
| `zhuye-sandbox/src/os_sandbox.rs`(敏感路径) | `path_protection_report()` 逐条给出受保护路径是否**真的**只读:可写根外 = 默认拒绝(`EnforcedByDefault`);可写根内 = macOS Seatbelt deny 规则(`Enforced`)/ Linux Landlock 无法撤销(`Unsupported` —— 规则按位合并,内核拒绝空掩码)。`strict_paths`(= `ZHUYE_SANDBOX_STRICT_PATHS=1`)时不可强制的显式要求会让 bash 失败关闭 |
| `zhuye-ast/src/lib.rs` | tree-sitter 代码搜索工具入口 |
| `zhuye-wasm/src/lib.rs` | WASM 组件宿主入口 |

## 深入阅读

- [docs/configuration.md](../../docs/configuration.md) —— 配置段与环境变量的完整说明
- [docs/plugins.md](../../docs/plugins.md) —— 插件体系总览
- [docs/plugin-dev-guide.md](../../docs/plugin-dev-guide.md) —— 插件开发指南
- [docs/architecture.md](../../docs/architecture.md) —— 全局分层与数据流
