**何时读我**:你要新增 / 修改插件 manifest 字段、插件安装与卸载流程、插件能力(命令 / agent / hook / skill / MCP / WASM)的扫描与挂载时,先读本篇。

# zhuye-plugin —— 插件 manifest、lifecycle 与能力 loader

## 本 crate 是什么

插件全生命周期:manifest 解析与校验、安装与卸载、状态记账、能力扫描与挂载、marketplace 拉取。
运行时装配入口在 `src/runtime.rs`(`bootstrap_plugins` 启动期挂载、`reload_plugins` 配置变更时 diff 同步挂卸),exec / serve / 门面 Builder / Python 绑定共用这一套入口。

## 本地契约

### 1. manifest schema 的唯一权威在 `src/manifest.rs`

- manifest 字段与嵌套结构(`PluginManifest`、`PluginDependency`、`McpServerSpec`、`SkillSpec`、`WasmToolSpec`、`UserConfigOption`)都在 `src/manifest.rs` 定义;新增字段在此加,并同步 `docs/plugin-dev-guide.md`。
- **未知字段当前不校验**:本 crate 没有启用 `deny_unknown_fields`,顶层未知键被 serde 静默丢弃;嵌套对象(`userConfig` / `mcpServers` 等)同样没有该属性,键名写错一样不会报错,只会**静默失效**。(manifest 模块头声称「嵌套保持 strict、错拼会被 `ManifestParse` 捕获」,与实现不一致 —— 写文档时不要把它写成「设计如此」。)
- 依赖声明 `dependencies` 支持三种字面量形态,版本后缀会被剥离;解析与环检测由 `dependency.rs` 负责。
- `user_config` 字段**已定义、当前无消费方** —— 全仓没有读它的代码,插件拿不到用户填的值;接线前先确认设计意图(见 [docs/plugin-dev-guide.md](../../../docs/plugin-dev-guide.md))。

### 2. 安装模型 = 缓存复制,不是链接也不是增量同步

- `install_local` 的流程:找 manifest → 解析 → 构造 id 与版本 → 检查已装 → `copy_dir_all` 复制到 cache 目录(`<root>/cache/<marketplace>/<plugin>/<version>/`)→ 写 `installed_plugins.json` 条目。
- **装完就是一份独立副本**:改源目录不会反映到已装的插件上,**迭代插件必须 reinstall**。
- 同 `(id, scope)` 重复安装 → `PluginError::AlreadyInstalled`(报错文案「插件已安装」);cache 目录已存在 → `PluginError::InstallDirConflict`。
- marketplace 的 git fetcher 在目标目录已存在时先删后 clone(简化 reinstall 路径);远端参数会校验,拒绝以 `-` 开头的值(否则会被 git 当成选项执行任意命令)。
- 插件状态三文件的关系(装了什么 / 启用什么 / 企业 pin)见 [crates/resources/AGENTS.md](../AGENTS.md)。

### 3. 扫描与挂载分离

- `capabilities::scan_all` 是纯函数:只扫不挂,返回 `LoadedPlugin`(按类别分组)与一组 `CapabilityError`。
- `loader::register` / `loader::unregister` 负责真接到各 registry(工具 / hook / MCP / 技能 / 子代理 / 命令表)。
- 能力类别共六类:五个 loader 文件(`capabilities/agents.rs`、`commands.rs`、`hooks.rs`、`mcp.rs`、`skills.rs`)+ WASM 组件工具(manifest `[[wasm_tools]]`,扫描在 `capabilities/mod.rs`,进程内实例化在 `loader.rs`;未启用 `wasm` feature 时跳过并告警)。

## 核心文件导航

| 文件 | 一句话职责 |
|---|---|
| `src/manifest.rs` | manifest schema 唯一权威 |
| `src/identifier.rs` | `PluginId` / `MarketplaceName` 标识符 |
| `src/manager/mod.rs` | 安装 / 卸载与状态记账 |
| `src/state.rs` | 持久化状态模型与 scope |
| `src/capabilities/` | 五类能力的纯函数扫描器 |
| `src/loader.rs` | 扫描结果 → 各 registry 的挂载 / 反挂 |
| `src/runtime.rs` | 启动期装配与 reload 同步 |
| `src/marketplace/` | marketplace 拉取(多种 source) |

## 禁止事项

- 不得新增 manifest 字段到别处 —— 只在 `src/manifest.rs` 定义。
- 不得假设未知字段会报错(当前会被静默丢弃)。
- 不得在安装流程里引入符号链接或增量同步语义 —— 安装就是缓存复制。
- 不得让 loader 直接调用各 registry —— 扫描归扫描,挂载归 `loader::register`。

## 深入阅读

- [docs/plugin-dev-guide.md](../../../docs/plugin-dev-guide.md) —— manifest 字段表、目录布局、hook / 命令 / agent 写法
- [docs/plugins.md](../../../docs/plugins.md) —— 插件体系总览与生命周期
- [docs/plugin-marketplace.md](../../../docs/plugin-marketplace.md) —— marketplace 清单与拉取
- [crates/resources/AGENTS.md](../AGENTS.md) —— 资源层的定位与本层通用规则
