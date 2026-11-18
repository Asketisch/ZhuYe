# 配置与环境变量

<p align="center">  <b>中文</b> · <a href="configuration.en.md">English</a>  </p>

> 配置文件、热重载与环境变量。完整配置项的字段定义见
> [`zhuye-config`](../crates/resources/zhuye-config/src/schema/mod.rs)。

## 配置文件

配置在 `~/.zhuye/config.toml`，`ZHUYE_HOME` 可以把整个目录挪走：

```toml
[active]
provider = "openai"

[openai]
api_key = ""                                  # 免 key 的端点可以留空
base_url = "https://api.kilo.ai/api/gateway"
model = "free"                                # 免费模型：运行时从清单里挑一个
temperature = 0.3
max_tokens = 4096
timeout_secs = 300                            # HTTP 超时:建连 + **读空闲**双层
                                              # (读到数据即重置),默认 300。注意不是
                                              # 整请求总时长 —— 合法长流不会因总时长
                                              # 被腰斩;只有连接无字节超过该值才断

[routing]
max_attempts = 10                             # 单次模型调用的总尝试上限(默认 10,
                                              # 即初始 1 次 + 最多 9 次自动重试)。
                                              # 网络 / SSE 中断 / 5xx / 429 等瞬时
                                              # 错误在引擎内自动重试,烧满才报错

[model]
context_window = 128000                       # 模型上下文最大长度(token);自适应输出
                                              # 预算与压缩阈值都会跟着走。私端/新模型
                                              # 也可用 [context_windows] 表按模型配:
                                              # [context_windows]
                                              # "MiniMax-M3" = 1000000
reasoning_effort = "high"                     # 思考强度:low|medium|high|max|ultra。
                                              # 实际上 wire 前会按模型族钳制 ——
                                              # qwen 系最高 high、deepseek 到 max、
                                              # OpenAI(gpt-5/o 系)到 ultra
```

anthropic 段同样支持免 key 本地网关（`api_key = ""` + 显式 `base_url`，如 LM Studio）。

改完不用重启进程，配置监听器会热重载。

## 环境变量

| 变量 | 干什么 |
|------|--------|
| `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` | provider 凭据，起步二选一 |
| `ZHUYE_MODEL` | 覆盖模型规格，`=mock` 供离线运行 |
| `ZHUYE_HOME` | 数据目录，默认 `~/.zhuye` |
| `ZHUYE_BIN` | SDK 去哪找 `zhuye` 二进制(查找顺序:`options.bin` → `ZHUYE_BIN` → 包内二进制 → PATH,见 [sdk.md](sdk.md)) |
| `ZHUYE_MEMORY_BACKEND` | 记忆后端：默认 `markdown`（MEMORY.md 文件 + BM25 分节检索）；`=sqlite` 启用条目版本库（`memory.db` + FTS5 中文检索，MEMORY.md 退化为可编辑投影；首次打开自动幂等导入既有内容）。sqlite 模式下 compact 固化与后台文件整理暂禁用（自动写入待 M5b 经持久队列提供），新笔记带 workspace 归属、旧无归属笔记不再参加跨项目召回 |
| `RUST_LOG` | 日志过滤，默认 `warn,zhuye=info` |

其余环境变量散落在各 crate 里，源码里搜 `ZHUYE_` 前缀即可。
