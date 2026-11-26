# zhuye-cli AGENTS.md

> **何时读我**：加或改 `zhuye` 二进制的子命令、旗标、参数互斥关系，或改动 CLI 层的行为时，读本篇。

## 模块概述

`zhuye` 二进制的入口 crate：clap 参数面、子命令分发，以及一批工具类子命令的实现。本 crate 是薄壳——公共库的公开面在 `zhuye` 门面 crate，不在这里。

## 核心文件导航

| 文件 | 一句话职责 |
|------|-----------|
| `src/main.rs` | `enum Command` 子命令枚举、全局参数与 dispatch 分发 |
| `src/commands/` | 16 个工具类子命令的实现（一文件一子命令），速览与契约见 [commands/AGENTS.md](src/commands/AGENTS.md) |
| `src/plugin/mod.rs` | `zhuye plugin` 安装 / 列表 / 启停 / 卸载（`skill` 安装亦由 plugin 模块承载） |
| `src/plugin/marketplace.rs` | `zhuye plugin marketplace` 增删 / 刷新 / 移除 |

## 本地契约与同步点

- **`enum Command`（`src/main.rs`）是 CLI 参数面的唯一权威**：子命令集合、旗标名、互斥关系、帮助文案都从这里出。`docs/cli.md` 的总览表是它的文档投影，不是另一份事实源。
- `doctor` 展示静态可解析模型的窗口、压缩阈值及来源；未解析模型应明确说明阈值尚未确定。
- **新增子命令的同步点**：① `src/commands/` 新增实现文件并在其 `mod.rs` 登记 + `src/main.rs` 的 `enum Command` 变体与 dispatch 分支；② `docs/cli.md` 的总览表与对应小节；③ `tests/functional/` 下的参数面用例（能力面统一用 Python 写）；④ 若因此新增了 crate，补 runtime 层篇的 crate 速览导航。
- **clap 外壳与逻辑下沉的分工**：`exec` / `serve` 的参数结构与业务逻辑都在 `zhuye-exec`，本 crate 只做投影与转发；工具类子命令在本 crate 内直接实现——16 个落 `src/commands/`（速览见其子目录篇），`plugin` 族落 `src/plugin/`。
- **本二进制不含 `tui` 子命令**：交互式 TUI 已拆至独立仓库，这里只提供 headless 与工具类子命令。
- **`marketplace remove` 是不可逆删除**：`src/plugin/marketplace.rs` 用 `std::fs::remove_dir_all` 递归删掉插件安装目录。改动这段代码时保留明确的危险行为提示注释，并确保待删路径不能被未校验的外部输入决定。
- 改子命令参数面后按根 `AGENTS.md` 的验证矩阵跑 `./tests/run_all.sh rust`（CLI 参数面）与 `./tests/run_all.sh functional`（能力面）。

## 禁止事项

1. 不要在 `enum Command` 之外另立子命令注册点——枚举是参数面的唯一权威。
2. 不要把 `exec` / `serve` 的业务逻辑写进本 crate——下沉到 `zhuye-exec`。
3. 不要新增 `tui` 子命令——交互式 TUI 属于独立仓库。
4. 不要在改动 marketplace 移除逻辑时去掉危险行为提示，也不要让待删目录的路径可被未校验的外部输入决定。
5. 不要在 CLI 里硬编码配置或数据目录路径——一律走 `zhuye-config` 解析。
6. 不要绕过 `src/test_home.rs` 的 HOME 串行锁——多个改 `$HOME` 的测试并发会互相污染。

## 注意事项

- `src/main.rs` 的 `Cli` 结构带全局 `--cwd` 参数，在 dispatch 之前用 `set_current_dir` 应用；子命令内部再读 `current_dir()` 才是一致的。
- 新增或删除子命令后，`docs/cli.md` 的总览表与 `tests/functional/test_cli.py` 要同步更新，别只改代码。
- CLI 层只做参数解析与转发，库能力一律经 `zhuye` 门面的 re-export 引用，不要在 CLI 里重复实现。
- 注释与文档一律中文；应用层错误用 `anyhow`。

## 深入阅读

- [docs/cli.md](../../../docs/cli.md) —— `zhuye` 二进制子命令全量一览
- [docs/architecture.md](../../../docs/architecture.md) —— §1 框架边界与入口
