//! Hooks capability —— 解析 plugin 提供的 hook 配置。
//!
//! 来源(`manifest.hooks` 三种形态,`schemas.ts:348-373`):
//! - `None` —— 无 hook
//! - `Path("./hooks/extra.json")` —— 指向 hooks.json 文件
//! - `Inline(json)` —— 内联 JSON
//! - `Many([...])` —— 多源叠加
//!
//! 本模块只解析 JSON 结构(`LoadedHook`);把命令包装成
//! `ShellHook` / `WasmHook` 实例挂到 `HookEngine` 由
//! `crate::loader::register` 完成。每个 hook 声明给出事件名 + matcher +
//! 实际命令。

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::errors::{PluginError, Result};
use crate::manifest::HookSpec;

/// 解析后的 hook 配置 —— 承载 raw JSON 描述。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedHook {
    /// 来源:文件路径(若来自 Path 形态)或 `"<inline>"`。
    pub source: String,
    /// 事件名(如 `PreToolUse` / `PostToolUse` / `Stop`)。
    pub event: String,
    /// matcher 字符串(如 tool 名 glob),`None` 表示无 matcher。
    pub matcher: Option<String>,
    /// hook 命令(数组 / 字符串 / 命令对象)。
    pub command: serde_json::Value,
}

/// hooks.json 顶层结构 —— 简化版,只解析 `{hooks: {EventName: [{matcher, hooks: [...]}]}}`。
#[derive(Debug, Deserialize)]
struct HooksFile {
    #[serde(default)]
    hooks: HooksMap,
}

type HooksMap = std::collections::BTreeMap<String, Vec<HookMatcherEntry>>;

#[derive(Debug, Deserialize)]
struct HookMatcherEntry {
    #[serde(default)]
    matcher: Option<String>,
    #[serde(default)]
    hooks: Vec<HookCommandEntry>,
}

#[derive(Debug, Deserialize)]
struct HookCommandEntry {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
    /// hooks.json 可声明 `env: ["NAME", ...]` 显式放行本应被 secret
    /// 剔除的环境变量(白名单优先)。此前本字段缺失 —— `expand` 重建
    /// command JSON 时静默丢弃,loader 读到的永远是空白名单,声明了也不生效。
    #[serde(default)]
    env: Option<Vec<String>>,
    /// v1.x C3:WASM 组件 hook —— `"wasm": "${PLUGIN_ROOT}/bin/x.wasm"`。
    /// 与 `env` 字段同款教训:loader 按 `command.get("wasm")` 判定 wasm
    /// 路径,但本结构体此前没有该字段,`expand` 重建 JSON 时把它丢了,
    /// wasm hook 永远落进 shell 路径再因缺 `command` 被跳过 —— 整条
    /// wasm hook 链路从未生效。
    #[serde(default)]
    wasm: Option<String>,
}

pub fn load(spec: &HookSpec, plugin_root: &Path) -> Result<Vec<LoadedHook>> {
    let sources = match spec {
        HookSpec::None => return Ok(Vec::new()),
        HookSpec::Path(p) => vec![SourceOrJson::Path(plugin_root.join(p))],
        HookSpec::Inline(v) => vec![SourceOrJson::Inline(v.clone())],
        HookSpec::Many(items) => items
            .iter()
            .map(|s| match s {
                HookSpec::Path(p) => Ok(SourceOrJson::Path(plugin_root.join(p))),
                HookSpec::Inline(v) => Ok(SourceOrJson::Inline(v.clone())),
                HookSpec::None => Err(PluginError::Validation(
                    "HookSpec::Many 不允许嵌套 None".into(),
                )),
                HookSpec::Many(_) => Err(PluginError::Validation(
                    "HookSpec::Many 不允许嵌套(深度限制 1)".into(),
                )),
            })
            .collect::<Result<Vec<_>>>()?,
    };

    let mut out = Vec::new();
    for source in sources {
        let (label, json) = match source {
            SourceOrJson::Path(p) => {
                let label = p.display().to_string();
                let text = std::fs::read_to_string(&p).map_err(|e| PluginError::ManifestIo {
                    path: p.clone(),
                    source: e,
                })?;
                let json: serde_json::Value =
                    serde_json::from_str(&text).map_err(|e| PluginError::ManifestParse {
                        path: p.clone(),
                        message: e.to_string(),
                    })?;
                (label, json)
            }
            SourceOrJson::Inline(v) => ("<inline>".into(), v.clone()),
        };
        expand(&label, &json, plugin_root, &mut out)?;
    }
    Ok(out)
}

enum SourceOrJson {
    Path(PathBuf),
    Inline(serde_json::Value),
}

