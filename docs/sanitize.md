# 输出脱敏(Sanitize)

> 实现见 `zhuye-tools/src/sanitize/mod.rs` 与
> `zhuye-sanitize`(独立 pattern 库);配置 schema 见
> `zhuye-config/src/schema/mod.rs::SanitizeSection`。

## 1. 目的

工具输出 / rollout 持久化 / 遥测三处统一走 `Sanitizer`:在**离开进程
边界之前**把密钥类内容替换为占位,防止 API key、私钥等随 JSONL 落盘
或进入 LLM 上下文。

默认 marker 是 `[REDACTED]`。多数 pattern 会带上类型后缀写成
`[REDACTED:<type>]`,便于事后定位是哪种密钥;少数几个只替换值本身、
保留周边结构。

## 2. 默认 pattern(10 类)

按实际匹配顺序排列(`default_patterns()` 的返回顺序):

| id | 命中形态 | 替换 |
|---|---|---|
| `AWS_ACCESS_KEY` | `AKIA` / `ASIA` 前缀 + 16 位大写字母数字(共 20 位) | `[REDACTED:aws_key]` |
| `ANTHROPIC_KEY` | `sk-ant-…`(≥20) | `[REDACTED:anthropic_key]` |
| `OPENAI_KEY` | `sk-…`(≥20) | `[REDACTED:openai_key]` |
| `GITHUB_TOKEN` | `ghp_` / `gho_` / `ghs_` / `ghr_` / `ghu_` 前缀(≥30) | `[REDACTED:github_token]` |
| `PRIVATE_KEY_BLOCK` | `-----BEGIN … PRIVATE KEY-----` 整块 | `[REDACTED:private_key]` |
| `JWT` | `eyJ….eyJ….…` 三段 | `[REDACTED:jwt]` |
| `DB_URL` | `postgres://` / `postgresql://` / `mysql://` / `mongodb://` / `mongodb+srv://` / `redis://` / `amqp://` / `amqps://` 连接串(含凭据部分) | scheme 保留 + `[REDACTED]` |
| `SLACK_TOKEN` | `xox[abprs]-…` | `[REDACTED:slack_token]` |
| `BEARER_TOKEN` | `Bearer …`(大小写不敏感) | `Bearer [REDACTED]` |
| `KEY_ASSIGN` | `(key|token|secret|password|credential|passwd|pwd) = value` 赋值 | 键保留 + `[REDACTED]` |

后三类(`DB_URL` / `BEARER_TOKEN` / `KEY_ASSIGN`)用**裸 marker** ——
它们需要保留 scheme、`Bearer ` 前缀或键名,才能不破坏上下文可读性。

顺序敏感:具体 provider key 先于 `KEY_ASSIGN`;`ANTHROPIC_KEY` 先于
`OPENAI_KEY`(`sk-ant-…` 是 `sk-…` 的子集)。`KEY_ASSIGN` 的值字符类
排除 `[` `]`,保证 `[REDACTED]` 不会被二次匹配(Rust regex crate 无
look-around,用显式前缀捕获组代替)。

## 3. 配置(config.toml `[sanitize]`)

```toml
[sanitize]
enabled = true                          # 总开关
marker = "[HIDDEN]"                     # 自定义替换占位符(默认 [REDACTED])
disable_default_patterns = false        # 关闭默认 10 类(高级用户)
extra_patterns = [                      # 追加自定义 regex(命中即整段替换)
  "(?i)\\bmy_token\\s*=\\s*\\S+",
]
```

`marker` 是**前缀**而不是完整占位符:自定义成 `[HIDDEN]` 时,带类型的
pattern 写成 `[HIDDEN:aws_key]`,裸 marker 的 pattern 写 `[HIDDEN]`。

regex 编译失败启动即报错(fail-fast,不静默跳过)。

## 4. 应用点

- 工具输出进上下文前(`ToolOutput` 序列化路径);
- rollout JSONL 写盘前(单条超 16 KiB 先截断再脱敏);
- 遥测 / LLM trace(`~/.zhuye/traces/model-io/`)写盘前。

密钥**来源**侧的防护是另一层:MCP 子进程 `env_clear()` 只传白名单
env;credential 支持 env 引用而非明文落 config。两层叠加 —— 来源
不泄漏 + 出口兜底。
