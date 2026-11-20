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

/// 门控型 bundled 插件工具的注册门(先例 goal-queue 的泛化)。
///
/// 门控型插件:能力本体留在引擎 crate,注册与否由插件启用状态决定
/// —— 用于「默认开启且必须保持原工具名 / 审批 / 行为」的能力(外部
/// 用法零影响)。装配层用 `zhuye_plugin::bundled_tool_gates(enabled,
/// disabled)` 构造后传入 [`register_builtin_tools_with_gates`];
/// 关闭 = 工具不进注册表(tools 面消失、tool_search 搜不到),区别于
/// `[active].disallowed_tools` 的仅 LLM 不可见(可见性过滤层)。
#[derive(Debug, Clone, Copy, Default)]
pub struct BundledToolGates {
    /// `web-search` 插件(web_search 工具)。默认 false(保守),
    /// 装配层按插件状态显式打开。
    pub web_search: bool,
    /// `notebook` 插件(notebook_edit 工具)。
    pub notebook_edit: bool,
}

impl BundledToolGates {
    /// 全关(保守默认)—— 装配层经链式 setter 显式打开需要的门。
    pub fn new() -> Self {
        Self::default()
    }

    /// 全开(不门控,全部注册)—— 兼容旧调用方 / 引擎内部测试。
    pub fn all_on() -> Self {
        Self {
            web_search: true,
            notebook_edit: true,
        }
    }

    /// 链式设置 web_search 门。
    pub fn web_search(mut self, on: bool) -> Self {
        self.web_search = on;
        self
    }

    /// 链式设置 notebook_edit 门。
    pub fn notebook_edit(mut self, on: bool) -> Self {
        self.notebook_edit = on;
        self
    }
}

/// 注册全部内置工具(exec / serve / lib facade 共用)。
///
/// 门控无关入口:等价于 `register_builtin_tools_with_gates(_, _, _,
/// BundledToolGates::all_on())`,供引擎内部测试与不需要插件门控的
/// 调用方使用;三条装配路径(exec headless / lib builder)应改调
/// `register_builtin_tools_with_gates` 以接入门控。
///
/// `ask_uq_max_questions` 为 `[ask_user_question]` 段解析出的上限;
/// `None` 时用内置默认(4)。`ast_tool` 由调用方注入 —— `zhuye-ast` 反向
/// 依赖本 crate(AstTool 实现 `Tool`),不能在此直接构造,否则成环。
pub fn register_builtin_tools(
    tools: &Arc<ToolRegistry>,
    ask_uq_max_questions: Option<u32>,
    ast_tool: Arc<dyn Tool>,
) {
    register_builtin_tools_with_gates(
        tools,
        ask_uq_max_questions,
        ast_tool,
        BundledToolGates::all_on(),
    );
}

/// 注册内置工具,带 bundled 插件门控(exec / serve / lib facade 共用,
/// 单一事实源)。注册顺序与门控无关项保持稳定;门控项(web_search /
/// notebook_edit)关闭时**跳过注册**(不进注册表)。
pub fn register_builtin_tools_with_gates(
    tools: &Arc<ToolRegistry>,
    ask_uq_max_questions: Option<u32>,
    ast_tool: Arc<dyn Tool>,
    gates: BundledToolGates,
) {
    tools.register(Arc::new(builtins::EchoTool));
    tools.register(Arc::new(builtins::BashTool::default()));
    // 后台任务状态查询:bash 的 run_in_background 回执让模型「可用
    // background_status 查询」,三条路径必须真的注册它,否则真二进制
    // 里模型按提示调用只会拿到 tool not found(此前仅引擎测试手工
    // 注册,遮住了这个缺口)。
    tools.register(Arc::new(builtins::BackgroundStatusTool));
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
    // web-search 门控插件(默认开;关闭 = 不注册,区别于可见性过滤)。
    if gates.web_search {
        tools.register(Arc::new(builtins::WebSearchTool::new()));
    }
    // 人工输入 / 提问族。
    tools.register(Arc::new(builtins::AskUserQuestionTool {
        max_questions: ask_uq_max_questions.unwrap_or(4) as u8,
    }));
    tools.register(Arc::new(builtins::AskUserTool));
    tools.register(Arc::new(builtins::RequestHumanInputTool));
    // ToolSearch 需要 registry 句柄(在注册表里查工具)。
    tools.register(Arc::new(builtins::ToolSearchTool::new(tools.clone())));
    tools.register(Arc::new(builtins::ImageViewTool));
    // notebook 门控插件(默认开;关闭 = 不注册)。
    if gates.notebook_edit {
        tools.register(Arc::new(builtins::NotebookEditTool));
    }
    tools.register(Arc::new(builtins::BriefTool));
    tools.register(Arc::new(builtins::GetContextRemainingTool));
    // git checkpoint / rewind(v1.2 P0-3;底层已下沉 zhuye-git)。
    tools.register(Arc::new(CheckpointTool));
    tools.register(Arc::new(RewindTool));
}

/// 内置工具数量(门控全开时的总数;清单变更时同步更新测试期望)。
pub const BUILTIN_TOOL_COUNT: usize = 27;

/// 无条件内置工具数(不含门控项)。
pub const UNGATED_BUILTIN_TOOL_COUNT: usize = BUILTIN_TOOL_COUNT - 2;

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
            "background_status",
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

    /// 门控全关:web_search / notebook_edit 不进注册表,其余 25 个不变
    /// (注册顺序与全开时一致 —— 仅少两个名字)。
    #[test]
    fn gates_off_skip_plugin_tools_only() {
        let tools = Arc::new(ToolRegistry::default());
        register_builtin_tools_with_gates(
            &tools,
            None,
            Arc::new(FakeAst),
            BundledToolGates::default(),
        );
        let names = tools.list();
        assert_eq!(names.len(), UNGATED_BUILTIN_TOOL_COUNT, "got {names:?}");
        assert!(
            !names.iter().any(|n| n == "web_search"),
            "门控关闭后不应注册 web_search"
        );
        assert!(
            !names.iter().any(|n| n == "notebook_edit"),
            "门控关闭后不应注册 notebook_edit"
        );
        // 关键引擎工具不受门控影响。
        for kept in ["echo", "bash", "read", "web_fetch", "checkpoint"] {
            assert!(names.iter().any(|n| n == kept), "门控不应影响 {kept}");
        }
    }
}
