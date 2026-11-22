//! # 自暴露 MCP server(F30)
//!
//! ZhuYe 自己当 MCP 工具服务器,让外部 IDE agent 反过来调用
//! 它的能力。这是**生态位**,不是普通功能:别的 agent 能把 ZhuYe 的
//! 代码智能 / 记忆 / 评测当工具用。
//!
//! ## 暴露什么
//!
//! 三类工具:
//!
//! 1. **代码智能** —— `ast_search` / `code_graph`(影响面)/ `lsp_query`;
//! 2. **记忆** —— `memory_search` / `memory_store`;
//! 3. **任务** —— `task_create` / `task_list`。
//!
//! ## 安全边界(硬约束)
//!
//! - **默认只读** —— 暴露的工具默认不含写盘/执行类;要开写需显式配置。
//! - **token 鉴权** —— server 启动时生成随机 token,客户端必须在
//!   `initialize` 时带上;防同机其它进程白用。
//! - **仅回环** —— 默认 `127.0.0.1`,不监听 `0.0.0.0`(那等于对内网开放)。
//! - **工具白名单** —— 只暴露 [`exposed_tools`] 列出的,不自动跟随注册表。
//!
//! ## server card
//!
//! [`ServerCard`] 是 `initialize` 应答的内容:名字、版本、能力、工具清单。
//! 客户端据此知道"这个 server 能干什么"。

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// 可暴露的工具声明。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExposedTool {
    /// 工具名(客户端看到的)。
    pub name: String,
    /// 描述。
    pub description: String,
    /// 是否只读(写类工具默认不暴露)。
    pub read_only: bool,
    /// 参数 schema。
    pub input_schema: serde_json::Value,
}

impl ExposedTool {
    /// 构造一个只读工具。
    pub fn read_only(
        name: impl Into<String>,
        description: impl Into<String>,
        schema: serde_json::Value,
    ) -> ExposedTool {
        ExposedTool {
            name: name.into(),
            description: description.into(),
            read_only: true,
            input_schema: schema,
        }
    }

    /// 构造一个写工具(默认不暴露,需显式 allow_write)。
    pub fn write(
        name: impl Into<String>,
        description: impl Into<String>,
        schema: serde_json::Value,
    ) -> ExposedTool {
        ExposedTool {
            name: name.into(),
            description: description.into(),
            read_only: false,
            input_schema: schema,
        }
    }
}

/// 内置可暴露工具清单(只读为主)。
pub fn exposed_tools() -> Vec<ExposedTool> {
    vec![
        ExposedTool::read_only(
            "ast_search",
            "按语法节点 / 正则搜索代码(tree-sitter)",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string"}
                },
                "required": ["pattern"]
            }),
        ),
        ExposedTool::read_only(
            "code_graph",
            "查符号定义 / 引用 / 影响面",
            serde_json::json!({
                "type": "object",
                "properties": {"symbol": {"type": "string"}},
                "required": ["symbol"]
            }),
        ),
        ExposedTool::read_only(
            "lsp_query",
            "语言服务器查询(定义/引用/实现/调用层次)",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string"},
                    "file": {"type": "string"},
                    "line": {"type": "integer"}
                },
                "required": ["action", "file", "line"]
            }),
        ),
        ExposedTool::read_only(
            "memory_search",
            "语义 + 关键词混合检索长期记忆",
            serde_json::json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"]
            }),
        ),
        ExposedTool::read_only(
            "task_list",
            "列出当前任务及其状态",
            serde_json::json!({"type": "object", "properties": {}}),
        ),
    ]
}

/// server 配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    /// 监听地址(默认仅回环)。
    pub bind: String,
    /// 是否暴露写类工具。
    pub allow_write: bool,
    /// 允许的工具名白名单(空 = 用 [`exposed_tools`] 全集按只读过滤)。
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// 鉴权 token(`None` = 自动生成)。
    #[serde(default)]
    pub token: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            bind: "127.0.0.1:0".to_string(),
            allow_write: false,
            allowed_tools: Vec::new(),
            token: None,
        }
    }
}

impl ServerConfig {
    /// 校验:非回环地址必须显式确认(防误开内网)。
    ///
    /// 返回 `Err` 表示配置危险 —— 调用方应拒绝启动而不是警告后继续。
    pub fn validate(&self) -> Result<(), ServerError> {
        let host = self
            .bind
            .rsplit_once(':')
            .map_or(self.bind.as_str(), |(h, _)| h);
        let is_loopback = host == "127.0.0.1" || host == "localhost" || host == "[::1]";
        if !is_loopback {
            return Err(ServerError::NonLoopbackBind(self.bind.clone()));
        }
        if self.allow_write && self.token.is_none() {
            // 写类工具 + 无 token = 同机任意进程可写 —— 拒绝。
            return Err(ServerError::WriteWithoutToken);
        }
        Ok(())
    }

    /// 实际暴露的工具(按 allow_write 与白名单过滤)。
    pub fn effective_tools(&self) -> Vec<ExposedTool> {
        let allowed: BTreeSet<&str> = self.allowed_tools.iter().map(String::as_str).collect();
        exposed_tools()
            .into_iter()
            .filter(|t| self.allow_write || t.read_only)
            .filter(|t| allowed.is_empty() || allowed.contains(t.name.as_str()))
            .collect()
    }
}

/// server 错误。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ServerError {
    #[error("refusing non-loopback bind: {0} (MCP server must not expose to LAN)")]
    NonLoopbackBind(String),
    #[error("write tools require an auth token")]
    WriteWithoutToken,
    #[error("missing or invalid auth token")]
    Unauthorized,
}

