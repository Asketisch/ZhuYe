//! `deferred_call` — tool-defer 门控插件的模型侧调用入口。
//!
//! 机制(借鉴同类 agent 的 ToolSearch local 模式):开启 `tool-defer` 后,MCP
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

/// 占位入口:实际转发仅允许由队列在目标工具审批前展开。
pub struct DeferredCallTool;

impl DeferredCallTool {
    pub fn new(_registry: Arc<ToolRegistry>) -> Self {
        Self
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

    async fn execute(&self, _ctx: ToolContext, _args: Value) -> Result<ToolOutput, ToolError> {
        Err(ToolError::InvalidArgs {
            message: "仅支持通过执行队列调用已注册的 MCP 工具".into(),
        })
    }
}
