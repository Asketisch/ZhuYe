# 实模型演练层(`tests/live/`)

这一层用**真实模型**跑烛夜的完整 agent 链路,补上 mock provider
永远测不到的行为:真实 SSE 帧、真实工具调用、真实限流(429)、真实
截断续作、真实思考过程。

## 与能力覆盖层的边界

| | 能力覆盖层 `tests/functional/` | 实模型层 `tests/live/` |
|---|---|---|
| 模型 | mock(`ZHUYE_MODEL=mock`) | 真实模型(清单接口实时拉取) |
| 断言 | 字段级、可逐字 | **协议形状级**,不断言回答文本 |
| 网络 | 全程离线 | 需要外网 + 免费额度 |
| 门禁 | **CI 强制**(`run_all.sh functional`) | 可选(`run_all.sh live`),默认 SKIP |

⚠️ **能力断言仍然只写在 `tests/functional/`** —— 本层只断言协议形状与
运行时行为,不重复能力覆盖。两层不重叠、不互相替代。

## 为什么需要它

能力覆盖层用 mock provider —— 确定性、离线、是 CI 门禁。但 mock 永远
测不到:真实 SSE 帧的形状、真实模型的工具调用质量、真实的 429 限流、
真实思考过程、真实的截断续作。

免费模型池让这些**零成本**地跑起来。

## 两条车道

| 车道 | 端点 | 凭据 | 前置条件 |
|---|---|---|---|
| **Kilo 免费池**(默认) | `https://api.kilo.ai/api/gateway` | 免 Key | 无 |
| **OpenCode inference** | `https://opencode.ai/inference/v1` | `Bearer public` | 必须经 dsh 网关 |

Kilo 车道**零外部依赖** —— 只改 `config.toml` 就能用。

OpenCode 车道有 **v2 客户端指纹门**:UA 必须是 `opencode/latest/2.0.26/cli`、
四个会话头、铸造形状的 session id、**逐字**的 `read` / `shell` 工具定义、
且绝不能下发 `tool_choice: none`。这些由 dsh-our-free-model 的本地网关
补齐(它还会把与官方定义不同的同名工具改名并在响应侧还原 —— 烛夜 自带
一个名为 `read` 的内置工具,走这条车道时会被改名再还原)。

```bash
# 启停网关(不自动 npm install —— 装依赖会改宿主状态,需显式触发)
cd ../dsh-our-free-model && npm install
scripts/ofm_gateway.sh start
scripts/ofm_gateway.sh status
```

## 模型从哪来(不写死)

免费模型集合由**上游**决定,随时可能增删 —— **写死模型 id 必然腐烂**。
烛夜 因此内置了**模型发现能力**:

```bash
zhuye models list --free-only          # 人看
zhuye models list --free-only --json   # 脚本消费
zhuye models pick --free-only          # 只打印一个 model id
```

「免费」的判定走**规则**(`zhuye_llm::FreeModelRules`),三通道:

1. **provider 显式标记** —— Kilo 的 `isFree: true`(权威);
2. **白名单** —— 不带 free 标记但确实免费的 id(默认三个,可配置);
3. **id 模式** —— 默认 `["-free", ":free", "/free"]`。

规则**不包含模型清单** —— 模型本身永远来自 provider 的清单端点。

配置里可以直接写 `model = "free"`:运行时查清单 → 按规则挑一个 →
写进运行时配置。事件流里看到的是**真实 model id**,不是占位符。

演练层的模型选取顺序:

1. `ZHUYE_LIVE_MODELS` 给了 → 用它;
2. 否则调 `zhuye models list --free-only --json` → 拿 provider 的实时清单;
3. 逐个做**能力探测**(带 `tools` 的最小请求)→ 不支持 tools 的模型标为
   **跳过**而不是失败;
4. 取前 N 个(`ZHUYE_LIVE_MODEL_COUNT`)。

```toml
[model_discovery]
enabled = true                    # 关掉则 model = "free" 直接报错
timeout_secs = 20
prefer = "code"                   # id 子串偏好(可空)
always_free = ["union-alpha", "big-pickle", "space-bunny-free"]
free_id_patterns = ["-free", ":free", "/free"]
```

## 配置模板

### Kilo 车道(默认,免 Key)

```toml
# ~/.zhuye/config.toml
[hooks]
enabled = []                      # 演练时关掉内置 hook(只验引擎行为)

[active]
provider = "openai"

[openai]
api_key = ""                      # 免 Key —— 空 key 不发 Authorization 头
base_url = "https://api.kilo.ai/api/gateway"
timeout_secs = 300                # ⚠️ 见下方「已知限制」
model = "free"                    # 运行时从清单里挑

[[openai.credentials]]
label = "kilo-free"
api_key = ""
base_url = "https://api.kilo.ai/api/gateway"
model = "free"

[routing]
max_attempts = 8
[routing.main]
primary = "openai/free"
fallbacks = ["openai/cohere/north-mini-code:free"]
```

### dsh 网关车道(OpenCode 车道)

```toml
[openai]
api_key = "<forwardKey>"           # ~/.our-free-model/settings.json
base_url = "http://127.0.0.1:18900"
timeout_secs = 300
model = "free"
```

### 跨模型降级链

`[routing].fallbacks` 是**跨模型**降级链:当前模型的凭据池全不可用
(全冷却 / 全被排除)时,自动切到链上的下一个模型继续本轮。

```toml
[routing.main]
primary = "openai/cohere/north-mini-code:free"
fallbacks = [
  "openai/nvidia/nemotron-3.5-lightning:free",
  "openai/minimax/minimax-m3.1-flash:free",
]
```

