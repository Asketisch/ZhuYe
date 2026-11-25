**何时读我**:你要在 `scripts/` 下新增、修改或删除脚本,或要跑构建打包 / 冒烟 / 发版流程时,先读本篇。

# scripts —— 构建、冒烟、发布与插件打包脚本

## 本目录是什么

仓库的**自动化入口层**:把构建分发、插件打包、端到端冒烟、SDK 发布、基准采样、网关启停这些动作固化成脚本,让 CI 与本地用同一套命令。
脚本本身不实现产品能力 —— 产品能力在 `crates/`;脚本只负责编排、校验与打包。

## 脚本速览

本目录下的可执行脚本与工具型 Python 脚本**全部登记在下表**;新增脚本必须同时在此登记。

### 构建与分发

| 脚本 | 一句话职责 |
|---|---|
| `build.sh` | 跨平台构建 CLI 二进制,`--dist` 时按平台分发打包到 `dist/` |
| `build-dmg.sh` | macOS DMG 镜像打包(含 Finder 拖拽安装) |
| `build-targz.sh` | Linux / macOS 的 tar.gz 分发包打包 |
| `build-zip.sh` | Windows 的 zip 分发包打包 |

### 插件打包

| 脚本 | 一句话职责 |
|---|---|
| `build_computer_use_plugin.sh` | 构建 Computer Use 插件二进制 |
| `build_browser_plugin.sh` | 构建 Browser 插件二进制(CDP;装插件前必跑) |
| `build_git_tools_plugin.sh` | 构建 Git Tools 插件二进制 |
| `build_workflow_plugin.sh` | 构建工作流插件 MCP 服务 |
| `build_kms_plugin.sh` | 构建 KMS 插件二进制(装插件前必跑) |
| `build_shell_plugin.sh` | 构建 Shell 插件二进制(装插件前必跑) |
| `build_plugin_demo_wasm.sh` | 构建 plugin-demo 的 Rust WASM 组件样例(含工作流节点) |

### 冒烟与端到端

| 脚本 | 一句话职责 |
|---|---|
| `e2e.sh` | 端到端冒烟:构建 + 文档 + examples + headless + 全量测试 + clippy/fmt(引擎步骤跑一次性 `ZHUYE_HOME` 沙箱) |
| `sdk_smoke.sh` | Python / TypeScript SDK 对真实 `zhuye serve` 二进制的离线冒烟(整体 `ZHUYE_HOME` 沙箱) |
| `full_link.sh` | 全链路集成测试(只 mock LLM,贯通引擎完整链路;整脚本 `ZHUYE_HOME` 沙箱) |
| `discussion_smoke.sh` | 真实 LLM 的 discussion 编排冒烟(用真实数据目录读配置,**不隔离**) |

**数据目录隔离契约**:凡是会跑引擎进程(example / `exec` / `serve` /
discussion)的 mock 冒烟,必须把 `ZHUYE_HOME` 指向一次性临时目录(`mktemp -d`
+ trap 清理;`ZHUYE_HOME` 优先级高于 `$HOME/.zhuye`,见
`zhuye-config::paths`)—— 引擎默认 recorder 落 `$HOME/.zhuye/sessions/`,
mock 会话会在真实数据目录留下「只有 meta、无用户消息」的壳会话,污染桌面端
(TUI / GUI)的会话列表(实测 2026-11:一天 8 个 8 位 ID 空条目)。例外:
`discussion_smoke.sh` 走真实 provider 配置,隔离会破坏鉴权;`bench.py` 已
自带隔离。不能全局 export 的反例见 `e2e.sh` 头注(workspace 测试含断言
`$HOME/.zhuye/sessions` 形态的用例)。

### 发布

| 脚本 | 一句话职责 |
|---|---|
| `release_sdk.sh` | SDK 发布辅助:版本一致性校验、文档门禁、bump、打 tag |
| `create_tag.sh` | 一站式发版:校验 + 四处版本 bump + 提交 + 打 tag + 推送 |

### 基准

