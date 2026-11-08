//! `zhuye-ast` — 基于 tree-sitter 的结构化代码搜索 / 重写工具 (v1.0.0-rc1)。
//!
//! 设计上镜像 `zhuye-lsp`:把 tree-sitter 的 5 个默认 grammar(rust /
//! typescript / python / go / javascript)+ feature-gated 的 15+ 扩展
//! grammar 统一暴露为单一 `ast` tool。`action` 字段决定走哪条路径,LLM
//! 不直接接触 grammar 选择 —— `LanguageGrid::for_ext(path)` 按文件后缀
//! 自动路由(对齐 LSP 的 `matching::pick_server_for`)。
//!
//! ## 实现范围(4 个 action 均已实装)
//!
//! - `action = list_languages` —— 列出当前 build 启用的所有 grammar,
//!   5 个默认 + feature 启用的扩展。
//! - `action = search` —— `kind:<node>` / `regex:<re>` 单文件 tree-sitter
//!   匹配;`text:<literal>` 走文件系统遍历(对齐 `grep` builtin)。
//! - `action = replace` —— `kind:<node>` 批量改写,经 approval gate 后写盘,
//!   返回 unified diff。
//! - `action = rename_symbol` —— 单文件 word-boundary 符号重命名(写盘)。
//!
//! ## 留待后续 (v1+)
//!
//! - Bare-pattern DSL (`fn $NAME(...)`) —— 留 v1+ 单独 PR
//! - 跨文件 rename —— 需 LSP `textDocument/references` 配合
//! - `replace` 对 `regex:` / `text:` pattern 的支持 —— 需先 search 拿
//!   byte range,留 v1+ 单独路径
//!
//! ## 关键约束
//!
//! - Tool 名称强制单数 `ast`(对齐 `lsp`),LLM 调 `ast({"action": "..."})`。
//! - grammar 选择按 Cargo feature 编译期决定;`cargo build -p zhuye-ast
//!   --features lang-all` 可启用全部 20 个 grammar。runtime 不下载 / 不
//!   spawn 子进程(parser 内存常驻)。
//! - 权限:只读 action(`list_languages` / `search`)走 `Auto`;
//!   `replace` / `rename_symbol` 由 [`AstTool::action_permission`] 切
//!   `Prompt`,mutation 必须经 approval gate。

#![allow(clippy::module_name_repetitions)] // 多模块暴露 *Config/*Error 等命名

pub mod action;
pub mod edit;
pub mod grid;
pub mod matcher;
pub mod tool;

pub use action::AstAction;
pub use edit::{apply_rename, apply_replace};
pub use grid::{AstError, LanguageGrid, LanguageId, LanguageSpec};
pub use matcher::{CompiledPattern, Hit, search_in_tree, search_kind, search_regex, search_text};
pub use tool::AstTool;
