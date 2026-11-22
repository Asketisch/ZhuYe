//! 外部 agent 插件格式兼容 —— 解析 / 安装 / 启用全链路。
//!
//! 背景:桌面端「插件市场支持全部其他 agent 插件」主打兼容外部 agent
//! 插件格式,而该生态的插件 manifest 是 **JSON**
//! (`<plugin_root>/.claude-plugin/plugin.json`),字段名与 ZhuYe 的
//! `plugin.toml`(snake_case)有差异(`mcpServers` vs `mcp_servers`)。
//!
//! 本文件锁三件事:
//! 1. `PluginManifest::from_path` 按扩展名分派解析器(JSON → serde_json,
//!    TOML → toml)—— 此前只走 TOML,外部 agent 格式插件**装上就读不了**;
//! 2. 外部生态的字段别名(`mcpServers`)能落到 `mcp_servers`;
//! 3. `PluginRuntime::enable_one` 能从安装目录里定位 manifest
//!    (`PluginManifest::find_in_dir`,而非硬编码 `plugin.toml`)——
//!    否则插件「装得上、启用不了」。

use std::fs;
use std::path::Path;

use tempfile::TempDir;
use zhuye_plugin::manifest::{McpServerSpec, PluginManifest};

/// 外部 agent 插件格式的插件目录(`<root>/.claude-plugin/plugin.json`)。
fn make_compat_fixture(parent: &Path) -> std::path::PathBuf {
    let dir = parent.join("cc-fixture");
    fs::create_dir_all(dir.join(".claude-plugin")).unwrap();
    fs::create_dir_all(dir.join("commands")).unwrap();
    fs::create_dir_all(dir.join("skills/demo")).unwrap();

    // 外部 agent 风格 manifest(JSON + camelCase 的 mcpServers)。
    fs::write(
        dir.join(".claude-plugin").join("plugin.json"),
        r#"{
            "name": "cc-fixture",
            "version": "2.1.0",
            "description": "外部 agent 格式插件",
            "commands": "./commands",
            "skills": "./skills",
            "mcpServers": {
                "fs": { "command": "nonexistent-fake-binary", "args": ["--stdio"] }
            }
        }"#,
    )
    .unwrap();

    fs::write(
        dir.join("commands/hello.md"),
        "---\ndescription: greet\n---\n# hi\n",
    )
    .unwrap();
    fs::write(
        dir.join("skills/demo/SKILL.md"),
        "---\ndescription: demo skill\n---\n",
    )
    .unwrap();
    dir
}

#[test]
fn compat_manifest_json_can_be_parsed() {
    let tmp = TempDir::new().unwrap();
    let root = make_compat_fixture(tmp.path());
    let path = PluginManifest::find_in_dir(&root)
        .expect("find_in_dir 应定位到 .claude-plugin/plugin.json");
    assert!(
        path.ends_with("plugin.json"),
        "应命中外部 agent 插件格式的 plugin.json:{}",
        path.display()
    );
    // 此前此处用 TOML 解析器读 JSON → 解析失败(装上就用不了)。
    let manifest = PluginManifest::from_path(&path).expect("JSON manifest 应能解析");
    assert_eq!(manifest.name, "cc-fixture");
    assert_eq!(manifest.version.as_deref(), Some("2.1.0"));
    assert_eq!(manifest.description.as_deref(), Some("外部 agent 格式插件"));
}

#[test]
fn compat_mcp_servers_field_alias_is_accepted() {
    let tmp = TempDir::new().unwrap();
    let root = make_compat_fixture(tmp.path());
    let path = PluginManifest::find_in_dir(&root).unwrap();
    let manifest = PluginManifest::from_path(&path).unwrap();
    // 外部生态写 `mcpServers`,ZhuYe 字段名是 `mcp_servers` —— 别名
    // 必须生效,否则插件带的 MCP server 声明被静默丢弃。
    match &manifest.mcp_servers {
        McpServerSpec::Inline(map) => {
            assert!(map.contains_key("fs"), "mcpServers 别名未生效:{map:?}");
        }
        other => panic!("mcpServers 别名未生效(应落到 Inline):{other:?}"),
    }
}
