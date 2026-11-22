# 配置与环境变量

<p align="center">  <b>中文</b> · <a href="configuration.en.md">English</a>  </p>

> 配置文件、热重载与环境变量。完整配置项的字段定义见
> [`zhuye-config`](../crates/resources/zhuye-config/src/schema/mod.rs)。

## 配置文件

配置在 `~/.zhuye/config.toml`，`ZHUYE_HOME` 可以把整个目录挪走：

```toml
[active]
group = "openai"                   # 活跃模型组 id(= 模型组数组里的 id)

[[model_group]]                    # 模型组数组 —— 唯一的模型接入单元
id = "openai"                       # 组 id = 模型 spec 前缀(<id>/<model>),
                                   # 也是凭证池名。必填、唯一、不含 '/';
                                   # 不得与保留名 mock 冲突 —— 违例时跳过
                                   # 该组并 warn,不阻断其余配置
label = "OpenAI"                    # 展示名(缺省用 id)
endpoint = "openai"                 # 线格式:openai | anthropic | ollama
api_key = ""                        # 免 key 的端点可以留空
base_url = "https://api.kilo.ai/api/gateway"
model = "free"                      # 免费模型：运行时从清单里挑一个
temperature = 0.3
max_tokens = 4096
timeout_secs = 300                  # HTTP 超时:建连 + **读空闲**双层
                                    # (读到数据即重置),默认 300。注意不是
                                    # 整请求总时长 —— 合法长流不会因总时长
                                    # 被腰斩;只有连接无字节超过该值才断

[routing]
max_attempts = 10                   # 单次模型调用的总尝试上限(默认 10,
                                    # 即初始 1 次 + 最多 9 次自动重试)。
                                    # 网络 / SSE 中断 / 5xx / 429 等瞬时
                                    # 错误在引擎内自动重试,烧满才报错

[model]
context_window = 128000             # 模型上下文最大长度(token);自适应输出
                                    # 预算与压缩阈值都会跟着走。私端/新模型
                                    # 也可用 [context_windows] 表按模型配:
                                    # [context_windows]
                                    # "MiniMax-M3" = 1000000
reasoning_effort = "high"           # 思考强度:low|medium|high|max|ultra。
                                    # 实际上 wire 前会按模型族钳制 ——
                                    # qwen 系最高 high、deepseek 到 max、
                                    # OpenAI(gpt-5/o 系)到 ultra
```

`endpoint = "anthropic"` 的组同样支持免 key 本地网关（`api_key = ""` + 显式
`base_url`，如 LM Studio）；`endpoint = "ollama"` 的组还支持
`keep_alive_secs` / `num_ctx` / `num_gpu` 三个本地推理专属字段。

## Prompt cache 保温([cache_warming])

空闲期 / 回合内以廉价重放刷新 provider prompt cache 的 TTL(默认关闭,
保温是引擎自发花钱的行为,必须显式开启)。全部字段可选,缺省回落引擎
默认:

```toml
[cache_warming]
enabled = true                   # 默认 false
min_expected_savings_usd = 0.05  # 期望节省低于此值(USD)不保温
idle_probability = 0.15          # 空闲期延续概率(回合内恒为 1.0)
ttl_secs = 300                   # 缓存 TTL 覆盖;缺省取请求各断点 TTL 的
                                 # 最小值(默认配置 = 1h,ZHUYE_CACHE_TTL 可调)
min_context_tokens = 1024        # 低于此规模不保温
refresh_margin_secs = 10         # 在 TTL-边际 处触发(约 90% TTL)
max_idle_window_secs = 1800      # 空闲保温最长窗口(30 分钟)
max_turn_window_secs = 3600      # 回合内保温最长窗口(60 分钟)
# force = true                   # 测试逃生门:跳过定价/命中观测门控
                                 # (配合 mock provider;生产勿开)
```

语义要点:决策消费上次请求的**真实命中回执**(provider 没有缓存行为时
自动跳过);状态经 `cache_warming` 事件可见(`warm_cached_tokens` 可与
后续 `token_count.cached_tokens` 对照验证收益);保温花费计入配额窗口与
telemetry(synthetic 标记),但不进会话 token 预算、不写 rollout。Anthropic
显式 thinking(`budget_tokens`)的请求不可保温重放(API 要求
`max_tokens > budget_tokens`)。热重载支持:改本段即生效(关闭即取消
在飞保温)。

## 从旧配置迁移（v1 → v2）

v1 的固定接入段 `[anthropic]` / `[openai]` / `[ollama]` **仍可读入** ——
加载时自动迁移成等价模型组，写回时只输出新格式：

| 旧配置 | 新配置 |
|---|---|
| `[anthropic]` | 模型组 `id = "anthropic"` / `endpoint = "anthropic"` |
| `[openai]` | 模型组 `id = "openai"` / `endpoint = "openai"` |
| `[ollama]` | 模型组 `id = "ollama"` / `endpoint = "ollama"` |
| `[active].provider` | `[active].group` |
| `[[<段>.credentials]]` | 组内 `[[model_group.credentials]]` |

