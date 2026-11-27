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
- 多凭据场景走 `registry.rs` 的 `ModelRegistry`(name → `CredentialPool`,加权轮询 + cooldown 过滤),单凭据元数据在 `billing/credential.rs`,冷却状态在 `billing/cooldown.rs`(计费族五模块由 `billing/mod.rs` 统一声明)。
- 端点有模型清单 API 的 provider 必须实现 `ModelClient::list_models`(trait 默认 `Ok(None)` = 诚实降级「无清单端点」):openai / responses / ollama / anthropic 均已实现(anthropic 走 `GET {base}/v1/models`,认证头与 chat 同规则)。**404/405 返回 `Ok(None)`、其余状态如实报错**;清单发现(`zhuye-config::discovery`)依赖本能力做 `model = "free"` 解析与 `zhuye models list`,GUI 侧展示清单另有直连实现,新增/改动时两侧口径要保持一致。

### 2. 内联 System 消息折叠是发 wire 前的硬步骤

- 压缩摘要（`zhuye-compact`）与 hook 注入会往 `messages` 中途塞入内联 `System`；而 OpenAI 兼容 / Ollama / Responses 的线上形态要求 system 只能出现在数组开头，严格网关对「用户消息之后再出现 system」直接返回 HTTP 400，Anthropic 则会静默丢弃消息流里的 `System`（内容直接丢失）。
- 因此各 provider 在发 wire 前统一调 `ChatRequest::fold_system_messages`（`src/request.rs`）：把内联 `System` 按原出现顺序折叠进顶层 `system` 块，`messages` 只留对话轮次。幂等；全 `System` 的消息列表保持原样（折叠后为空会被 provider 拒绝）；空白内容直接丢弃。
- 新增 provider 必须同样走这一步 —— 跳过它不仅触发 400 / 丢内容，`inject_cache_control` 按消息下标写入的缓存断点也会因 provider 丢弃消息而漂移。

### 3. 新增 provider 的同步点

新增一个 provider 时,除本 crate 的实现外,下列三处必须同步:

| 同步点 | 落点 |
|---|---|
| 凭据写入入口 | `zhuye login` 子命令的 provider 枚举(`crates/runtime/zhuye-cli/src/commands/login.rs`) |
| 配置段 | `docs/configuration.md` 的 provider 段说明 + `zhuye-config` 的 schema |
| 模型清单入口 | `zhuye models` 子命令(`crates/runtime/zhuye-cli/src/commands/models.rs`) |

### 4. mock provider 是离线门禁的基础设施,不得静默变更

`providers/mock.rs` 的行为契约是全仓离线流程(`ZHUYE_MODEL=mock`、examples、SDK 冒烟)的地基:

- 注册条件由配置侧决定(`ZHUYE_PROVIDER=mock`,或 `ZHUYE_MODEL` 形如 `mock` / `mock/...`),注册为 `mock` pool,**免 API key、零网络**。
- 环境变量 `ZHUYE_MOCK_SCRIPT` 指向一个 JSONL 文件,**每行对应一次模型调用**,按顺序消耗;支持文本回复、整段消息回复、工具调用、工具调用参数分片几种形态 —— 分片形态是为锻炼下游的流式聚合路径而存在的。
- `tool_batch` 可在同一次模型回复内按数组顺序生成多个工具调用,用于验证批次屏障;不改变单调用脚本语义。
- 脚本耗尽或未配置时回退到**带可辨识标记**的固定回复,让「脚本写错 / 覆盖不足」不会被断言「文本非空」的测试假绿。
- 脚本路径不存在 / 读取失败 / 全部行解析失败时,**首次模型调用显式报错**,不静默降级。
- **v1.x 唯一例外**:带 `metadata["warm_replay"]="true"` 标记的合成请求(prompt cache 保温重放,见 `src/cache_warming.rs`)**不消耗脚本行**,也不触发 config_error —— 保温是引擎自发的额外调用,照常 pop 脚本会破坏「脚本行 ↔ 真实模型调用」的一一对应。仍进 `recorded_requests()` 供断言。真实 provider 忽略未知 metadata 键,wire 不受影响。

改动这个文件时,以上契约必须原样保住;确需调整要同步 `docs/cli.md` 与相关冒烟脚本。

### 5. prompt cache 保温决策是纯函数(数字在此,调度在 core)

- `src/cache_warming.rs` 是保温的**决策模块**(零 I/O):`evaluate` 按期望收益(`延续概率 × miss成本 − warmCost ≥ 阈值`)决定是否保温,`effective_ttl_secs` / `refresh_delay_secs` 给出刷新节奏。默认值与 pi(cache-warming.ts)对齐(阈值 $0.05、空闲延续概率 0.15、窗口 60/30 分钟)。
- 与 pi 的差异(设计约定):决策消费**真实观测**回执(`CacheObservation` = 上次请求的 cached + cache_write 两段)—— 全零回执直接 skip(`no_cache_hits`,provider 实际没在缓存),warmCost 按「存活段 read 价 + 失效段重写价」精化;OpenAI 无写入档时失效重读回退 input 价。
- **调度器不在这里**:zhuye-core 的 `CacheWarmingScheduler` 管候选槽 / 单飞 / 代际失效 / 零污染不变式;本模块不知道引擎状态的存在,表驱动单测完全离线。
- `ModelClient::warming_replayable` 是保温重放可行性的 provider 判定点:默认 `true`;**Anthropic override 为「显式 thinking(`Enabled{budget_tokens}`)不可重放」**(API 要求 `max_tokens > budget_tokens`,1-token 重放会被拒绝)。新增 provider 若 wire 形态里有与 `max_tokens` 派生 / 约束相关的缓存键字段,必须同样 override。