⚠️ `fallbacks` 里写 `"openai/free"` **不会**自动挑不同模型 —— `free`
每次都解析成同一个。真正降级要写具体 id(或用脚本按清单生成)。

切换时引擎会发 `routing(kind="spec_fallback")` 事件,`from_credential` /
`to_credential` 装的是 `"provider/model"` 形式的 spec。

## 跑法

```bash
# 默认跳过(不发起任何网络请求,退出码 0)
./tests/run_all.sh live

# 真跑
ZHUYE_LIVE=1 ./tests/run_all.sh live

# 指定车道(默认 Kilo 免费池:免 Key、无客户端指纹门)
ZHUYE_LIVE=1 \
ZHUYE_LIVE_BASE_URL=https://api.kilo.ai/api/gateway \
ZHUYE_LIVE_API_KEY= \
./tests/run_all.sh live

# 指定模型(逗号分隔);不指定则从清单里取
ZHUYE_LIVE=1 ZHUYE_LIVE_MODELS=cohere/north-mini-code:free ./tests/run_all.sh live
```

单轮手动跑:

```bash
zhuye exec "用一句话回答:1+1 等于几?"
zhuye models list --free-only
zhuye doctor --check-network          # 自检(含模型清单检查)
```

### 环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `ZHUYE_LIVE` | 空 | 设 `1` 才真跑(否则 SKIP) |
| `ZHUYE_LIVE_BASE_URL` | `https://api.kilo.ai/api/gateway` | provider 端点 |
| `ZHUYE_LIVE_API_KEY` | 空 | 免 Key 端点留空;dsh 网关填 `forwardKey` |
| `ZHUYE_LIVE_MODELS` | 空 | 逗号分隔模型 id;空则从清单取 |
| `ZHUYE_LIVE_MODEL_COUNT` | `3` | 从清单取几个 |
| `ZHUYE_LIVE_RETRIES` | `2` | 单个用例重试次数(限流是常态) |
| `ZHUYE_LIVE_RETRY_DELAY` | `5` | 重试间隔(秒) |
| `ZHUYE_LIVE_DRAIN_TIMEOUT` | `180` | 事件流等待预算(秒) |
| `ZHUYE_LIVE_TOKEN_BUDGET` | — | 预留:token 预算护栏 |

### 横向对比演练

```bash
scripts/free_model_practice.sh --count 3
scripts/free_model_practice.sh --models cohere/north-mini-code:free --out report.md
```

输出 Markdown 对比表(终态 / 耗时 / token / 工具调用数 / 错误分类)。
**不是门禁** —— 退出码只反映脚本自身错误。

## 断言口径

只断言**协议形状**:

- 事件序:`turn_started` → delta → `token_count` → `turn_complete`;
- `turn_id` 首尾一致;
- `usage` 四字段齐全且 `total_tokens > 0`;
- 工具调用成对(`tool_call_begin` / `tool_call_end` 数量相等);
- `turn_complete.status == "success"`。

**不断言具体回答文本** —— 真实模型的输出不确定,逐字断言只会制造假失败。
内容类需求一律用宽松判定或直接跳过。

上游限流(429)判为**跳过**而不是失败 —— 测的是代码,不是配额。

## 已知限制

| 限制 | 说明 | 缓解 |
|---|---|---|
| **配额是全局的** | 免费池 429 常见(OpenCode 车道按会话计额度,`retry-after` 可达 12h) | 用例自带重试;429 判为跳过;**矩阵必须串行** |
| **地区门 403** | 免费车道按出口 IP 判地区 | 换出口后重跑 |
| **模型集合随时变** | 上游随时下架某个 id | 清单每次实时拉取;fallback 链兜底 |
| **慢首字 / 思考静默** | 实测首字 >30s、思考阶段可静默 60~70s | `timeout_secs` **必须放大到 300** —— reqwest 的 timeout 是「建连→body 读完」的**总**时长,默认 60s 会把流式响应腰斩 |
| **思考过程字段名不一** | 多数端点用 `delta.reasoning_content`,免费池用 `delta.reasoning` | 烛夜 两个都认(`reasoning_content` 优先) |
| **usage 可能在结束原因之后** | 部分端点把 usage 放在尾部额外 chunk | 烛夜 收到结束原因后会在有界窗口内继续读 usage |
| **部分模型不支持 tools** | provider 直接 400 | 能力探测识别并跳过 |
| **prompt 可能被上游记录** | Kilo 模型卡明示 | **只用合成数据**,不要喂真实仓库内容 / 凭据 |
| **`context_window` 可能不披露** | OpenCode 原生车道只给 id | 用 `[context_windows]` 段显式声明 |

## 排障

**`no model configured for the active provider`**
→ 配置里没写 model。写具体 id,或写 `model = "free"`(需 `[model_discovery]
enabled = true`)。

**`cannot fetch model list` / `provider 不支持模型清单`**
→ 该 provider 没有清单端点。显式配置具体 model id。

**`auth failed`(401)**
→ 请求打到了**付费**模型上,或 key 无效。检查 model id 拼写;确认
`base_url` 指向免费池。

**`ALL_CREDENTIALS_EXHAUSTED`**
→ 池内凭据全不可用(常见于 429 全冷却)。配多个 credential,或用
`[routing].fallbacks` 做跨模型降级。

**思考过程看不到**
→ 端点用了第三种字段名(`delta.reasoning_details[]`),当前不解析 ——
见「已知限制」。

**流式响应被腰斩**
→ `timeout_secs` 太小。放大到 300。
