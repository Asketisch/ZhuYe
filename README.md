# 烛夜 ZhuYe

烛夜是一个 AI agent 的运行时引擎。你把它嵌进自己的产品，agent 就在你的产品里跑起来：读代码、改文件、调工具、调模型、开子代理。

它不是终端里的编程助手——那种产品用 Claude Code 或 Codex 就够了。烛夜是造 agent 的底盘：装好的产品是房子，烛夜是盖房子的那套框架。

English: [README_EN.md](README_EN.md).

## 快速开始

引擎要求 Rust 1.89（安全修复后的 rmcp / rusqlite 依赖面要求）。先编出二进制：

```bash
cargo build -p zhuye-cli
ZHUYE_MODEL=mock ./target/debug/zhuye exec "hello"
```

`ZHUYE_MODEL=mock` 是一个显式的离线模型，没配 API key 时它不会偷偷把你换掉。想用真模型就设 `OPENAI_API_KEY` 或 `ANTHROPIC_API_KEY`。事件走 stdout，日志走 stderr。

进程内直接链接（同样免 key）：

```rust
use zhuye::{Submission, Zhuye};

let agent = Zhuye::builder("mock").workspace(".").build()?;
let mut stream = agent.submit(Submission::user_input("hello")).await;
while let Some(event) = stream.next().await {
    let _ = event;
}
```

宿主语言里的工具留在宿主进程。SDK 会拉起 `zhuye serve`（可以用 `ZHUYE_BIN` 指定二进制），你注册的工具只在宿主这边执行：

```python
from zhuye import ZhuyeAgent, ToolOutput, ContentBlock

agent = ZhuyeAgent.spawn()
agent.register_tool(
    "get_weather",
    "查询城市天气",
    {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
    lambda args: ToolOutput(
        content=[ContentBlock(type="text", text=f"{args['city']} 晴")],
        is_error=False,
    ),
)
for ev in agent.submit("北京天气如何?"):
    if ev["msg"]["type"] == "turn_complete":
        break
agent.close()
```

`pip install zhuye` 装 Python 包（只依赖标准库），`npm install zhuye` 装 TypeScript 包。接入细节见 [docs/sdk.md](docs/sdk.md)。

## 它能做什么

- **嵌进自己的产品** —— 引擎是库，不是产品：链接进进程、跑一轮就退、或常驻会话服务，三种用法一套引擎。
- **工具留在宿主进程** —— Python / TypeScript 函数注册个名字就能被模型调用，不用改写成 Rust。
- **多 agent 是原语** —— 子代理、角色讨论、DAG 流水线、任务与团队，引擎自带，不用装扩展。
- **危险操作有闸门** —— 权限模式 + hooks + 系统级沙箱（macOS Seatbelt / Linux Landlock）。
- **上下文自己会收拾** —— 本地裁剪 → 摘要重写 → 活跃文件自动读回。
- **换模型不重写提示词** —— 提示词按「默认 → 厂商 → 具体模型」分层叠好，还支持 provider 候选链。
- **每次会话都留档** —— JSONL 全程记录，可回看、续跑、分叉、跨会话搜索。
- **会话之外也有抓手** —— 跨会话记忆、cron 任务、side-channel、操作审计。
- **git 是一等公民** —— checkpoint / rewind 快照回退；插件补分支 / PR 探测与每轮自动快照。

一轮对话在引擎里怎么流转见 [docs/how-it-works.md](docs/how-it-works.md)，完整能力说明与已知边界见 [docs/features.md](docs/features.md)。

## 文档

| 文档 | 内容 |
|------|------|
| [docs/how-it-works.md](docs/how-it-works.md) | 一轮对话里发生了什么 |
| [docs/features.md](docs/features.md) | 能力总览与边界 |
| [docs/cli.md](docs/cli.md) | CLI 参考：子命令、zhuye-import |
| [docs/configuration.md](docs/configuration.md) | 配置文件与环境变量 |
| [docs/architecture.md](docs/architecture.md) | 引擎怎么搭的：分层、数据流、门面映射 |
| [docs/sdk.md](docs/sdk.md) | SDK 接入：会话服务、远程工具、离线联调 |
| [sdks/PROTOCOL.md](sdks/PROTOCOL.md) | 客户端和服务端之间的 wire 协议 |
| [docs/tools-and-hooks.md](docs/tools-and-hooks.md) | 内置工具、hook 事件与决策、权限模式、沙箱 |
| [docs/prompts.md](docs/prompts.md) | 提示词库和分层覆盖 |
| [docs/plugin-dev-guide.md](docs/plugin-dev-guide.md) | 插件开发指南（社区向：从零到发布的完整教程） |
| [docs/plugins.md](docs/plugins.md) | 插件参考手册（manifest 字段、安全模型、API 索引；含 WASM 组件插件：多语言、进程内热插拔） |
| [wit/README.md](wit/README.md) | WASM 插件契约（`zhuye:plugin` WIT 世界、版本策略、语言矩阵） |
| [docs/plugin-marketplace.md](docs/plugin-marketplace.md) | 插件市场：manifest 与发布 |
| [docs/PLAN_MODE.md](docs/PLAN_MODE.md) | 只读规划模式 |
| [docs/sanitize.md](docs/sanitize.md) | 密钥脱敏 |
| [docs/release.md](docs/release.md) | SDK 发版（PyPI / npm） |
| [docs/development.md](docs/development.md) | 开发指南：构建、测试、门禁 |
| [AGENTS.md](AGENTS.md) | 代理协作约定与开发流程检查表 |

## 许可

引擎与 CLI 是 [Apache-2.0](LICENSE)，Python SDK 与 TypeScript SDK 各自的包清单为 MIT。仓库：<https://github.com/Asketisch/ZhuYe>。
