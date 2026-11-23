//! MCP server 发起的用户交互请求(additive EventMsg 载荷)。
//!
//! 对应 MCP 协议的 `elicitation/create`,有两种服务端形态:
//! - **form**:服务端给一份扁平对象 schema(属性为原始类型),宿主按
//!   [`McpElicitationKind::Form`] 的 `fields` 逐项收集,回执 `content`
//!   必须是 `{字段名: 值}` 对象 —— 字段名由服务端 schema 决定,引擎不做改名;
//! - **url**:服务端给一个外部链接([`McpElicitationKind::Link`]),宿主把用户
//!   引导过去完成操作,回执 `accept` 时 `content` 可省略。
//!
//! 与 `ApprovalRequest` / `AskUserQuestion` 一样,这是**要人答**的阻塞请求:
//! 宿主不回执则引擎按 TTL 以 `cancel` 了结(fail-closed,请求不会永久挂起)。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 表单字段的输入类型。取自服务端 JSON schema 的属性定义。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpElicitationFieldKind {
    /// 单行文本。`secret = true` 时客户端必须掩码显示且不得回显到日志。
    Text {
        /// 是否为敏感字段(按字段名启发式判定:含 password / secret / token)。
        #[serde(default)]
        secret: bool,
    },
    /// 数值输入。`integer = true` 对应 schema `type: "integer"`,否则 `"number"`。
    Number {
        /// `true` = 整数输入(schema `type: "integer"`)。
        #[serde(default)]
        integer: bool,
    },
    /// 布尔开关。
    Boolean,
    /// 从服务端候选项中选择。`multi = true` 表示允许多选(schema 的 `anyOf` 形态),
    /// 此时回执值应是候选项数组;`false` 是单选(schema 的 `enum` 形态)。
    Enum {
        /// 候选值(保持服务端声明顺序)。
        options: Vec<String>,
        /// `true` = 多选(schema 的 `anyOf` const 形态),回执值应是候选项数组。
        #[serde(default)]
        multi: bool,
    },
}

/// 表单字段定义。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpElicitationField {
    /// 回执 `content` 里使用的键名(服务端 schema 属性名,不得改写)。
    pub name: String,
    /// 人类可读标题(schema `title`)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 字段说明(schema `description`)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 是否为服务端要求的必填项。
    #[serde(default)]
    pub required: bool,
    /// 输入类型。
    pub kind: McpElicitationFieldKind,
}

/// 服务器请求的交互形式。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpElicitationKind {
    /// 表单输入:按 `fields` 顺序收集,回执 `content` = `{name: value}`。
    Form {
        /// 字段列表,保持服务端 schema 的声明顺序。
        fields: Vec<McpElicitationField>,
    },
    /// URL 引导:宿主把用户导向 `url` 完成操作,`accept` 时无需携带内容。
    Link {
        /// 服务端给出的授权 / 补全链接,宿主负责把用户引导过去。
        url: String,
    },
}

/// MCP server 向宿主用户请求输入。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpElicitationEvent {
    /// 请求 id,客户端用 `Op::McpElicitationResponse` 原样回传。
    pub request_id: String,
    /// 发起请求的 MCP server 名(config `[mcp_servers.<name>]` 的键;
    /// 插件挂载的 server 为 `plugin:<id>:<名>`)。
    pub server: String,
    /// 向用户展示的提示。
    pub message: String,
    /// 交互形式。
    pub kind: McpElicitationKind,
}

/// 用户对 MCP server 交互请求的回执载荷。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpElicitationResponse {
    /// 与 `McpElicitationEvent.request_id` 配对。
    pub id: String,
    /// `"accept"` / `"decline"` / `"cancel"` 之一(直接透传给服务端)。
    pub action: String,
    /// 用户输入;`decline` / `cancel` 时为 `None`。
    pub content: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventMsg;
    use serde_json::json;

    #[test]
    fn form_roundtrips_with_wire_shape() {
        let ev = EventMsg::McpElicitation(McpElicitationEvent {
            request_id: "r-1".into(),
            server: "fsx".into(),
            message: "请补充".into(),
            kind: McpElicitationKind::Form {
                fields: vec![
                    McpElicitationField {
                        name: "repo".into(),
                        title: Some("仓库".into()),
                        description: None,
                        required: true,
                        kind: McpElicitationFieldKind::Text { secret: false },
                    },
                    McpElicitationField {
                        name: "mode".into(),
                        title: None,
                        description: None,
                        required: false,
                        kind: McpElicitationFieldKind::Enum {
                            options: vec!["fast".into(), "slow".into()],
                            multi: false,
                        },
                    },
                ],
            },
        });
        let value = serde_json::to_value(&ev).unwrap();
        assert_eq!(value["type"], "mcp_elicitation");
        assert_eq!(value["kind"]["type"], "form");
        assert_eq!(value["kind"]["fields"][1]["kind"]["options"][0], "fast");
        // EventMsg 本身不实现 PartialEq(各变体载荷类型各异),按线格式比对。
        let back: EventMsg = serde_json::from_value(value).unwrap();
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            serde_json::to_value(&ev).unwrap()
        );
    }

    #[test]
    fn response_content_is_optional() {
        let op = crate::Op::McpElicitationResponse(McpElicitationResponse {
            id: "r-1".into(),
            action: "accept".into(),
            content: Some(json!({"repo": "zhuYe"})),
        });
        let value = serde_json::to_value(&op).unwrap();
        assert_eq!(value["type"], "mcp_elicitation_response");
        assert_eq!(value["content"]["repo"], "zhuYe");
        assert_eq!(op.discriminant(), "mcp_elicitation_response");
    }
}
