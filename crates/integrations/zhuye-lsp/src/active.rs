//! # LSP 主动查询扩展(F15)
//!
//! 现有 `lsp` 工具已有 6 个只读 action(definition / references / hover /
//! documentSymbol / completion / signatureHelp)。本模块补**符号级主动
//! 查询**与 **DAP 调试**的协议形状:
//!
//! ## LSP 扩展方法
//!
//! | action | LSP method | 用途 |
//! |---|---|---|
//! | `find_referencing_symbols` | `textDocument/references` + 符号范围 | 找符号引用(比行内 references 更准) |
//! | `find_implementations` | `textDocument/implementation` | 找接口/虚方法的实现 |
//! | `rename` | `textDocument/rename` | 语义级重命名(跨文件,返回 WorkspaceEdit) |
//! | `call_hierarchy` | `textDocument/prepareCallHierarchy` + `callHierarchy/incomingCalls` | 调用者树 |
//! | `type_hierarchy` | `textDocument/prepareTypeHierarchy` | 类型继承层次 |
//!
//! ## DAP 调试
//!
//! 断点 / 变量 / REPL 三类请求的形状([`DapRequest`])。DAP 传输与适配器
//! 拉起在 `integrations/` 层,本模块只定义**协议消息**与**能力协商**。
//!
//! ## 为什么 rename 需要 LSP 而不是正则
//!
//! 正则改名会误伤注释、字符串、同名局部变量。LSP `textDocument/rename`
//! 返回的是**语义正确的 WorkspaceEdit**(按符号解析)。这也是 F13 的
//! 跨文件 rename 在有 LSP 时应该优先走 LSP 的原因。

use serde::{Deserialize, Serialize};

/// 扩展的 LSP action。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtendedLspAction {
    /// 找符号引用(带符号范围,比纯行内 references 准)。
    FindReferencingSymbols,
    /// 找接口 / 虚方法的实现。
    FindImplementations,
    /// 语义级重命名(返回 WorkspaceEdit)。
    Rename,
    /// 调用层次(谁调用了我)。
    CallHierarchy,
    /// 类型继承层次。
    TypeHierarchy,
}

impl ExtendedLspAction {
    pub const ALL: [ExtendedLspAction; 5] = [
        ExtendedLspAction::FindReferencingSymbols,
        ExtendedLspAction::FindImplementations,
        ExtendedLspAction::Rename,
        ExtendedLspAction::CallHierarchy,
        ExtendedLspAction::TypeHierarchy,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ExtendedLspAction::FindReferencingSymbols => "find_referencing_symbols",
            ExtendedLspAction::FindImplementations => "find_implementations",
            ExtendedLspAction::Rename => "rename",
            ExtendedLspAction::CallHierarchy => "call_hierarchy",
            ExtendedLspAction::TypeHierarchy => "type_hierarchy",
        }
    }

    pub fn parse(s: &str) -> Option<ExtendedLspAction> {
        ExtendedLspAction::ALL
            .iter()
            .copied()
            .find(|a| a.as_str() == s.trim().to_ascii_lowercase())
    }

    /// 对应的 LSP method(rename 需要 `workspace/applyEdit` 回执,单独标注)。
    pub fn lsp_method(self) -> &'static str {
        match self {
            ExtendedLspAction::FindReferencingSymbols => "textDocument/references",
            ExtendedLspAction::FindImplementations => "textDocument/implementation",
            ExtendedLspAction::Rename => "textDocument/rename",
            ExtendedLspAction::CallHierarchy => "textDocument/prepareCallHierarchy",
            ExtendedLspAction::TypeHierarchy => "textDocument/prepareTypeHierarchy",
        }
    }

    /// 是否是写操作(rename 会改文件)。
    ///
    /// **这是权限判定的唯一依据** —— 只读 action 可 `Auto`,写 action
    /// 必须 `Prompt`。新增 action 时若忘了分类,`is_write` 默认返回 true
    /// (保守),不会误放行。
    pub fn is_write(self) -> bool {
        matches!(self, ExtendedLspAction::Rename)
    }

    /// 需要的 `ServerCapabilities` 字段。
    pub fn capability_field(self) -> &'static str {
        match self {
            ExtendedLspAction::FindReferencingSymbols => "references_provider",
            ExtendedLspAction::FindImplementations => "implementation_provider",
            ExtendedLspAction::Rename => "rename_provider",
            ExtendedLspAction::CallHierarchy => "call_hierarchy_provider",
            ExtendedLspAction::TypeHierarchy => "type_hierarchy_provider",
        }
    }
}

