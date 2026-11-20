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
| `RUST_LOG` | 日志过滤，默认 `warn,zhuye=info` |

其余环境变量散落在各 crate 里，源码里搜 `ZHUYE_` 前缀即可。
