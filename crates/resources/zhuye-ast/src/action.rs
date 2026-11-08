//! `AstAction` — `ast` tool 的 action 枚举。
//!
//! 镜像 `zhuye_lsp::methods::LspAction` 的设计:每个 variant 对应一个
//! 结构化操作。`is_read_only()` / `is_concurrency_safe()` 提供按 action
//! 的只读与并发判定;工具侧的权限路由实际由 `AstTool::action_permission`
//! 完成(`replace` / `rename_symbol` → `Prompt`),本模块的
//! `required_permission()` 保留 `Auto` 兜底(工具级缺省),供直接调用方
//! 与测试 sentinel 使用。

use serde::Deserialize;
use zhuye_protocol::PermissionMode;

/// LLM 在 `ast` tool 调用里填的 `action` 字段。
///
/// v1 共 4 个 variant,均已实装(对应 `AstTool` 的
/// `exec_list_languages` / `exec_search` / `exec_replace` /
/// `exec_rename_symbol`)。`serde(rename_all = "snake_case")` 把 variant
/// 序列化成 `"list_languages"` / `"search"` 等 snake_case,LLM 在 tool
/// call JSON 里直接填这些字符串。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AstAction {
    /// 列出当前 build 支持的所有 grammar(只读)。
    ListLanguages,
    /// 按 `pattern` 在 `path` 下找结构化匹配(只读)。
    Search,
    /// 用 `replacement` 改写每个匹配节点(写,经 approval gate)。
    Replace,
    /// 单文件内 symbol 重命名(写,经 approval gate)。
    RenameSymbol,
}

impl AstAction {
    /// 当前 action 是否只读(不修改文件、不发副作用)。
    ///
    /// `ListLanguages` / `Search` 是只读;`Replace` / `RenameSymbol` 会
    /// 写盘,返回 `false`。
    pub fn is_read_only(self) -> bool {
        match self {
            AstAction::ListLanguages | AstAction::Search => true,
            AstAction::Replace | AstAction::RenameSymbol => false,
        }
    }

    /// 权限兜底表:恒 `Auto`。
    ///
    /// mutation 的 `Prompt` 路由在 `AstTool::action_permission`(`replace`
    /// / `rename_symbol` → `Prompt`,走 ApprovalGate);本方法只提供工具级
    /// `Auto` 缺省,并在测试里作为 sentinel —— 若它不再是全 `Auto`,
    /// 需同步检查 `AstTool::action_permission` 的缺省分支。
    pub fn required_permission(self) -> PermissionMode {
        match self {
            AstAction::ListLanguages
            | AstAction::Search
            | AstAction::Replace
            | AstAction::RenameSymbol => PermissionMode::Auto,
        }
    }

    /// Per-action 并发安全标记。
    ///
    /// `ListLanguages` / `Search` 只读且文件无关(`true`);
    /// `Replace` / `RenameSymbol` 按文件写盘(`false`)。工具级
    /// `AstTool::is_concurrency_safe` 恒为 `false`(保守串行)。
    pub fn is_concurrency_safe(self) -> bool {
        match self {
            AstAction::ListLanguages | AstAction::Search => true,
            AstAction::Replace | AstAction::RenameSymbol => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_serde_roundtrip_all_variants() {
        for (raw, want) in [
            (r#""list_languages""#, AstAction::ListLanguages),
            (r#""search""#, AstAction::Search),
            (r#""replace""#, AstAction::Replace),
            (r#""rename_symbol""#, AstAction::RenameSymbol),
        ] {
            let parsed: AstAction = serde_json::from_str(raw).unwrap();
            assert_eq!(parsed, want);
        }
    }

    #[test]
    fn invalid_action_errors() {
        let r: Result<AstAction, _> = serde_json::from_str(r#""unknown_action""#);
        assert!(r.is_err());
    }

    #[test]
    fn required_permission_defaults_to_auto_and_read_only_consistent() {
        // 权限兜底恒为 Auto:mutation 的 Prompt 路由由 AstTool::action_permission
        // 负责(见 tool/mod.rs)。这个测试是 sentinel —— 一旦失败,说明
        // 有人改了 required_permission,需要同步检查 AstTool::action_permission
        // 的缺省分支。
        for action in [
            AstAction::ListLanguages,
            AstAction::Search,
            AstAction::Replace,
            AstAction::RenameSymbol,
        ] {
            assert_eq!(
                action.required_permission(),
                PermissionMode::Auto,
                "{action:?} should be Auto"
            );
        }
        // Replace / RenameSymbol 是写 action,必须返回 false;
        // ListLanguages / Search 是只读,必须返回 true。
        assert!(!AstAction::Replace.is_read_only());
        assert!(!AstAction::RenameSymbol.is_read_only());
        assert!(AstAction::ListLanguages.is_read_only());
        assert!(AstAction::Search.is_read_only());
    }
}