fn expand(
    source: &str,
    json: &serde_json::Value,
    plugin_root: &Path,
    out: &mut Vec<LoadedHook>,
) -> Result<()> {
    let parsed: HooksFile =
        serde_json::from_value(json.clone()).map_err(|e| PluginError::ManifestParse {
            path: PathBuf::from(source),
            message: e.to_string(),
        })?;
    for (event, entries) in parsed.hooks {
        for entry in entries {
            for cmd in entry.hooks {
                // 命令串里的 `${PLUGIN_ROOT}` 展开为插件安装目录 ——
                // 插件被复制进 cache 后相对路径不再成立。wasm 路径同款展开。
                let command = cmd
                    .command
                    .map(|c| super::expand_plugin_root(&c, plugin_root));
                let wasm = cmd.wasm.map(|w| super::expand_plugin_root(&w, plugin_root));
                let cmd_value = serde_json::json!({
                    "type": cmd.kind,
                    "command": command,
                    "timeout": cmd.timeout,
                    "env": cmd.env,
                    "wasm": wasm,
                });
                out.push(LoadedHook {
                    source: source.to_string(),
                    event: event.clone(),
                    matcher: entry.matcher.clone(),
                    command: cmd_value,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_none() {
        let tmp = TempDir::new().unwrap();
        assert!(load(&HookSpec::None, tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn load_path_reads_hooks_json() {
        let tmp = TempDir::new().unwrap();
        let hooks_path = tmp.path().join("hooks.json");
        std::fs::write(
            &hooks_path,
            r#"{
                "hooks": {
                    "PreToolUse": [
                        { "matcher": "bash", "hooks": [
                            { "type": "command", "command": "echo blocked" }
                        ] }
                    ]
                }
            }"#,
        )
        .unwrap();
        let out = load(&HookSpec::Path("./hooks.json".into()), tmp.path()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].event, "PreToolUse");
        assert_eq!(out[0].matcher.as_deref(), Some("bash"));
        assert!(out[0].command["command"] == "echo blocked");
    }

    #[test]
    fn load_inline_parses_json() {
        let json = serde_json::json!({
            "hooks": {
                "Stop": [
                    { "matcher": null, "hooks": [
                        { "type": "command", "command": "cleanup" }
                    ] }
                ]
            }
        });
        let out = load(&HookSpec::Inline(json), Path::new("/")).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].event, "Stop");
        assert_eq!(out[0].matcher, None);
    }

    /// v1.x 回归:hooks.json 的 `env: ["NAME"]` 白名单必须透传到
    /// `LoadedHook.command` —— 此前 `HookCommandEntry` 没有 env 字段,
    /// expand 重建 JSON 时静默丢弃,loader 读到的永远是空白名单。
    #[test]
    fn load_preserves_env_allowlist() {
        let json = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    { "matcher": "bash", "hooks": [
                        { "type": "command", "command": "blocked",
                          "timeout": 5, "env": ["INTERNAL_API_KEY", "HOME"] }
                    ] }
                ]
            }
        });
        let out = load(&HookSpec::Inline(json), Path::new("/")).unwrap();
        assert_eq!(out.len(), 1);
        let env = out[0]
            .command
            .get("env")
            .and_then(|v| v.as_array())
            .expect("env allowlist must survive expand");
        let names: Vec<&str> = env.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(names, vec!["INTERNAL_API_KEY", "HOME"]);
        // 其余字段不受影响。
        assert!(out[0].command["command"] == "blocked");
        assert!(out[0].command["timeout"] == 5);
    }

    /// 回归:`wasm` 字段必须经 `expand` 存活并展开 `${PLUGIN_ROOT}` ——
    /// loader 按 `command.get("wasm")` 判定 wasm hook;此前该字段被丢弃,
    /// wasm hook 永远落进 shell 路径再因缺 `command` 被跳过。
    #[test]
    fn load_preserves_wasm_path_and_expands_root() {
        let json = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    { "matcher": "bash", "hooks": [
                        { "type": "wasm", "wasm": "${PLUGIN_ROOT}/bin/hook.wasm", "timeout": 5 }
                    ] }
                ]
            }
        });
        let out = load(&HookSpec::Inline(json), Path::new("/plugins/demo")).unwrap();
        assert_eq!(out.len(), 1);
        let wasm = out[0]
            .command
            .get("wasm")
            .and_then(|v| v.as_str())
            .expect("wasm path must survive expand");
        assert_eq!(wasm, "/plugins/demo/bin/hook.wasm");
        assert!(out[0].command["command"].is_null(), "wasm hook 无 command");
        assert_eq!(out[0].matcher.as_deref(), Some("bash"));
    }

    #[test]
    fn load_many_combines_sources() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("a.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"x","hooks":[{"type":"command","command":"a"}]}]}}"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("b.json"),
            r#"{"hooks":{"PostToolUse":[{"matcher":null,"hooks":[{"type":"command","command":"b"}]}]}}"#,
        )
        .unwrap();
        let spec = HookSpec::Many(vec![
            HookSpec::Path("./a.json".into()),
            HookSpec::Path("./b.json".into()),
        ]);
        let out = load(&spec, tmp.path()).unwrap();
        assert_eq!(out.len(), 2);
    }
}
