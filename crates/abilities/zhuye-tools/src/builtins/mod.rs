//! agent 自带的内置工具集。
//!
//! 按能力分为六组：
//!
//! | 子目录 | 能力域 |
//! |---|---|
//! | [`fs`] | 文件与目录读写 |
//! | [`web`] | 网络取内容 |
//! | [`human`] | 人机交互 |
//! | [`plan`] | Plan mode |
//! | [`exec`] | 命令执行与 worktree 切换 |
//! | [`meta`] | 引擎自省与调度基建 |
//!
//! ## 对外路径契约
//!
//! **crate 外一律走 `zhuye_tools::builtins::<Type>` 的平铺路径** ——
//! 本模块下方的 `pub use` 是唯一入口，不暴露分组路径。
//! 改分组只动目录结构，不动这批 re-export，调用方零感知。

pub mod exec;
pub mod fs;
pub mod human;
pub mod meta;
pub mod plan;
pub mod web;

pub use exec::bash::BashTool;
pub use exec::echo::EchoTool;
pub use exec::enter_worktree::EnterWorktreeTool;
pub use exec::exit_worktree::ExitWorktreeTool;
pub use fs::delete::DeleteTool;
pub use fs::edit::EditTool;
pub use fs::glob::GlobTool;
pub use fs::grep::GrepTool;
pub use fs::image_view::ImageViewTool;
pub use fs::notebook_edit::NotebookEditTool;
pub use fs::read::ReadTool;
pub use fs::write::WriteTool;
pub use human::ask_user::AskUserTool;
pub use human::ask_user_question::AskUserQuestionTool;
pub use human::brief::BriefTool;
pub use human::request_human_input::RequestHumanInputTool;
pub use meta::background_status::BackgroundStatusTool;
pub use meta::context_remaining::GetContextRemainingTool;
pub use meta::deferred_call::DeferredCallTool;
pub use meta::tool_search::ToolSearchTool;
pub use plan::enter_plan_mode::EnterPlanModeTool;
pub use plan::exit_plan_mode::ExitPlanModeTool;
pub use plan::plan_write::PlanWriteTool;
pub use web::web_fetch::WebFetchTool;
pub use web::web_search::WebSearchTool;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;

    #[test]
    fn builtin_descriptions_are_loaded_from_prompt_resources() {
        let tool_search = ToolSearchTool::new(std::sync::Arc::new(crate::ToolRegistry::new()));
        let web_fetch = WebFetchTool::default();
        let web_search = WebSearchTool::new();
        let ask_uq = AskUserQuestionTool::default();
        let bash = BashTool::default();
        let descriptions = [
            AskUserTool.description(),
            ask_uq.description(),
            bash.description(),
            BriefTool.description(),
            GetContextRemainingTool.description(),
            DeleteTool.description(),
            EchoTool.description(),
            EditTool.description(),
            EnterPlanModeTool.description(),
            EnterWorktreeTool.description(),
            ExitPlanModeTool.description(),
            ExitWorktreeTool.description(),
            GlobTool.description(),
            GrepTool.description(),
            ImageViewTool.description(),
            NotebookEditTool.description(),
            PlanWriteTool.description(),
            ReadTool.description(),
            RequestHumanInputTool.description(),
            tool_search.description(),
            web_fetch.description(),
            web_search.description(),
            WriteTool.description(),
        ];
        assert_eq!(descriptions.len(), 23);
        assert!(
            descriptions
                .iter()
                .all(|description| !description.is_empty())
        );
    }
}
