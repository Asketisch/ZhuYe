//! 内置工具注册清单 —— 全框架**单一事实源**(WP8 收编)。
//!
//! 此前 `zhuye-exec/src/headless.rs`(exec / serve 路径)与
//! `zhuye/src/builder.rs::default_tool_registry`(lib facade / SDK 路径)
//! 各维护一份 builtin 注册清单,且已漂移:门面缺 AskUser / AskUserQuestion /
//! RequestHumanInput / ToolSearch / ImageView / Brief / GetContextRemaining /
//! Checkpoint / Rewind(共 9 个)—— 于是同一份 prompt 向模型宣告这些工具
//! 可用,SDK 路径却报 "tool not found"。
//!
//! 本函数收编为并集,三条路径(exec / serve / lib facade)都调它。
//! `ToolSearchTool` 需要 registry 自身句柄(它在注册表里查工具),故签名
//! 接收 `Arc<ToolRegistry>`。

use std::sync::Arc;

use crate::builtins;
use crate::registry::ToolRegistry;
use crate::tool::Tool;
use crate::{CheckpointTool, RewindTool};

/// 注册全部内置工具(exec / serve / lib facade 共用)。
///
/// `ask_uq_max_questions` 为 `[ask_user_question]` 段解析出的上限;
/// `None` 时用内置默认(4)。`ast_tool` 由调用方注入 —— `zhuye-ast` 反向
/// 依赖本 crate(AstTool 实现 `Tool`),不能在此直接构造,否则成环。
pub fn register_builtin_tools(
    tools: &Arc<ToolRegistry>,
    ask_uq_max_questions: Option<u32>,
    ast_tool: Arc<dyn Tool>,
) {
    tools.register(Arc::new(builtins::EchoTool));
    tools.register(Arc::new(builtins::BashTool::default()));
    // 核心文件工具。
    tools.register(Arc::new(builtins::ReadTool));
    tools.register(Arc::new(builtins::WriteTool));
    tools.register(Arc::new(builtins::EditTool));
    tools.register(Arc::new(builtins::DeleteTool));
    tools.register(Arc::new(builtins::GrepTool));
    tools.register(Arc::new(builtins::GlobTool));
    // Plan mode 控制面(EnterPlanMode / ExitPlanMode / PlanWrite 必须同
    // 注册,否则 LLM 看到 schema 却找不到实现 → "tool not found")。
    tools.register(Arc::new(builtins::EnterPlanModeTool));
    tools.register(Arc::new(builtins::ExitPlanModeTool));
    tools.register(Arc::new(builtins::PlanWriteTool));
    // worktree 隔离会话。
    tools.register(Arc::new(builtins::EnterWorktreeTool));
    tools.register(Arc::new(builtins::ExitWorktreeTool));
    // AST 结构化搜索 / 重写(tree-sitter;调用方注入,见函数文档)。
    tools.register(ast_tool);
    // Web 工具:WebFetch 走 Prompt 权限,WebSearch 默认 Auto。
    tools.register(Arc::new(builtins::WebFetchTool::new()));
    tools.register(Arc::new(builtins::WebSearchTool::new()));
    // 人工输入 / 提问族。
    tools.register(Arc::new(builtins::AskUserQuestionTool {
        max_questions: ask_uq_max_questions.unwrap_or(4) as u8,
    }));
    tools.register(Arc::new(builtins::AskUserTool));
    tools.register(Arc::new(builtins::RequestHumanInputTool));
    // ToolSearch 需要 registry 句柄(在注册表里查工具)。
    tools.register(Arc::new(builtins::ToolSearchTool::new(tools.clone())));
    tools.register(Arc::new(builtins::ImageViewTool));
    tools.register(Arc::new(builtins::NotebookEditTool));
    tools.register(Arc::new(builtins::BriefTool));
    tools.register(Arc::new(builtins::GetContextRemainingTool));
    // git checkpoint / rewind(v1.2 P0-3;底层已下沉 zhuye-git)。
    tools.register(Arc::new(CheckpointTool));
    tools.register(Arc::new(RewindTool));
}

/// 内置工具数量(清单变更时同步更新测试期望)。
pub const BUILTIN_TOOL_COUNT: usize = 26;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;

    /// 替身 AstTool(真 AstTool 在 zhuye-ast,会成环)。
    struct FakeAst;
    #[async_trait::async_trait]
    impl Tool for FakeAst {
        fn name(&self) -> &str {
            "ast"
        }
        fn description(&self) -> &str {
            "fake"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(
            &self,
            _ctx: ToolContext,
            _args: serde_json::Value,
        ) -> Result<crate::ToolOutput, crate::ToolError> {
            unreachable!()
        }
    }

    #[test]
    fn registers_the_full_builtin_set() {
        let tools = Arc::new(ToolRegistry::default());
        register_builtin_tools(&tools, None, Arc::new(FakeAst));
        let names = tools.list();
        assert_eq!(names.len(), BUILTIN_TOOL_COUNT, "got {names:?}");
        for expected in [
            "echo",
            "bash",
            "read",
            "write",
            "edit",
            "delete_file",
            "grep",
            "glob",
            "EnterPlanMode",
            "ExitPlanMode",
            "PlanWrite",
            "EnterWorktree",
            "ExitWorktree",
            "ast",
            "web_fetch",
            "web_search",
            "ask_user_question",
            "ask_user",
            "request_human_input",
            "tool_search",
            "image_view",
            "notebook_edit",
            "brief",
            "get_context_remaining",
            "checkpoint",
            "rewind",
        ] {
            assert!(names.iter().any(|n| n == expected), "缺内置工具 {expected}");
        }
    }
}