`api_key` / `base_url` / `model` / `temperature` / `max_tokens` /
`timeout_secs` / `credentials` 原样下沉；`[openai].responses_api` 与
`[ollama].keep_alive_secs` / `num_ctx` / `num_gpu` 落到组级同名字段。

子代理的独立端点段同步改名：`[subagent_providers]` → `[subagent_models]`
（旧段名仍能读入，写回只写新名）。

迁移**幂等**：已是新格式的配置读入后不产生任何变化（不重复追加组，也不
覆盖已有的同名模型组 —— 同名时以模型组现状为准）。旧段数据不会出现在
写回结果里。

## 活跃组的解析顺序

1. env `ZHUYE_PROVIDER`（显式覆盖，兼容面保留）；
2. env `ZHUYE_MODEL` 形如 `mock` / `mock/...`（离线 mock 特判）；
3. TOML `[active].group`；
4. env `ZHUYE_MODEL` 的前缀（是**已知组 id** 时）；
5. 第一个有凭据的组；
6. 都没有 → 未配置模型。

模型名上的前缀只剥「配置里声明过的组 id」与 `mock` —— 用户真实模型名
（如 `spark-2/qwen3.8-27b-nvfp4`）原样保留。

组 id 为 `anthropic` / `openai` / `ollama` 时保留 env 兜底：
`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `OLLAMA_HOST` + `OLLAMA_API_KEY`。
其余组一律不读 env（第三方网关误吞 `OPENAI_API_KEY` 比缺 key 更难诊断）。

热重载、冷却 failover、`model = "free"` 清单发现、
`zhuye models list --provider <组id>` 全部对模型组生效。anthropic 格式的
端点（含网关）在引擎内同样支持模型清单拉取（`GET {base}/v1/models`）。

改完不用重启进程，配置监听器会热重载。

## 工具管控(三层)

工具的「关」有三个正交入口,粒度从粗到细(详见
[features.md](features.md)「三层工具管控」):

```toml
[plugins]
enabled_plugins  = ["git-tools@inline"]   # 启用权威表(name@marketplace 全名)
disabled_plugins = ["web-search@inline"]  # 门控插件显式禁用(见下)

[active]
tools             = []                    # 工具白名单:非空时 LLM 可见工具集 ∩
disallowed_tools  = ["web_search"]        # 工具黑名单:LLM 可见工具集排除 ∪
```

| 层 | 入口 | 效果 |
|---|---|---|
| 装配期·注册门控 | `[plugins].disabled_plugins` | **插件级**:门控插件(web-search / notebook 等默认开插件)的工具不进注册表 —— `tool_search` 搜不到、调用报 tool not found。等价命令:`zhuye plugin disable <id>` |
| 定义期·可见性过滤 | `[active] tools` / `disallowed_tools` | **工具名级**:工具已注册,仅 LLM 不可见(可与 `.zhuye/agents/*.md` frontmatter 组合) |
| 执行期·审批 | 权限规则 / floor | **操作级**:决定能否执行 |

门控插件的启用判定**不热重载**(与插件挂载的热重载不同),变更后重启
会话生效。

## 环境变量

| 变量 | 干什么 |
|------|--------|
| `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` | 凭据兜底（仅 `openai` / `anthropic` 组） |
| `OLLAMA_HOST` / `OLLAMA_API_KEY` | 凭据兜底（仅 `ollama` 组） |
| `ZHUYE_MODEL` | 覆盖模型规格，`=mock` 供离线运行 |
| `ZHUYE_PROVIDER` | 覆盖活跃模型组 id |
| `ZHUYE_HOME` | 数据目录，默认 `~/.zhuye` |
| `ZHUYE_BIN` | SDK 去哪找 `zhuye` 二进制(查找顺序:`options.bin` → `ZHUYE_BIN` → 包内二进制 → PATH,见 [sdk.md](sdk.md)) |
| `ZHUYE_MEMORY_BACKEND` | 记忆后端：默认 `markdown`（MEMORY.md 文件 + BM25 分节检索）；`=sqlite` 启用条目版本库（`memory.db` + FTS5 中文检索，MEMORY.md 退化为可编辑投影；首次打开自动幂等导入既有内容）。sqlite 模式下 compact 固化与后台文件整理暂禁用（自动写入待 M5b 经持久队列提供），新笔记带 workspace 归属、旧无归属笔记不再参加跨项目召回 |
| `ZHUYE_MODELS_DEV_REFRESH` | models.dev 社区目录后台刷新开关:默认开启(exec/serve 启动时后台按 24h TTL 刷新,detached 不阻塞);`=0` 关闭。mock 模型恒跳过(离线零网络) |
| `RUST_LOG` | 日志过滤，默认 `warn,zhuye=info` |

其余环境变量散落在各 crate 里，源码里搜 `ZHUYE_` 前缀即可。