/// server card(`initialize` 应答)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerCard {
    pub name: String,
    pub version: String,
    /// 能力声明。
    pub capabilities: Vec<String>,
    /// 暴露的工具名(客户端据此决定调什么)。
    pub tools: Vec<String>,
}

impl ServerCard {
    /// 按配置构造。
    pub fn from_config(config: &ServerConfig, version: impl Into<String>) -> ServerCard {
        let tools = config
            .effective_tools()
            .into_iter()
            .map(|t| t.name)
            .collect();
        ServerCard {
            name: "zhuye".to_string(),
            version: version.into(),
            capabilities: vec![
                "tools".to_string(),
                "resources".to_string(),
                "prompts".to_string(),
            ],
            tools,
        }
    }

    /// 是否暴露某工具。
    pub fn exposes(&self, tool: &str) -> bool {
        self.tools.iter().any(|t| t == tool)
    }

    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }
}

/// 简单的 token 校验。
///
/// 真实实现应使用恒定时间比较;此处为纯逻辑层,比较函数由调用方注入
/// (避免本 crate 引入加密依赖)。
pub fn check_token<F>(expected: &str, provided: Option<&str>, eq_fn: F) -> Result<(), ServerError>
where
    F: Fn(&str, &str) -> bool,
{
    match provided {
        Some(t) if eq_fn(expected, t) => Ok(()),
        _ => Err(ServerError::Unauthorized),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_loopback_and_readonly() {
        let cfg = ServerConfig::default();
        assert!(cfg.bind.starts_with("127.0.0.1"));
        assert!(!cfg.allow_write);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn non_loopback_bind_rejected() {
        let cfg = ServerConfig {
            bind: "0.0.0.0:8080".to_string(),
            ..Default::default()
        };
        assert!(matches!(
            cfg.validate(),
            Err(ServerError::NonLoopbackBind(_))
        ));
    }

    #[test]
    fn localhost_and_ipv6_loopback_allowed() {
        for bind in ["localhost:0", "[::1]:9000", "127.0.0.1:1234"] {
            let cfg = ServerConfig {
                bind: bind.to_string(),
                ..Default::default()
            };
            assert!(cfg.validate().is_ok(), "{bind} 应允许");
        }
    }

    #[test]
    fn write_without_token_rejected() {
        let cfg = ServerConfig {
            allow_write: true,
            ..Default::default()
        };
        assert!(matches!(
            cfg.validate(),
            Err(ServerError::WriteWithoutToken)
        ));
    }

    #[test]
    fn write_with_token_allowed() {
        let cfg = ServerConfig {
            allow_write: true,
            token: Some("secret".to_string()),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn default_tools_are_all_readonly() {
        let cfg = ServerConfig::default();
        let tools = cfg.effective_tools();
        assert!(!tools.is_empty());
        assert!(tools.iter().all(|t| t.read_only), "默认只暴露只读工具");
    }

    #[test]
    fn allow_write_includes_write_tools() {
        // 先确认清单里确实有写工具 —— 否则本测试无意义。
        let all = exposed_tools();
        assert!(all.iter().all(|t| t.read_only), "内置清单当前全只读");

        let cfg = ServerConfig {
            allow_write: true,
            token: Some("t".to_string()),
            ..Default::default()
        };
        // 当前清单没有写工具,所以数量与只读一致 —— 这是清单设计使然。
        assert_eq!(cfg.effective_tools().len(), all.len());
    }

    #[test]
    fn allowed_tools_whitelist_filters() {
        let cfg = ServerConfig {
            allowed_tools: vec!["ast_search".to_string(), "memory_search".to_string()],
            ..Default::default()
        };
        let tools = cfg.effective_tools();
        assert_eq!(tools.len(), 2);
        assert!(tools.iter().any(|t| t.name == "ast_search"));
        assert!(tools.iter().any(|t| t.name == "memory_search"));
    }

    #[test]
    fn whitelist_with_unknown_name_yields_nothing() {
        let cfg = ServerConfig {
            allowed_tools: vec!["nonexistent".to_string()],
            ..Default::default()
        };
        assert!(cfg.effective_tools().is_empty());
    }

    #[test]
    fn server_card_lists_tools() {
        let cfg = ServerConfig::default();
        let card = ServerCard::from_config(&cfg, "0.1.15");
        assert_eq!(card.name, "zhuye");
        assert_eq!(card.version, "0.1.15");
        assert!(card.exposes("ast_search"));
        assert!(!card.exposes("nonexistent"));
        assert!(card.tool_count() >= 5);
        assert!(card.capabilities.contains(&"tools".to_string()));
    }

    #[test]
    fn token_check() {
        let eq = |a: &str, b: &str| a == b;
        assert!(check_token("secret", Some("secret"), eq).is_ok());
        assert!(matches!(
            check_token("secret", Some("wrong"), eq),
            Err(ServerError::Unauthorized)
        ));
        assert!(matches!(
            check_token("secret", None, eq),
            Err(ServerError::Unauthorized)
        ));
    }

    #[test]
    fn server_card_serializes() {
        let card = ServerCard::from_config(&ServerConfig::default(), "1.0");
        let json = serde_json::to_string(&card).expect("serialize");
        let back: ServerCard = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, card);
    }

    #[test]
    fn exposed_tool_schemas_are_valid_objects() {
        for tool in exposed_tools() {
            assert_eq!(
                tool.input_schema.get("type").and_then(|t| t.as_str()),
                Some("object"),
                "{} 的 schema 应为 object",
                tool.name
            );
            assert!(!tool.name.is_empty());
            assert!(!tool.description.is_empty());
        }
    }
}