| 脚本 | 一句话职责 |
|---|---|
| `bench.sh` | 性能基线采样入口(薄包装,转调 `bench.py`) |
| `bench.py` | 采集隔离 mock 会话的启动、首事件、首文本、耗时与峰值内存 |

### 网关与演练

| 脚本 | 一句话职责 |
|---|---|
| `ofm_gateway.sh` | 本地免费模型网关的启停与状态查询助手 |
| `free_model_practice.sh` | 免费模型横向对比演练,输出 Markdown 对比表 |

### 提示词来源维护

| 脚本 | 一句话职责 |
|---|---|
| `check_prompt_sources.py` | 对比提示词方案固定版与上游分支(`zhuye-prompt/prompts/sources.json` 登记来源),产出维护者审核报告 |

### 门禁

| 脚本 | 一句话职责 |
|---|---|
| `check_brand_names.py` | 品牌名门禁:工作树与提交信息命中其它产品品牌名即失败(豁免只限外部生态格式标识符与厂商 API 名 / 模型 id) |
| `check_stale_doc_phrases.py` | 作废说法门禁:现状文档命中已作废表述即失败 |

- 两者均以 `python3 scripts/<name>.py` 运行;`check_brand_names.py` 还要扫描
  提交信息,需完整克隆(`git fetch --unshallow`)。品牌名登记表在
  `BRAND_PATTERNS`,豁免字面量在 `EXEMPT_PATTERNS` —— **豁免必须窄到具体
  字面量**,不得放宽成整类。`scripts/e2e.sh` 的「品牌残留扫描」一步会调用
  它(只扫工作树,不扫历史)。
- 另有一类**路径限定豁免** `EXEMPT_SCOPED_PATTERNS`(路径前缀 + 字面量):
  同一批字面量只在登记路径内不作数,作用域外照旧拦下。当前只登记提示词
  两级目录(`crates/abilities/zhuye-prompt/`、协议 config 文档注释与
  `docs/prompts*.md`)—— 那里出现的是上游公开数据集的目录名 / 展示名与
  **发给模型的策展正文**(格式标识符,非归属表述)。新增路径限定豁免必须
  写明依据,并确认作用域外的同名品牌仍会被拦下。

### 模型目录快照

| 脚本 | 一句话职责 |
|---|---|
| `fetch-models-dev.sh` | 拉取 models.dev api.json 生成 zhuye-llm 内嵌快照(`assets/api.json.gz` + `.meta` 抓取时刻) |

- 非可执行,以 `python3 scripts/check_prompt_sources.py` 运行;依赖 `gh` CLI 访问 GitHub API。
- 由 CI `.github/workflows/prompt-sources.yml` 定期调用;**只产出报告不自动改文件** —— 是否跟随上游更新由维护者决定,更新文案时须同步 `sources.json` 的 sha256(见 abilities 层篇 `zhuye-prompt` 条目)。

### 开发辅助(非脚本)

| 路径 | 一句话职责 |
|---|---|
| `dev/ime-test/` | 独立 Rust 小程序(不在主 workspace 构建面),隔离排查中文 IME 引发的终端 panic |

## 各分区本地契约

### 构建与分发

- `make dist` → `scripts/build.sh --dist` → 按平台转调 `build-dmg.sh`(macOS)/ `build-targz.sh`(Linux)/ `build-zip.sh`(Windows),产物落 `dist/`。
- `--dist` 强制用 release profile(分发要小体积);不带 `--dist` 时用 `release-fast` 供本地快速迭代。

### 插件打包

- 5 个 `build_*_plugin.sh` 与 `build_plugin_demo_wasm.sh` 的产物都拷进对应 `plugins/<name>/bin/`。
- 这与插件的「安装是缓存复制」模型直接相关:改插件源码后必须**重跑构建脚本并 reinstall**,否则装出去的仍是旧副本。
- WASM 样例是独立 cargo 项目(不进主 workspace 构建面),需先 `rustup target add wasm32-wasip2`。

### 冒烟与端到端

