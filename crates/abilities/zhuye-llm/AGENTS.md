**何时读我**:你要新增 / 修改 provider、调整模型能力标志、路由与凭据池、免费模型判定,或改动 mock provider 的离线行为时,先读本篇。

# zhuye-llm —— 模型客户端与 provider

## 本 crate 是什么

所有模型 provider 的抽象与实现所在:`ModelClient` trait 定义在此,各 provider 实现成 `providers/` 下的模块;同层还承载凭据池、路由策略、配额 / 成本护栏等「选模型、调模型、算账」的逻辑。
本 crate 不碰协议层类型定义(`Submission` / `EventMsg` 在 `zhuye-protocol`),也不直接读配置文件 —— 配置装配由 `zhuye-config` 的 builder 完成。

## 本地契约

### 1. provider 的注册机制

- provider 实现放在 `providers/` 下,各自实现 `client.rs` 的 `ModelClient` trait;`providers/mod.rs` 只放 provider 之间共享的 helper(版本段端点拼接、SSE 响应校验)。
- provider 对外声明的能力走 `capabilities.rs` 的 `Capabilities` 标志位(`tool_use` / `prompt_caching` / `extended_thinking` / `vision` / `json_mode` / `system_blocks`)。**`zhuye-core` 据此决定是否注入 cache breakpoint、是否启用 extended thinking、是否发送多块 system prompt** —— 标志填错,行为会在上层静默走偏。
- provider 身份用强类型的 `ProviderKind` 表达(`Anthropic` / `OpenAI` / `Ollama` / `Custom`),不要退回字符串比较。
- 多凭据场景走 `registry.rs` 的 `ModelRegistry`(name → `CredentialPool`,加权轮询 + cooldown 过滤),单凭据元数据在 `credential.rs`,冷却状态在 `cooldown.rs`。

### 2. 新增 provider 的同步点

新增一个 provider 时,除本 crate 的实现外,下列三处必须同步:

| 同步点 | 落点 |
|---|---|
| 凭据写入入口 | `zhuye login` 子命令的 provider 枚举(`crates/runtime/zhuye-cli/src/login.rs`) |
| 配置段 | `docs/configuration.md` 的 provider 段说明 + `zhuye-config` 的 schema |
| 模型清单入口 | `zhuye models` 子命令(`crates/runtime/zhuye-cli/src/models.rs`) |

### 3. mock provider 是离线门禁的基础设施,不得静默变更

`providers/mock.rs` 的行为契约是全仓离线流程(`ZHUYE_MODEL=mock`、examples、SDK 冒烟)的地基:

- 注册条件由配置侧决定(`ZHUYE_PROVIDER=mock`,或 `ZHUYE_MODEL` 形如 `mock` / `mock/...`),注册为 `mock` pool,**免 API key、零网络**。
- 环境变量 `ZHUYE_MOCK_SCRIPT` 指向一个 JSONL 文件,**每行对应一次模型调用**,按顺序消耗;支持文本回复、整段消息回复、工具调用、工具调用参数分片几种形态 —— 分片形态是为锻炼下游的流式聚合路径而存在的。
- 脚本耗尽或未配置时回退到**带可辨识标记**的固定回复,让「脚本写错 / 覆盖不足」不会被断言「文本非空」的测试假绿。
- 脚本路径不存在 / 读取失败 / 全部行解析失败时,**首次模型调用显式报错**,不静默降级。

改动这个文件时,以上四条必须原样保住;确需调整要同步 `docs/cli.md` 与相关冒烟脚本。

## 核心文件导航

| 文件 | 一句话职责 |
|---|---|
| `src/client.rs` | `ModelClient` trait 定义 |
| `src/capabilities.rs` | provider / 模型能力标志与 `ProviderKind` |
| `src/registry.rs` | 多凭据池注册表与派位 |
| `src/policy.rs` | 角色 → spec slot 路由策略 |
| `src/routing.rs` | 每轮动态选模 |
| `src/providers/` | 各 provider 实现与共享 helper |
| `src/providers/mock.rs` | 离线 mock provider 契约 |
| `src/quota.rs` / `src/spend.rs` | 配额追踪与成本护栏 |

## 禁止事项

- 不得让 provider 实现绕过 `Capabilities` 声明能力。
- 不得把 provider 身份退化成裸字符串比较。
- 不得在 mock provider 里「顺手」加真实网络调用,也不得把固定回复改成不带标记的文案。
- 不得把 provider 专属的凭据解析逻辑复制到 CLI 层 —— CLI 只做读写。

## 深入阅读

- [docs/configuration.md](../../../docs/configuration.md) —— provider 配置段与环境变量
- [docs/architecture.md](../../../docs/architecture.md) —— 模型调用在 StateGraph 中的位置
- [docs/cli.md](../../../docs/cli.md) —— `login` / `models` 子命令的参数面
