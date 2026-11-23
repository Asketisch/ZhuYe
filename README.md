<p align="center">
  <b>中文</b> · <a href="README.en.md">English</a>
</p>

# 烛夜 ZhuYe

烛夜是一个 AI agent 运行时框架,用 Rust 编写。你把它嵌进自己的产品,agent 就在你的产品里跑起来:读代码、改文件、调工具、调模型、开子代理。

它不是终端里的编程助手——那种产品用现成的就够了。烛夜是造 agent 的底盘:装好的产品是房子,烛夜是盖房子的那套框架。

## 快速开始

引擎要求 Rust 1.89(edition 2024)。先编出二进制:

```bash
cargo build -p zhuye-cli
ZHUYE_MODEL=mock ./target/debug/zhuye exec "hello"
```

`ZHUYE_MODEL=mock` 是显式的离线模型——不配 API key 也能跑,且不会被静默替换成别的 provider。想用真模型就设 `OPENAI_API_KEY` 或 `ANTHROPIC_API_KEY`。事件以 JSONL 走 stdout,tracing 日志走 stderr(`zhuye exec | jq` 不被日志污染)。

## 三种用法

**进程内链接(Rust)** —— 引擎是库,直接连进你的进程:

```rust
use zhuye::{Submission, Zhuye};

let agent = Zhuye::builder("mock").workspace(".").build()?;
let mut stream = agent.submit(Submission::user_input("hello")).await;
while let Some(event) = stream.next().await {
    let _ = event;
}
```

**headless 单轮** —— 跑一轮就退,事件 JSONL 流适合脚本与 CI:

```bash
ZHUYE_MODEL=mock ./target/debug/zhuye exec "重构这个模块"
```

**常驻会话(Python / TypeScript SDK)** —— `zhuye serve` 提供 stdio JSONL 会话服务,官方 SDK 在其上封装;宿主语言的函数注册个名字就能被模型调用,不用改写成 Rust(推理在 Rust 引擎,业务逻辑留在你的进程):

```python
from zhuye import ZhuyeAgent, ToolOutput, ContentBlock

agent = ZhuyeAgent.spawn()
agent.register_tool(
    "get_weather", "查询城市天气",
    {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
    lambda args: ToolOutput(content=[ContentBlock(type="text", text=f"{args['city']} 晴")]),
)
for ev in agent.submit("北京天气如何?"):
    if ev["msg"]["type"] == "turn_complete":
        break
agent.close()
```

`pip install zhuye` 装 Python 包(纯标准库),`npm install zhuye` 装 TypeScript 包(API 同构)。接入细节见 [docs/sdk.md](docs/sdk.md)。

## 核心特性

- **嵌入产品,不是产品本身** —— 库门面、headless CLI、常驻 serve 三种用法,一套引擎。
- **宿主语言工具** —— Python / TS 函数注册即成 LLM 可调用的工具,经 serve 协议回程执行。
- **多 agent 是原语** —— 子代理、角色讨论、任务/团队、DAG 流水线、目标模式,均有独立 crate。
- **危险操作有闸门** —— 权限模式 + hooks(12 类事件 / 7 种决策)+ 系统级沙箱(macOS Seatbelt / Linux Landlock)。
- **协议冻结,SDK 可演进** —— Protocol v0 additive 扩展;双 SDK 对未知变体降级不崩。
- **会话留档可恢复** —— JSONL rollout,`--resume` 按 ID / 最近会话 / 序号续跑。

## 文档

| 文档 | 内容 |
|------|------|
| [docs/architecture.md](docs/architecture.md) | 架构:分层、协议、状态转移、设计决策 |
| [docs/features.md](docs/features.md) | 特性列表与边界 |
| [docs/cli.md](docs/cli.md) | CLI 参考:子命令 |
| [docs/sdk.md](docs/sdk.md) | SDK 与使用说明(Python / TS) |
| [docs/plugins.md](docs/plugins.md) | 插件使用(manifest、安全模型、WASM) |
| [docs/plugin-dev-guide.md](docs/plugin-dev-guide.md) | 插件开发指南 |
| [docs/configuration.md](docs/configuration.md) | 配置与环境变量 |
| [docs/development.md](docs/development.md) | 开发与发布流程 |
| [docs/eval-baseline.md](docs/eval-baseline.md) | 真实任务评测基线与前后对比 |
| [SECURITY.md](SECURITY.md) | 安全说明 |

## 许可

引擎与 CLI 是 [Apache-2.0](LICENSE),Python SDK 与 TypeScript SDK 各自的包清单为 MIT。仓库:<https://github.com/Asketisch/ZhuYe>。
