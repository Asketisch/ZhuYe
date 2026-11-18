# protocol 层 AGENTS.md

> **何时读我**：在 `crates/protocol/` 下工作——新增或修改跨层数据类型、动 `Op` / `EventMsg` / `Submission` / `Item` 的枚举形态、或排查「上层引的类型在协议层找不到」这类跨层不一致时，读本篇。

## 层定位

仓库最底的一层，也是唯一的一层契约层：这里只定义各层共用的**数据类型与 trait**，不含任何运行时行为（不跑事件循环、不碰 IO）。`docs/architecture.md` §3「协议与会话生命周期」是这一层的完整系统说明。

## crate 速览

| crate | 一句话职责 | 本地契约在哪 |
|-------|-----------|-----------|
| `zhuye-protocol` | 客户端与引擎之间的公共契约：`Submission` / `Op` / `Event` / `EventMsg` / `Item` 与 `RolloutRecorder` trait | [zhuye-protocol/AGENTS.md](zhuye-protocol/AGENTS.md) |

## 层内依赖约定

- 本层当前只有一个 crate；`zhuye-protocol` 不依赖仓库内任何其他 crate（见其 `Cargo.toml`），因此它不可能反向引用上层。
- 反向依赖是禁止的：协议层不得出现 abilities / resources / orchestration / integrations / runtime 层的任何类型。
- 新增「多个上层都要用」的共享类型时，优先放本层——`ToolError` 就是这么定义的，用来打断 tools ↔ hooks 的循环依赖。
- 只有跨层复用的东西才配进本层；只在一个上层内部流转的类型留在那个上层。

## 跨层边界

- 上层引用本层类型是允许的；本层引用上层类型不是。破坏这一条就会形成 crate 级循环，`cargo check --workspace` 直接失败。
- 本层不关心「谁消费这些类型」：上层如何组装、什么时候发事件，都不在本层的契约范围内。
- wire 语义（serde 标签、字段名、变体名）在本层定死；上层只做透传，不得再包一层改名字段。
- 上层绕开本层自造一份「差不多」的协议类型，等于凭空多出第二套契约——不要这么做。

## 本层通用规则

1. **协议 v0 已冻结**：只允许 additive 新增变体，既有变体不可改字段名、改类型、改语义。
2. **枚举形态就是门禁输入**：`scripts/release_sdk.sh` 的 `check_docs` 会解析 `Op` 的变体全集，解析不到就 FAIL。改动 `Op` 枚举形态前先读该脚本的解析规则。
3. 新增 Op / EventMsg 变体后的完整同步链路（双 SDK 类型层、双 README 协议表、`sdks/PROTOCOL.md`）写在 [zhuye-protocol/AGENTS.md](zhuye-protocol/AGENTS.md)，本篇不重复。
4. 改本层后必跑 `./scripts/release_sdk.sh check_docs`——文档落后协议会在发布前直接失败。
5. 注释与文档一律中文；库 crate 的错误类型用 `thiserror`。

## 深入阅读

- [docs/architecture.md](../../docs/architecture.md) —— §3 协议与会话生命周期、§2 workspace 六层分层
- [sdks/PROTOCOL.md](../../sdks/PROTOCOL.md) —— wire 语义与双 SDK 的对应关系