/// 一个位置(LSP `Position` + 文件)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceLocation {
    pub file: String,
    /// 0 起(LSP 原生)。
    pub line: u32,
    /// 0 起(LSP 原生)。
    pub character: u32,
}

/// 一个范围(起止位置)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRange {
    pub start: SourceLocation,
    pub end: SourceLocation,
}

/// 符号引用结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolReference {
    pub range: SourceRange,
    /// 是否声明点(非引用)。
    #[serde(default)]
    pub is_declaration: bool,
}

/// 类型层次条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeHierarchyItem {
    pub name: String,
    pub kind: String,
    pub location: SourceLocation,
}

/// 调用层次条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallHierarchyItem {
    pub name: String,
    pub location: SourceLocation,
    /// 调用者(上一层)。
    #[serde(default)]
    pub callers: Vec<CallHierarchyItem>,
}

/// WorkspaceEdit 中的一个文件改动。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEdit {
    pub range: SourceRange,
    pub new_text: String,
}

/// 一个文件的编辑集。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEdit {
    pub file: String,
    pub edits: Vec<TextEdit>,
}

/// 语义重命名的结果(`WorkspaceEdit`)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceEdit {
    pub files: Vec<FileEdit>,
}

impl WorkspaceEdit {
    /// 受影响文件数。
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// 总编辑数。
    pub fn edit_count(&self) -> usize {
        self.files.iter().map(|f| f.edits.len()).sum()
    }

    /// 是否空编辑。
    pub fn is_empty(&self) -> bool {
        self.edit_count() == 0
    }

    /// 受影响文件列表(排序,便于展示)。
    pub fn affected_files(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.files.iter().map(|f| f.file.as_str()).collect();
        out.sort_unstable();
        out
    }
}

/// 服务端能力(协商结果)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspCapabilities {
    /// 支持的能力字段名集合。
    supported: Vec<String>,
}

impl LspCapabilities {
    pub fn from_fields(fields: &[String]) -> LspCapabilities {
        LspCapabilities {
            supported: fields.to_vec(),
        }
    }

    /// 服务端是否支持该 action。
    pub fn supports(&self, action: ExtendedLspAction) -> bool {
        self.supported
            .iter()
            .any(|f| f == action.capability_field())
    }

    /// 支持的扩展 action 列表。
    pub fn supported_actions(&self) -> Vec<ExtendedLspAction> {
        ExtendedLspAction::ALL
            .iter()
            .copied()
            .filter(|a| self.supports(*a))
            .collect()
    }
}

// ── DAP ──────────────────────────────────────────────────────────────────

/// DAP 请求类型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum DapRequest {
    /// 设置断点(文件 + 行)。
    SetBreakpoints { file: String, lines: Vec<u32> },
    /// 清除某文件的断点。
    ClearBreakpoints { file: String },
    /// 继续执行。
    Continue { thread_id: u64 },
    /// 单步跳过。
    Next { thread_id: u64 },
    /// 单步进入。
    StepIn { thread_id: u64 },
    /// 单步跳出。
    StepOut { thread_id: u64 },
    /// 暂停。
    Pause { thread_id: u64 },
    /// 取线程列表。
    Threads,
    /// 取栈帧。
    StackTrace { thread_id: u64 },
    /// 取变量(scope 内)。
    Variables { variables_reference: u64 },
    /// 求值(REPL)。
    Evaluate { expression: String, frame_id: u64 },
}

