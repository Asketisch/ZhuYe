//! 5 类 plugin capability 的解析器。
//!
//! 每个 loader 函数:
//! - 接收 `&PluginManifest` + `install_path` + 各 spec 字段对应的来源
//! - 扫描/解析 plugin 文件,返回 `LoadedCapability` 子结构
//! - **不**直接调用 `ToolRegistry` / `HookEngine` 等 —— 实际注册由
//!   `crate::loader::register` 在挂载阶段统一做。
//!
//! 这样做的好处:loader 是纯函数,易于测试,且不引入对其他 crate 的依赖。
//! "已扫描项 → 实际 registry"的映射集中在 `loader.rs::register`,
//! loader 函数保持稳定。

pub mod agents;
pub mod commands;
pub mod hooks;
pub mod mcp;
pub mod skills;

use std::path::PathBuf;

use crate::PluginId;
use crate::manifest::PluginManifest;

/// 一次 plugin load 的综合报告 —— 5 类能力的扫描结果。
///
/// 各字段都是 `Vec` 而非 `HashMap` 以保留顺序。`loader::register` 挂载时
/// 遍历这个结构,逐项注册。
#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    pub plugin_id: PluginId,
    pub install_path: PathBuf,
    pub commands: Vec<commands::LoadedCommand>,
    pub agents: Vec<agents::LoadedAgent>,
    pub hooks: Vec<hooks::LoadedHook>,
    pub skills: Vec<skills::LoadedSkill>,
    pub mcp_servers: Vec<mcp::LoadedMcpServer>,
    /// v1.x C3:WASM 组件工具(manifest `[[wasm_tools]]`)。组件实现
    /// `zhuye:plugin/zhuye-tool` 世界;loader 进程内实例化注册。
    pub wasm_tools: Vec<WasmToolLoaded>,
}

/// 单个 WASM 工具的已解析形态(路径已展开为绝对)。
#[derive(Debug, Clone)]
pub struct WasmToolLoaded {
    /// 组件文件绝对路径(`.wasm`)。
    pub path: PathBuf,
    /// 工具名(缺省取组件 describe() 的 name;此处是 manifest 显式名
    /// 用于错误报告与去重)。
    pub name: String,
    /// manifest 覆盖描述(describe() 优先)。
    pub description: Option<String>,
}

impl LoadedPlugin {
    /// 空 LoadedPlugin 配合具体 plugin_id 用 —— `scan_all` 内部构造。
    pub fn empty(plugin_id: PluginId, install_path: PathBuf) -> Self {
        Self {
            plugin_id,
            install_path,
            commands: Vec::new(),
            agents: Vec::new(),
            hooks: Vec::new(),
            skills: Vec::new(),
            mcp_servers: Vec::new(),
            wasm_tools: Vec::new(),
        }
    }
}

impl LoadedPlugin {
    /// 是否完全没有提供任何能力(纯元数据 plugin)。
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
            && self.agents.is_empty()
            && self.hooks.is_empty()
            && self.skills.is_empty()
            && self.mcp_servers.is_empty()
    }
}

/// 加载所有 5 类能力 —— 由 `loader.rs::load(plugin)` 调用。
///
/// 任何 loader 失败被收敛到 `Vec<CapabilityError>` —— 单个能力坏了
/// 不阻断其他能力的挂载,错误按 plugin 累积。
pub fn scan_all(
    plugin_id: &PluginId,
    manifest: &PluginManifest,
    install_path: &std::path::Path,
) -> (LoadedPlugin, Vec<CapabilityError>) {
    let mut loaded = LoadedPlugin::empty(plugin_id.clone(), install_path.to_path_buf());
    let mut errors = Vec::new();
    let plugin_name = plugin_id.name();

    // 命令
    match commands::load(&manifest.commands, install_path, plugin_name) {
        Ok(cmds) => loaded.commands = cmds,
        Err(e) => errors.push(CapabilityError {
            capability: "commands",
            message: e.to_string(),
        }),
    }
    // Agent(代理)
    match agents::load(&manifest.agents, install_path, plugin_name) {
        Ok(ag) => loaded.agents = ag,
        Err(e) => errors.push(CapabilityError {
            capability: "agents",
            message: e.to_string(),
        }),
    }
    // Hook(钩子)
    match hooks::load(&manifest.hooks, install_path) {
        Ok(hk) => loaded.hooks = hk,
        Err(e) => errors.push(CapabilityError {
            capability: "hooks",
            message: e.to_string(),
        }),
    }
    // Skill(技能)
    match skills::load(&manifest.skills, install_path) {
        Ok(sk) => loaded.skills = sk,
        Err(e) => errors.push(CapabilityError {
            capability: "skills",
            message: e.to_string(),
        }),
    }
    // WASM 工具(v1.x C3):路径展开为绝对,缺失字段到 loader 期报错
    // (组件加载需要 wasmtime,scan 阶段只收集声明)。
    for (i, spec) in manifest.wasm_tools.iter().enumerate() {
        let Some(rel) = spec.path.as_deref() else {
            errors.push(CapabilityError {
                capability: "wasm_tools",
                message: format!("第 {i} 条缺 path"),
            });
            continue;
        };
        loaded.wasm_tools.push(WasmToolLoaded {
            path: install_path.join(rel),
            name: spec
                .name
                .clone()
                .unwrap_or_else(|| format!("wasm_tool_{i}")),
            description: spec.description.clone(),
        });
    }
    // MCP(服务器)
    match mcp::load(&manifest.mcp_servers, install_path, plugin_name) {
        Ok(mc) => loaded.mcp_servers = mc,
        Err(e) => errors.push(CapabilityError {
            capability: "mcp_servers",
            message: e.to_string(),
        }),
    }

    (loaded, errors)
}

/// 单类能力加载失败的描述 —— 聚合到 `LoadedPlugin.errors`,由 UI 显示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityError {
    pub capability: &'static str,
    pub message: String,
}

/// 展开能力声明里的 `${PLUGIN_ROOT}` 占位符为插件安装目录绝对路径。
///
/// 插件安装后会被复制到 `~/.zhuye/plugins/cache/...`,manifest 里写的
/// 相对路径不再成立;命令 / hook / MCP server 的可执行入口因此约定用
/// `${PLUGIN_ROOT}` 引用插件内文件(对齐 Claude Code 的
/// `${CLAUDE_PLUGIN_ROOT}` 约定)。
pub(crate) fn expand_plugin_root(s: &str, plugin_root: &std::path::Path) -> String {
    s.replace("${PLUGIN_ROOT}", &plugin_root.to_string_lossy())
}
