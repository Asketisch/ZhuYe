//! `deferred_call` — tool-defer 门控插件的模型侧调用入口。
//!
//! 机制(借鉴 ZCode ToolSearch local 模式):开启 `tool-defer` 后,MCP
//! source 工具**不下发完整 schema**,只以目录公告(名字 + 描述摘要)进入
//! 模型上下文;模型经本工具按名调用 —— queue 在派发前把 `deferred_call`
//! 展开为真实调用,**审批 / hook / 权限 floor 全部按真实工具走**(展开
//! 发生在 gate 与 PreToolUse 之前,不是权限旁路)。
//!
//! 本工具的 `execute` 正常情况下不会被触达(queue 已展开);保留防御性
//! 实现以应对绕过 queue 的直接调用。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::registry::ToolRegistry;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

/// 工具名(wire 词表)。
pub const DEFERRED_CALL_TOOL_NAME: &str = "deferred_call";

/// 持有 registry 引用:防御性直调时查真实工具并转述(不走审批 ——
/// 该路径只应出现在测试;生产调用一律经 queue 展开)。
pub struct DeferredCallTool {
    registry: Arc<ToolRegistry>,
}

impl DeferredCallTool {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for DeferredCallTool {
    fn name(&self) -> &str {
        DEFERRED_CALL_TOOL_NAME
    }

    fn description(&self) -> &str {
        "调用一个按需加载(deferred)的工具。目录公告里列出的工具未随请求 \
         下发完整定义;用本工具按名字调用:name = 工具名,args = 参数对象。"
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "要调用的 deferred 工具名"},
                "args": {"type": "object", "description": "该工具的参数对象"}
            },
            "required": ["name"]
        })
    }

    fn required_permission(&self) -> zhuye_protocol::PermissionMode {
        // 权限 floor 不能低于被转发工具 —— 正常路径 queue 展开后按真实
        // 工具走;这里是直调兜底,取最保守的 Prompt。
        zhuye_protocol::PermissionMode::Prompt
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs {
                message: "缺少 name 字段".into(),
            })?
            .to_string();
        let inner_args = args.get("args").cloned().unwrap_or(Value::Null);
        let tool = self
            .registry
            .get(&name)
            .ok_or_else(|| ToolError::InvalidArgs {
                message: format!("工具 {name} 不存在(目录可能已变化;可用 tool_search 重查)"),
            })?;
        tracing::debug!(tool = %name, "deferred_call:防御性直调(未经 queue 展开)");
        tool.execute(ctx, inner_args).await
    }
}