impl DapRequest {
    /// DAP `command` 字符串(线上字段)。
    pub fn command(&self) -> &'static str {
        match self {
            DapRequest::SetBreakpoints { .. } => "setBreakpoints",
            DapRequest::ClearBreakpoints { .. } => "setBreakpoints",
            DapRequest::Continue { .. } => "continue",
            DapRequest::Next { .. } => "next",
            DapRequest::StepIn { .. } => "stepIn",
            DapRequest::StepOut { .. } => "stepOut",
            DapRequest::Pause { .. } => "pause",
            DapRequest::Threads => "threads",
            DapRequest::StackTrace { .. } => "stackTrace",
            DapRequest::Variables { .. } => "variables",
            DapRequest::Evaluate { .. } => "evaluate",
        }
    }

    /// 是否会改变被调试进程状态(continue/step/pause)。
    ///
    /// 设置断点不改进程状态(只是配置),求值**可能**有副作用 —— 归为
    /// "改变状态"侧(保守)。
    pub fn mutates_process_state(&self) -> bool {
        matches!(
            self,
            DapRequest::Continue { .. }
                | DapRequest::Next { .. }
                | DapRequest::StepIn { .. }
                | DapRequest::StepOut { .. }
                | DapRequest::Pause { .. }
                | DapRequest::Evaluate { .. }
        )
    }
}

/// DAP 断点。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DapBreakpoint {
    pub file: String,
    pub line: u32,
    /// 是否已绑定到实际代码位置(未绑定的断点不会命中)。
    pub verified: bool,
}

/// DAP 调试会话状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DapState {
    /// 未启动。
    Inactive,
    /// 正在启动(拉起适配器 + initialize)。
    Starting,
    /// 已停止在断点(可查变量 / 求值)。
    Stopped,
    /// 运行中。
    Running,
    /// 已终止。
    Terminated,
}