- `e2e.sh` 的步骤数以脚本内的 `step "N/13 ..."` 为准;增删步骤时同步改这个计数。
- `e2e.sh`、`sdk_smoke.sh`、`full_link.sh` 走离线 mock 模型(不需要真实 API key)。
- `discussion_smoke.sh` **例外**:它要求真实的 provider key,跑一次完整 discussion 流程验证编排;不能用 mock 结论冒充通过。

### 发布

- `release_sdk.sh` 的子命令:`check` / `check_docs` / `bump` / `tag`;版本四处同步:`Cargo.toml`(workspace package 版本)/ `sdks/python/pyproject.toml` / `sdks/python/zhuye/__init__.py`(`__version__`)/ `sdks/typescript/package.json`。
- `check_docs` 从 `zhuye-protocol` 源码提取 Op 全集,校验两份 SDK README 与类型层是否收录齐全;它的**解析口径(枚举边界怎么判定、变体怎么提取)以 [crates/protocol/zhuye-protocol/AGENTS.md](../crates/protocol/zhuye-protocol/AGENTS.md) 为准**,本篇不重复。
- `release_sdk.sh tag` 要求:本地版本已等于目标版本、工作区干净、当前分支是 main;推送 `v*` tag 会触发两条流水线(`release.yml` 发 CLI、`release-sdks.yml` 发 Python SDK 到 PyPI 与 TS SDK 到 npm),**公开发布且不可撤回**。
- `create_tag.sh` 是更前置的一站式脚本:它自己做前置校验、四处版本 bump、提交、打 tag、推送;`release_sdk.sh tag` 只负责「版本已就绪」之后的打 tag 与推送。两者触发的是同样的两条流水线。
- **测试阶段只跑 `check` / `bump`,不跑 `tag`,也不跑 `create_tag.sh`。**

### 基准

- `bench.sh` 只是 `bench.py` 的转调包装,两者指标口径一致(冷启动 / 首事件 / RSS)。
- 改基准实现或采集口径时,要给出与既有基线数值的对比说明,而不是只贴新数字。

### 模型目录快照

- `fetch-models-dev.sh` 产物是**原始 api.json 的 gzip**(不做字段裁剪),与运行时拉取零格式漂移 —— 聚合 / 投票都在 Rust 加载期完成(`zhuye-llm/src/models_dev.rs`)。
- 快照明显过期(新模型上线 / 发版前)时手动跑;脚本只写 crate `assets/`,不碰宿主状态。
- `.meta`(unix 秒)参与「磁盘缓存 vs 内嵌快照取新」判定,删了它等价于把快照视为远古。

### 网关与演练

- **`ofm_gateway.sh` 不执行 `npm install`** —— 安装依赖会修改宿主状态,必须由用户显式触发;脚本只做启停与状态查询。这是本目录「脚本不碰宿主状态」边界的样板。
- `free_model_practice.sh` 跑真实模型的横向对比,输出 Markdown 表。

## 禁止事项

- **不新增会改宿主状态的隐式操作**(如 `npm install`、`cargo install`、写全局配置)。这类动作必须由用户显式触发,脚本最多提示,不得代劳 —— 参照 `ofm_gateway.sh` 的既有边界。
- **新增脚本必须在本速览表登记**,否则目录里的脚本集合与本篇脱节,评审与排障都会失真。
- 不在测试 / 验证阶段跑 `release_sdk.sh tag` 或 `create_tag.sh`(会推送 tag 并触发公开发布);不把构建产物提交进仓库(`dist/`、插件 `bin/` 产物按各自约定处理)。
- 不改动冒烟脚本的断言口径来「让门禁变绿」—— 门禁红是真问题。

## 深入阅读

- [docs/release.md](../docs/release.md) —— SDK 发布流程、流水线与正式发布清单
- [docs/development.md](../docs/development.md) —— 开发流程与验证矩阵
- [docs/plugin-dev-guide.md](../docs/plugin-dev-guide.md) —— 插件构建产物与安装模型
- [docs/cli.md](../docs/cli.md) —— CLI 参数面
