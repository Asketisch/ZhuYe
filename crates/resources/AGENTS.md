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

## 核心文件导航

| 文件 | 一句话职责 |
|---|---|
| `zhuye-fs/src/lib.rs` | 数据目录解析与原子写实现 |
| `zhuye-config/src/schema/mod.rs` | 配置数据结构定义处 |
| `zhuye-config/src/watch.rs` | 配置热重载监听 |
| `zhuye-config/src/builder/` | 配置 → ModelRegistry 的装配 |
| `zhuye-plugin/src/manager/mod.rs` | 插件安装 / 卸载与状态记账 |
| `zhuye-rollout/src/lib.rs` | JSONL rollout 落盘与轮转 |
| `zhuye-ast/src/lib.rs` | tree-sitter 代码搜索工具入口 |
| `zhuye-wasm/src/lib.rs` | WASM 组件宿主入口 |

## 深入阅读

- [docs/configuration.md](../../docs/configuration.md) —— 配置段与环境变量的完整说明
- [docs/plugins.md](../../docs/plugins.md) —— 插件体系总览
- [docs/plugin-dev-guide.md](../../docs/plugin-dev-guide.md) —— 插件开发指南
- [docs/architecture.md](../../docs/architecture.md) —— 全局分层与数据流