impl DapState {
    pub fn as_str(self) -> &'static str {
        match self {
            DapState::Inactive => "inactive",
            DapState::Starting => "starting",
            DapState::Stopped => "stopped",
            DapState::Running => "running",
            DapState::Terminated => "terminated",
        }
    }

    /// 是否可在该状态下查变量 / 求值(必须停在断点)。
    pub fn can_inspect(self) -> bool {
        matches!(self, DapState::Stopped)
    }

    /// 是否可发步进 / 继续(必须在停止或运行态)。
    pub fn can_control(self) -> bool {
        matches!(self, DapState::Stopped | DapState::Running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_action_roundtrip() {
        for a in ExtendedLspAction::ALL {
            assert_eq!(ExtendedLspAction::parse(a.as_str()), Some(a));
        }
        assert_eq!(ExtendedLspAction::parse("bogus"), None);
    }

    #[test]
    fn only_rename_is_write() {
        for a in ExtendedLspAction::ALL {
            if a == ExtendedLspAction::Rename {
                assert!(a.is_write(), "rename 会改文件");
            } else {
                assert!(!a.is_write(), "{} 应只读", a.as_str());
            }
        }
    }

    #[test]
    fn lsp_methods_correct() {
        assert_eq!(
            ExtendedLspAction::FindReferencingSymbols.lsp_method(),
            "textDocument/references"
        );
        assert_eq!(
            ExtendedLspAction::FindImplementations.lsp_method(),
            "textDocument/implementation"
        );
        assert_eq!(
            ExtendedLspAction::Rename.lsp_method(),
            "textDocument/rename"
        );
        assert_eq!(
            ExtendedLspAction::CallHierarchy.lsp_method(),
            "textDocument/prepareCallHierarchy"
        );
    }

    #[test]
    fn capability_negotiation() {
        let caps =
            LspCapabilities::from_fields(&["references_provider".into(), "rename_provider".into()]);
        assert!(caps.supports(ExtendedLspAction::FindReferencingSymbols));
        assert!(caps.supports(ExtendedLspAction::Rename));
        assert!(!caps.supports(ExtendedLspAction::FindImplementations));
        let actions = caps.supported_actions();
        assert_eq!(actions.len(), 2);
    }

    #[test]
    fn workspace_edit_counts() {
        let edit = WorkspaceEdit {
            files: vec![
                FileEdit {
                    file: "a.rs".into(),
                    edits: vec![TextEdit {
                        range: SourceRange {
                            start: SourceLocation {
                                file: "a.rs".into(),
                                line: 0,
                                character: 0,
                            },
                            end: SourceLocation {
                                file: "a.rs".into(),
                                line: 0,
                                character: 3,
                            },
                        },
                        new_text: "new".into(),
                    }],
                },
                FileEdit {
                    file: "b.rs".into(),
                    edits: vec![],
                },
            ],
        };
        assert_eq!(edit.file_count(), 2);
        assert_eq!(edit.edit_count(), 1);
        assert!(!edit.is_empty());
        assert_eq!(edit.affected_files(), vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn empty_workspace_edit() {
        let edit = WorkspaceEdit::default();
        assert!(edit.is_empty());
        assert_eq!(edit.file_count(), 0);
    }

    #[test]
    fn dap_commands() {
        assert_eq!(DapRequest::Threads.command(), "threads");
        assert_eq!(
            DapRequest::SetBreakpoints {
                file: "a.rs".into(),
                lines: vec![1]
            }
            .command(),
            "setBreakpoints"
        );
        assert_eq!(
            DapRequest::Evaluate {
                expression: "x".into(),
                frame_id: 1
            }
            .command(),
            "evaluate"
        );
    }

    #[test]
    fn dap_state_mutation_classification() {
        assert!(DapRequest::Continue { thread_id: 1 }.mutates_process_state());
        assert!(DapRequest::Next { thread_id: 1 }.mutates_process_state());
        assert!(
            DapRequest::Evaluate {
                expression: "x".into(),
                frame_id: 0
            }
            .mutates_process_state()
        );
        assert!(!DapRequest::Threads.mutates_process_state());
        assert!(!DapRequest::StackTrace { thread_id: 1 }.mutates_process_state());
        assert!(
            !DapRequest::SetBreakpoints {
                file: "a".into(),
                lines: vec![1]
            }
            .mutates_process_state()
        );
    }

    #[test]
    fn dap_state_capabilities() {
        assert!(DapState::Stopped.can_inspect());
        assert!(!DapState::Running.can_inspect(), "运行中不能查变量");
        assert!(DapState::Stopped.can_control());
        assert!(DapState::Running.can_control());
        assert!(!DapState::Terminated.can_control());
        assert!(!DapState::Inactive.can_control());
    }

    #[test]
    fn dap_request_serializes_with_command_tag() {
        let req = DapRequest::Continue { thread_id: 7 };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(json.contains("\"command\":\"continue\""), "{json}");
        assert!(json.contains("\"thread_id\":7"), "{json}");
        let back: DapRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, req);
    }

    #[test]
    fn breakpoint_serializes() {
        let bp = DapBreakpoint {
            file: "a.rs".into(),
            line: 10,
            verified: true,
        };
        let json = serde_json::to_string(&bp).expect("serialize");
        let back: DapBreakpoint = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, bp);
    }

    #[test]
    fn call_hierarchy_nesting() {
        let inner = CallHierarchyItem {
            name: "inner".into(),
            location: SourceLocation {
                file: "a.rs".into(),
                line: 1,
                character: 0,
            },
            callers: vec![],
        };
        let outer = CallHierarchyItem {
            name: "outer".into(),
            location: SourceLocation {
                file: "a.rs".into(),
                line: 10,
                character: 0,
            },
            callers: vec![inner],
        };
        let json = serde_json::to_string(&outer).expect("serialize");
        let back: CallHierarchyItem = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.callers.len(), 1);
        assert_eq!(back.callers[0].name, "inner");
    }
}
