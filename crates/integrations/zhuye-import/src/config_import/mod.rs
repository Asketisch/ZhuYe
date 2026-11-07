//! v1.10 功能1:**一键导入其他主流本地 agent 配置**。
//!
//! 从 Claude Code / Codex / Gemini CLI / opencode / crush 的本地配置中
//! 提取 **provider(base_url / api_key / model)** 与 **MCP server** 定义,
//! 合并进烛夜 `~/.zhuye/config.toml`:
//!
//! - anthropic 兼容端点 → `[[anthropic.credentials]]`;
//! - OpenAI 兼容端点 → `[[openai.credentials]]`(空 api_key = 免 key
//!   端点,照常导入 —— 与 v1.10 功能5 语义对齐);
//! - MCP server → `[mcp_servers.<name>]`(同名已存在则跳过,不覆盖)。
//!
//! ```text
//! sources(各 agent 配置文件) ──parse──▶ ImportedAgentConfig(IR)
//!          ──merge_into_config──▶ ReflectConfig(内存)
//!          ──write──▶ ~/.zhuye/config.toml
//! ```
//!
//! 设计约束(与会话导入 `crate::adapter` 同构):
//! - **逐字段容错**:某来源缺字段 / 坏 JSON 记 warning 跳过,不整体失败;
//! - **幂等**:provider 落到稳定的 `imported:<agent>:<key>` label,
//!   重复导入命中同 label 跳过;MCP 同名跳过;
//! - **不越权**:不修改用户已有的 provider / MCP 条目 / active.provider,
//!   只做**增量**插入。

pub mod merge;
pub mod sources;

use serde::{Deserialize, Serialize};

/// 支持配置导入的本地 agent。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConfigSourceKind {
    ClaudeCode,
    Codex,
    Gemini,
    Opencode,
    Crush,
}

impl ConfigSourceKind {
    /// CLI / MCP 工具参数取值(kebab-case,与会话导入 `HarnessKind` 别名
    /// 口径一致)。
    pub fn as_str(self) -> &'static str {
        match self {
            ConfigSourceKind::ClaudeCode => "claude-code",
            ConfigSourceKind::Codex => "codex",
            ConfigSourceKind::Gemini => "gemini",
            ConfigSourceKind::Opencode => "opencode",
            ConfigSourceKind::Crush => "crush",
        }
    }

    /// 大小写不敏感解析(`-` / `_` 均可)。
    pub fn parse(s: &str) -> Option<Self> {
        let norm = s.trim().to_ascii_lowercase().replace('_', "-");
        Some(match norm.as_str() {
            "claude-code" | "claude" => ConfigSourceKind::ClaudeCode,
            "codex" => ConfigSourceKind::Codex,
            "gemini" => ConfigSourceKind::Gemini,
            "opencode" => ConfigSourceKind::Opencode,
            "crush" => ConfigSourceKind::Crush,
            _ => return None,
        })
    }

    /// 全部已知来源(`--from auto` 的扫描面)。
    pub fn all() -> &'static [ConfigSourceKind] {
        &[
            ConfigSourceKind::ClaudeCode,
            ConfigSourceKind::Codex,
            ConfigSourceKind::Gemini,
            ConfigSourceKind::Opencode,
            ConfigSourceKind::Crush,
        ]
    }
}

/// 单个来源解析出的 IR(provider + MCP server + 警告)。
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportedAgentConfig {
    pub kind: Option<ConfigSourceKind>,
    /// 提取出的 provider 定义(可多个)。
    pub providers: Vec<ImportedProvider>,
    /// 提取出的 MCP server 定义。
    pub mcp_servers: Vec<ImportedMcpServer>,
    /// 逐字段容错产生的警告(不可映射的内容如实记录)。
    pub warnings: Vec<String>,
}

impl ImportedAgentConfig {
    pub fn push_warning(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }
}

/// 一条 provider 定义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedProvider {
    /// 稳定 label(`imported:<agent>:<key>`),幂等去重用。
    pub label: String,
    /// API key;空串 = 免 key 端点(合法值,功能5)。
    pub api_key: String,
    pub base_url: Option<String>,
    pub model: Option<String>,
    /// `true` → 落 `[[anthropic.credentials]]`;否则 `[[openai.credentials]]`。
    pub anthropic_compatible: bool,
}

/// 一条 MCP server 定义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedMcpServer {
    pub name: String,
    pub transport: ImportedMcpTransport,
}

/// MCP transport IR(映射 `McpServerEntry`)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImportedMcpTransport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
    },
    Http {
        url: String,
        /// 远程 MCP 的鉴权头等(Claude Code `headers` / opencode `headers`)。
        #[serde(default)]
        headers: std::collections::HashMap<String, String>,
        /// `true` = SSE 传输(如 Gemini 的 `url` 字段、显式 `type: "sse"`);
        /// `false` = streamable HTTP。
        #[serde(default)]
        sse: bool,
    },
}

/// 一键导入:扫描 `home` 下全部已知来源(未安装的跳过),解析为 IR 列表。
///
/// 返回 (命中的来源列表, 全局警告)。单个来源解析失败只记 warning,
/// 不阻断其它来源(「一键」要的是尽力而为)。
pub fn scan_all(home: &std::path::Path) -> (Vec<ImportedAgentConfig>, Vec<String>) {
    let mut out = Vec::new();
    let mut global = Vec::new();
    for kind in ConfigSourceKind::all() {
        match sources::parse_source(*kind, home) {
            Ok(cfg) if !cfg.providers.is_empty() || !cfg.mcp_servers.is_empty() => {
                out.push(cfg);
            }
            Ok(cfg) => {
                // 有警告但没提取到内容 → 把警告上浮(如 gemini 的模型不可映射)。
                global.extend(cfg.warnings);
            }
            Err(e) => global.push(format!("{}: {e}", kind.as_str())),
        }
    }
    (out, global)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_roundtrip_and_aliases() {
        for k in ConfigSourceKind::all() {
            assert_eq!(ConfigSourceKind::parse(k.as_str()), Some(*k));
        }
        assert_eq!(
            ConfigSourceKind::parse("Claude_Code"),
            Some(ConfigSourceKind::ClaudeCode)
        );
        assert_eq!(
            ConfigSourceKind::parse("claude"),
            Some(ConfigSourceKind::ClaudeCode)
        );
        assert_eq!(ConfigSourceKind::parse("unknown"), None);
    }
}