### 6. usage 口径归一:provider 必须把缓存分段折入 `input_tokens`

- **协议不变式**:`ChatEvent::Usage.input_tokens` 是「本次 prompt 的全部输入」,`cached_tokens`(cache_read)与 `cache_write_tokens`(cache_creation)都是它的**子集**,不是并列项。引擎内所有消费者(计价、缓存命中率、上下文占用、压缩触发)都按这个不变式写。
- **Anthropic 线缆相反**:它的 `usage.input_tokens` 只报「既未从缓存读出、也未为缓存写入」的部分,**不含**两个缓存分段。原样透传会让 `cached > input`,后果是缓存命中率算出 > 100%(真实案例 1052%)、`pricing::price` 的 `non_cached` 饱和到 0(新鲜输入按缓存价计费)、`total_tokens` 与上下文占用系统性低报、压缩几乎永不触发。
- **适配层义务**:Anthropic 适配器在发射 `ChatEvent::Usage` 前调用 `zhuye_protocol::TokenUsage::fold_cache_segments(raw_input, cached, cache_write)` 折入(见 `src/providers/anthropic.rs`);OpenAI 兼容 / Responses 的 `prompt_tokens` 本就含 cached,无需折入;Ollama / mock 无缓存段。
- **新增 provider 的同步点**:线缆的 `input_tokens` 若**不含**缓存分段,必须在发射前折入,并补一条断言「`cached ≤ input`」的回归测试。

## 核心文件导航

| 文件 | 一句话职责 |
|---|---|
| `src/client.rs` | `ModelClient` trait 定义 |
| `src/request.rs` | `ChatRequest` 及嵌套类型;`fold_system_messages` 内联 System 折叠 |
| `src/cache_warming.rs` | v1.x prompt cache 保温决策(纯函数;`METADATA_WARM_REPLAY` 标记) |
| `src/task_budget.rs` | 任务树预算的原子预留、实际与未知用量结算、派发计数和模型流包装 |
| `src/schema_fold.rs` | v1.x strict schema 折叠(不支持约束写进 description;折不掉整体降级) |
| `src/capabilities.rs` | provider / 模型能力标志与 `ProviderKind` |
| `src/model_catalog.rs` | 清单条目 `ModelInfo`(窗口/视觉/输出上限/思考档位,缺省即 `None`)与免费判定规则 |
| `src/models_dev.rs` | models.dev 社区目录:跨厂商投票 + 兜底链查询 + 内嵌快照/磁盘缓存/刷新,补静态表未收录模型的四维能力 |
| `src/output_budget.rs` | 自适应输出预算(模型输出上限 = 内置精确表 → models.dev 目录) |
| `src/reasoning_effort.rs` | 思考强度阶梯与钳制(上限 = 内置静态表 → models.dev 目录档位映射) |
| `src/registry.rs` | 多凭据池注册表与派位 |
| `src/policy.rs` | 角色 → spec slot 路由策略 |
| `src/routing.rs` | 每轮动态选模 |
| `src/providers/` | 各 provider 实现与共享 helper |
| `src/providers/mock.rs` | 离线 mock provider 契约 |
| `src/billing/` | 计费族:`accounts` / `quota` / `spend` / `cooldown` / `credential`(配额追踪与成本护栏、凭据元数据与冷却),`billing/mod.rs` 统一声明 |

### 模型元数据级联(谁能给谁补缺)

`provider 清单 live 披露 > 用户配置 override > 本 crate 策展静态表 > models.dev 目录` —— 目录(`models_dev.rs`)只填空缺,从不覆盖更权威的源(与 magpie 的 Decorate 语义一致):

- 窗口:`context_window_for` = override → 静态表 → 目录;
- 输出上限:`output_budget::model_output_limit` = 精确表 → 目录;
- 思考档位:`reasoning_effort::max_effort_for_model` = 静态表 → 目录档位映射 → 保守默认;
- 清单条目:`enrich_model_info` 用目录补 `ModelInfo` 的 `None` 字段。

目录数据 = 内嵌快照(`assets/api.json.gz`,`scripts/fetch-models-dev.sh` 产出)与磁盘缓存(`$ZHUYE_HOME > $HOME/.zhuye` 下 `cache/models-dev.json`)取新;`refresh_if_stale` 由调用方显式触发,**引擎启动不联网**。快照与缓存都是原始 api.json 格式,聚合 / 投票在 Rust 加载期一处完成。

## 禁止事项

- 不得让 provider 实现绕过 `Capabilities` 声明能力。
- 不得把 provider 身份退化成裸字符串比较。
- 不得在 mock provider 里「顺手」加真实网络调用,也不得把固定回复改成不带标记的文案。
- 不得把 provider 专属的凭据解析逻辑复制到 CLI 层 —— CLI 只做读写。
- **不得原样透传线缆的 `input_tokens`** —— 若该 provider 的线缆口径不含缓存分段,必须先折入再上报(见本地契约 6)。

## 深入阅读

- [docs/configuration.md](../../../docs/configuration.md) —— provider 配置段与环境变量
- [docs/architecture.md](../../../docs/architecture.md) —— 模型调用在 StateGraph 中的位置
- [docs/cli.md](../../../docs/cli.md) —— `login` / `models` 子命令的参数面
