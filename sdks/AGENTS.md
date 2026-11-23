**何时读我:** 当你要改动 Python / TypeScript 双 SDK 的公开 API、事件类型层、协议适配或发布文档时读本篇 —— 本篇说明双 SDK 的同构约束、wire 语义权威来源与发布门禁。

# sdks/ —— 双官方 SDK(Python / TypeScript)

## 目录职责

本目录是两个官方客户端 SDK 与其共同协议依据 —— 底层都是 spawn `zhuye serve` 子进程、走 stdio JSONL wire,推理与编排在 Rust 引擎,业务逻辑留在宿主语言。

> 本篇正文里的文件路径一律相对仓库根目录。

## 分区速览表

| 分区 | 一句话职责 |
|---|---|
| `sdks/PROTOCOL.md` | serve wire 语义的权威说明,双 SDK 的共同实现依据(另有 `sdks/PROTOCOL.en.md` 英文版) |
| `sdks/python/` | Python SDK 包:纯标准库实现,`pip install zhuye` |
| `sdks/typescript/` | TypeScript SDK 包:零运行时依赖,`npm install zhuye` |
| `sdks/python/README.md`、`sdks/typescript/README.md` | PyPI / npm 页面渲染的正式发布文档(另有 `README.en.md`) |

## 各分区本地契约

### 双 SDK 同构(`sdks/python/` 与 `sdks/typescript/`)

- `sdks/python/zhuye/agent.py` ↔ `sdks/typescript/src/agent.ts`:两边的公开 API、选项字段、事件语义逐项对齐。**改一边必须看另一边**,不要只改一处。
- 类型层同样成对:`sdks/python/zhuye/protocol.py` ↔ `sdks/typescript/src/protocol.ts`。
- 两侧零运行时第三方依赖(Python 仅标准库,TS 仅 Node 内置模块);新增依赖前先确认真有必要。

### wire 语义权威(`sdks/PROTOCOL.md`)

- 改 wire 语义**先改 `sdks/PROTOCOL.md`**,再改双 SDK 类型层与实现。
- 三个协议面只有一套定义:Submission(Op)、Event(EventMsg)、RolloutRecord,判别符统一为 snake_case 字符串。
- 英文版 `sdks/PROTOCOL.en.md` 不参与自动门禁,但协议变更后仍须同步翻译。
- 前台 `bash` 成功启动后的 `tool_call_end.output.metadata.network_restriction` 报告实际网络限制；它是开放 metadata 内的附加字段，其他工具和失败启动不保证存在。
- `ReadMcpResource` 的内容块与 `metadata.content_descriptors` 按服务端顺序对应，二进制资源以含 base64 `blob` 的 JSON 文本块传递。

### 未知变体降级(两侧类型层)

- Rust 核心新增变体时,SDK 必须**降级不崩**:未知 EventMsg 收敛为 `UnknownEventMsg`(开放透传,保留原始 `type` 与其余字段),不得改成固定占位的 unknown 字段,也不得丢弃原字段。
- 未知 `"type"` 的容忍是客户端义务;两侧的 `UnknownEventMsg` 语义要保持一致。

### 二进制解析顺序(`sdks/python/` 与 `sdks/typescript/`)

解析优先级固定为:

```
options.bin  →  env ZHUYE_BIN  →  随包安装的平台子包内嵌二进制  →  PATH 上的 zhuye
```

- Python 侧:`sdks/python/zhuye/agent.py` 的 `ZhuyeAgent.spawn`;内嵌二进制由同文件的 `_bundled_binary()` 解析(平台 wheel 自带)。
- TS 侧:`sdks/typescript/src/agent.ts` 的 `ZhuyeAgent.spawn`;内嵌二进制由 `sdks/typescript/src/binary.ts` 的 `resolveBundledBinary()` 解析,平台子包名由同文件的 `platformPackageName()` 给出。
- 内嵌二进制**优先于 PATH**:它与 SDK 版本严格配套;平台子包未落地时回退 PATH。
- 两边的解析顺序必须一致,不要只改一侧。

### 发布文档与门禁

- `./scripts/release_sdk.sh check_docs` 从 `crates/protocol/zhuye-protocol/src/op.rs` 提取 Op 全集,校验四处收录:`sdks/python/README.md`、`sdks/typescript/README.md`、`sdks/python/zhuye/protocol.py`、`sdks/typescript/src/protocol.ts`。
- 英文版 `sdks/python/README.en.md`、`sdks/typescript/README.en.md` **不在门禁范围**,但改了协议仍要同步翻译。
- 协议变更属 additive 时,新变体同样要进两份 README 的 Op 表与两个类型层,否则发布前直接失败。

### 测试归属

- SDK 纯逻辑测试(mock serve,随包发布)在 `sdks/python/tests/` 与 `sdks/typescript/tests/`。
- 真二进制绑定层在 `tests/sdk-python/` 与 `tests/sdk-ts/`,见 [tests/AGENTS.md](../tests/AGENTS.md)。
- 协议 / 能力断言不写在 SDK 测试里(那属于能力覆盖层)。

## 禁止事项

- 不在 SDK 内自行新增或改写 wire 变体名 —— 变体集由 `crates/protocol/zhuye-protocol` 唯一定义。
- 不让 SDK 直接依赖 Rust 内部 crate:SDK 只经 `zhuye serve` 子进程 + JSONL 交互(Python wheel 走 maturin `bindings = "bin"` + `sdks/python/bindings-stub`;TS 走 npm 平台子包)。
- 不解析或改写 `zhuye serve` 的 stdout 非协议内容:stdout 承载协议 JSONL,tracing 走 stderr。
- 不在 SDK 侧硬编码协议版本号或服务端行为细节,这类事实以 `sdks/PROTOCOL.md` 与协议层源码为准。
- 不在 SDK 测试里重复写协议 / 能力断言。

## 深入阅读

- [PROTOCOL.md](PROTOCOL.md) —— serve wire 语义的权威说明
- [docs/sdk.md](../docs/sdk.md) —— SDK 接入指南与三种用法总览
- [python/README.md](python/README.md) / [typescript/README.md](typescript/README.md) —— PyPI / npm 发布页正式文档
- [scripts/release_sdk.sh](../scripts/release_sdk.sh) —— 版本一致性校验与 `check_docs` 文档门禁
