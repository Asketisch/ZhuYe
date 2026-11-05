# 烛夜 ZhuYe

烛夜是一个 AI agent 的运行时引擎。你把它嵌进自己的产品，agent 就在你的产品里跑起来：读代码、改文件、调工具、调模型、开子代理。

它不是终端里的编程助手——那种产品用 Claude Code 或 Codex 就够了。烛夜是造 agent 的底盘：装好的产品是房子，烛夜是盖房子的那套框架。

English: [README_EN.md](README_EN.md).

## 一轮对话里发生了什么

你给一句话，引擎把它和当前的工具清单一起发给模型。模型要么回话，要么要求调用某个工具；引擎执行工具、把结果送回模型，循环继续。模型不再要工具的时候，这轮就结束了。

中间没有黑魔法。整个循环就是 `PreLoop → ModelCall → ToolExec → PreLoop → CheckStop`，每一步都往外发事件，你的界面照着事件画就行。

## 你能用它做什么

**嵌进自己的产品。** 同一套 Rust 引擎有三种用法：直接链接进你的进程、跑一轮就退出、或者维持一个会话服务让客户端连上来。引擎是库，不是产品。

**工具留在你自己的进程里。** 你在 Python 或 TypeScript 里写的函数，不用改写成 Rust。注册个名字和参数格式，模型要用的时候引擎把请求发回你的进程，函数跑完把结果回执。

**多 agent 是原语，不是插件。** 子代理、角色讨论、声明式的任务流水线、任务与团队，都是引擎自带的编排能力，不用先装什么扩展。

**换模型不用重写提示词。** 每家模型有各自的脾气，内置提示词按「默认 → 厂商 → 具体模型」分层叠好，换模型自动换用适合它的那套文案；还能配一条候选链，某个 provider 挂了自动换下一个。

**上下文自己会收拾。** 快到上限时先在本地裁剪，实在压不下去才叫摘要模型重写一遍；压缩完再把还活跃的文件读回来。

**危险操作有闸门。** 权限模式决定哪些操作要问你、哪些直接放行、哪些直接禁止；hooks 能在工具执行前后改参数、拦调用、插话；`bash` 默认跑在系统沙箱里（macOS 用 Seatbelt、Linux 用 Landlock），沙箱起不来就直接失败，不会偷偷退回裸 shell。

**每次会话都留档。** 全过程写成 JSONL，随时回看、接着跑、分叉出新分支、跨会话搜索，还能导出成带用量统计的 Markdown。

**会话之外也有抓手。** `memory` 工具让模型把经验写进跨会话记忆；side-channel 让用户发起的并发 agent 与主轮次并行互不干扰；cron 任务跨重启持久化；每轮结束落一条操作审计（Activity），谁在何时做了什么可回查。

**git 是一等公民。** 内置 checkpoint / rewind（打 tag 的快照，回退之后依然可达）；插件再加分支 / PR 探测和每轮自动快照。

## 快速开始

引擎要求 Rust 1.85。先编出二进制：

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

`build()` 会把压缩、记忆、技能和会话记录都装上。

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

## 命令行

`zhuye` 有 19 个子命令，常用的几个：

| 命令 | 干什么 |
|------|--------|
| `exec` | 跑一轮就走，事件以 JSONL 写 stdout |
| `serve` | 常驻的会话服务，SDK 走的就是它 |
| `config` | 看 / 改 `~/.zhuye/config.toml`，改完热重载 |
| `session` | 列会话、看内容、删、fork、跨会话搜索 |
| `plugin` / `mcp` / `lsp` | 装扩展、接 MCP server、接语言服务器 |
| `models` | 看当前 provider 有哪些模型可用，清单实时取自接口 |
| `discussion` / `pipeline` / `task` | 跑多 agent 讨论、DAG 流水线、任务与团队 |

完整列表看 `zhuye --help`，参数看 `zhuye <命令> --help`。

另外有个独立二进制 `zhuye-import`，把 Claude Code / Codex / ZCode 的历史会话导入成烛夜会话，导入后就能 `zhuye exec --resume` 接着跑。

## 配置

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
```

改完不用重启进程，配置监听器会热重载。完整的配置项见 [`zhuye-config`](crates/resources/zhuye-config/src/schema/mod.rs)。

## 环境变量

| 变量 | 干什么 |
|------|--------|
| `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` | provider 凭据，起步二选一 |
| `ZHUYE_MODEL` | 覆盖模型规格，`=mock` 供离线运行 |
| `ZHUYE_HOME` | 数据目录，默认 `~/.zhuye` |
| `ZHUYE_BIN` | SDK 去哪找 `zhuye` 二进制 |
| `RUST_LOG` | 日志过滤，默认 `warn,zhuye=info` |

其余环境变量散落在各 crate 里，源码里搜 `ZHUYE_` 前缀即可。

## 文档

| 文档 | 内容 |
|------|------|
| [docs/architecture.md](docs/architecture.md) | 引擎怎么搭的：分层、数据流、门面映射 |
| [docs/sdk.md](docs/sdk.md) | SDK 接入：会话服务、远程工具、离线联调 |
| [sdks/PROTOCOL.md](sdks/PROTOCOL.md) | 客户端和服务端之间的 wire 协议 |
| [docs/tools-and-hooks.md](docs/tools-and-hooks.md) | 内置工具、hook 事件与决策、权限模式、沙箱 |
| [docs/prompts.md](docs/prompts.md) | 提示词库和分层覆盖 |
| [docs/plugins.md](docs/plugins.md) | 插件开发与使用 |
| [docs/plugin-marketplace.md](docs/plugin-marketplace.md) | 插件市场：manifest 与发布 |
| [docs/PLAN_MODE.md](docs/PLAN_MODE.md) | 只读规划模式 |
| [docs/sanitize.md](docs/sanitize.md) | 密钥脱敏 |
| [docs/release.md](docs/release.md) | SDK 发版（PyPI / npm） |
| [AGENTS.md](AGENTS.md) | 开发指南与项目约定 |

## 开发

```bash
make check                                            # cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./tests/run_all.sh functional                         # 能力覆盖层
./scripts/e2e.sh                                      # 端到端冒烟
```

examples 和 e2e 都走 `ZHUYE_MODEL=mock` 离线模型，不需要 API key。

## 边界

- 这个仓库的二进制没有 TUI 子命令。交互界面在 [Reflect-TUI](https://cnb.cool/Demon1019/Reflect-CLI) 仓库。
- 沙箱在 macOS 上默认断网，Linux 上只限制写、不切网络。
- 同一个会话里同时在途的子代理最多 16 个，子任务结束就释放名额。

## 许可

引擎与 CLI 是 [Apache-2.0](LICENSE)，Python SDK 与 TypeScript SDK 各自的包清单为 MIT。仓库：<https://github.com/Asketisch/ZhuYe>。
