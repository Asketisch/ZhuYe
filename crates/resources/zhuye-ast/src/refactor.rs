//! # 跨文件符号重命名 + 语法预验证(F13)
//!
//! `edit::apply_rename` 只处理**单文件** word-boundary 替换。真实重构里
//! 改一个函数名会波及多个文件;而且改完如果语法坏了,应该**在写盘前**
//! 就发现,而不是让 agent 事后 `cargo check` 才发现。
//!
//! 本模块补两块:
//!
//! 1. [`rename_across_files`] —— 计划式重命名:输入一组 `(path, source)`,
//!    输出每个文件的替换结果 + unified diff + 命中数。**不写盘** ——
//!    调用方拿到 [`RenamePlan`] 决定是否落盘(propose/apply 两段)。
//!
//! 2. [`validate_syntax`] —— 用 tree-sitter 重新 parse 改写后的源码,
//!    若有 `ERROR` / `MISSING` 节点则拒绝(返回错误,不产生坏写盘)。
//!
//! ## 为什么是"计划"而不是直接写
//!
//! agent 改大仓库最容易翻车的地方是"改了一半失败"。计划式 API 让调用方
//! 先看完整 diff、再决定提交;`RenamePlan::all_valid` 给出写盘前的语法
//! 体检结果,`RenamePlan::is_noop` 避免空 diff 写盘。

use std::path::{Path, PathBuf};

use crate::AstError;
use crate::grid::LanguageGrid;

/// 单个文件的改写结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRenameOutcome {
    pub path: PathBuf,
    /// 改写后的完整源码(未写盘)。
    pub new_source: String,
    /// unified diff(空 = 无改动)。
    pub diff: String,
    /// 命中替换次数。
    pub hits: usize,
    /// 改写后语法是否通过 tree-sitter 校验(`None` = 该扩展无 grammar,
    /// 无法校验 —— 视为"不阻断但未验证")。
    pub syntax_ok: Option<bool>,
    /// 语法校验失败时的首个错误片段(诊断用)。
    pub syntax_error: Option<String>,
}

impl FileRenameOutcome {
    /// 是否有实际改动。
    pub fn changed(&self) -> bool {
        self.hits > 0
    }

    /// 是否可安全写盘(有改动时语法必须通过;无改动恒 true)。
    pub fn safe_to_write(&self) -> bool {
        if !self.changed() {
            return true;
        }
        // `None`(无 grammar)= 不阻断;`Some(false)` = 阻断。
        self.syntax_ok != Some(false)
    }
}

/// 跨文件重命名计划。
#[derive(Debug, Clone, Default)]
pub struct RenamePlan {
    pub from: String,
    pub to: String,
    pub files: Vec<FileRenameOutcome>,
}

impl RenamePlan {
    /// 总命中数。
    pub fn total_hits(&self) -> usize {
        self.files.iter().map(|f| f.hits).sum()
    }

    /// 是否空操作(没有任何文件命中)。
    pub fn is_noop(&self) -> bool {
        self.total_hits() == 0
    }

    /// 有改动的文件。
    pub fn changed_files(&self) -> Vec<&FileRenameOutcome> {
        self.files.iter().filter(|f| f.changed()).collect()
    }

    /// 全部改动是否都通过语法校验。
    pub fn all_valid(&self) -> bool {
        self.files.iter().all(|f| f.safe_to_write())
    }

    /// 语法失败的文件(诊断)。
    pub fn invalid_files(&self) -> Vec<&FileRenameOutcome> {
        self.files
            .iter()
            .filter(|f| f.syntax_ok == Some(false))
            .collect()
    }

    /// 拼接所有 diff(多文件 review 视图用)。
    pub fn combined_diff(&self) -> String {
        self.files
            .iter()
            .filter(|f| !f.diff.is_empty())
            .map(|f| {
                format!(
                    "--- {}\n{}",
                    f.path.display(),
                    f.diff.trim_start_matches("--- original\n")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// 重命名一个标识符,跨多个文件(不写盘)。
///
/// `sources` 是 `(path, content)` 列表;`grid` 用于按扩展名选 grammar
/// 做语法预验证。命中按 **word boundary** 语义(`edit::apply_rename`
/// 同款),保证 `foo` 不会误改 `foobar`。
pub fn rename_across_files(
    sources: &[(PathBuf, String)],
    from: &str,
    to: &str,
    grid: &LanguageGrid,
) -> Result<RenamePlan, AstError> {
    if from.is_empty() {
        return Err(AstError::BadPattern("rename: from is empty".into()));
    }
    if from == to {
        return Err(AstError::BadPattern(
            "rename: from and to are identical".into(),
        ));
    }
    // 标识符合法性前置:改成一个非法标识符会产出语法坏文件。
    if !is_identifier(to) {
        return Err(AstError::BadPattern(format!(
            "rename: '{to}' is not a valid identifier"
        )));
    }

    let mut plan = RenamePlan {
        from: from.to_string(),
        to: to.to_string(),
        files: Vec::with_capacity(sources.len()),
    };

    for (path, source) in sources {
        let (new_source, diff, hits) = crate::edit::apply_rename(source, from, to)?;
        // 语法预验证:只在真的改了才校验(省钱,也避免把既有坏文件算账)。
        let (syntax_ok, syntax_error) = if hits > 0 {
            validate_syntax(path, &new_source, grid)
        } else {
            (None, None)
        };
        plan.files.push(FileRenameOutcome {
            path: path.clone(),
            new_source,
            diff,
            hits,
            syntax_ok,
            syntax_error,
        });
    }

    Ok(plan)
}

/// 用 tree-sitter 校验一段源码的语法。
///
/// 返回 `(Some(true), None)` = 通过;`(Some(false), Some(片段))` = 有错;
/// `(None, None)` = 该扩展没有可用 grammar(不阻断)。
pub fn validate_syntax(
    path: &Path,
    source: &str,
    grid: &LanguageGrid,
) -> (Option<bool>, Option<String>) {
    let Some(lang) = grid.for_ext(path) else {
        return (None, None);
    };
    let tree = match grid.parse(lang, source) {
        Ok(t) => t,
        Err(e) => return (Some(false), Some(format!("parse failed: {e}"))),
    };
    let root = tree.root_node();
    if !root.has_error() {
        return (Some(true), None);
    }
    // 定位首个错误节点,取它附近 80 字符做诊断片段。
    if let Some(err) = find_first_error(root) {
        let start = err.start_byte().min(source.len());
        let end = err.end_byte().min(source.len());
        let snippet = source
            .get(start..end)
            .unwrap_or("")
            .chars()
            .take(80)
            .collect::<String>();
        let kind = if err.is_missing() { "MISSING" } else { "ERROR" };
        return (
            Some(false),
            Some(format!("{kind} at byte {start}: {snippet:?}")),
        );
    }
    // 有 error 但找不到具体节点 —— 仍然拒绝(保守)。
    (
        Some(false),
        Some("syntax error (node not located)".to_string()),
    )
}

/// 深度优先找第一个 `is_error` / `is_missing` 节点。
fn find_first_error(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    if node.is_error() || node.is_missing() {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = find_first_error(child) {
            return Some(found);
        }
    }
    None
}

/// 粗略标识符校验:首字符字母 / `_`,其余字母数字 / `_`。
///
/// 不追求完整的 Unicode 标识符规则 —— 目的是拦掉明显非法的重命名目标
/// (空串、含空格、含运算符),这些一定会产出语法坏文件。
fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn grid() -> Arc<LanguageGrid> {
        LanguageGrid::with_defaults()
    }

    #[test]
    fn identifier_validation() {
        assert!(is_identifier("foo"));
        assert!(is_identifier("_bar"));
        assert!(is_identifier("Bar2"));
        assert!(!is_identifier(""));
        assert!(!is_identifier("2foo"));
        assert!(!is_identifier("foo bar"));
        assert!(!is_identifier("foo-bar"));
        assert!(!is_identifier("foo()"));
    }

    #[test]
    fn rename_rejects_invalid_target() {
        let sources = vec![(PathBuf::from("a.rs"), "fn foo() {}".to_string())];
        let err = rename_across_files(&sources, "foo", "bad name", &grid());
        assert!(matches!(err, Err(AstError::BadPattern(_))));
    }

    #[test]
    fn rename_rejects_identical() {
        let sources = vec![(PathBuf::from("a.rs"), "fn foo() {}".to_string())];
        let err = rename_across_files(&sources, "foo", "foo", &grid());
        assert!(matches!(err, Err(AstError::BadPattern(_))));
    }

    #[test]
    fn rename_across_two_files_word_boundary() {
        let sources = vec![
            (PathBuf::from("a.rs"), "fn foo() { foo(); }\n".to_string()),
            (
                PathBuf::from("b.rs"),
                "const X: &str = \"foobar\";\nfn call() { foo(); }\n".to_string(),
            ),
        ];
        let plan = rename_across_files(&sources, "foo", "renamed", &grid()).expect("plan");
        assert!(!plan.is_noop());
        let changed = plan.changed_files();
        assert_eq!(changed.len(), 2);
        // a.rs: 两处命中(fn 名 + 调用)
        assert_eq!(changed[0].hits, 2);
        // b.rs: 一处命中;`foobar` 不能被改(word boundary)
        assert_eq!(changed[1].hits, 1);
        assert!(
            changed[1].new_source.contains("foobar"),
            "word boundary 必须保留 foobar:{}",
            changed[1].new_source
        );
        assert!(plan.all_valid(), "改写后语法应通过");
    }

    #[test]
    fn validate_syntax_rejects_broken_source() {
        // 直接喂一段语法坏源码:`fn foo( {` 缺右括号与返回体。
        let (ok, err) = validate_syntax(Path::new("bad.rs"), "fn foo( -> i32 { 1 \n", &grid());
        assert_eq!(ok, Some(false), "坏语法必须被拒绝");
        assert!(err.is_some(), "应给出诊断");
    }

    #[test]
    fn validate_syntax_accepts_good_source() {
        let (ok, err) = validate_syntax(Path::new("good.rs"), "fn foo() -> i32 { 1 }\n", &grid());
        assert_eq!(ok, Some(true));
        assert!(err.is_none());
    }

    #[test]
    fn rename_to_invalid_identifier_rejected_before_plan() {
        // 非标识符目标在**计划阶段**就被拒(不产生坏写盘)。
        let sources = vec![(PathBuf::from("a.rs"), "fn foo() -> i32 { 1 }\n".to_string())];
        for bad in ["{", "1x", "a b", "a-b", ""] {
            let r = rename_across_files(&sources, "foo", bad, &grid());
            assert!(matches!(r, Err(AstError::BadPattern(_))), "{bad:?} 应被拒");
        }
    }

    #[test]
    fn no_hit_plan_is_noop() {
        let sources = vec![(PathBuf::from("a.rs"), "fn other() {}".to_string())];
        let plan = rename_across_files(&sources, "ghost", "x", &grid()).expect("plan");
        assert!(plan.is_noop());
        assert_eq!(plan.total_hits(), 0);
        assert!(plan.all_valid(), "无改动视为可写");
    }

    #[test]
    fn unknown_extension_skips_syntax_check() {
        let sources = vec![(PathBuf::from("a.unknownext"), "some foo text".to_string())];
        let plan = rename_across_files(&sources, "foo", "bar", &grid()).expect("plan");
        let changed = plan.changed_files();
        assert_eq!(changed[0].syntax_ok, None, "无 grammar 不做语法校验");
        assert!(changed[0].safe_to_write(), "未验证不阻断");
    }

    #[test]
    fn combined_diff_has_all_files() {
        let sources = vec![
            (PathBuf::from("a.rs"), "fn foo() {}\n".to_string()),
            (PathBuf::from("b.rs"), "fn foo() {}\n".to_string()),
        ];
        let plan = rename_across_files(&sources, "foo", "bar", &grid()).expect("plan");
        let combined = plan.combined_diff();
        assert!(combined.contains("a.rs"), "{combined}");
        assert!(combined.contains("b.rs"), "{combined}");
    }

    #[test]
    fn python_syntax_validation_works() {
        let sources = vec![(
            PathBuf::from("a.py"),
            "def foo():\n    return 1\n".to_string(),
        )];
        let plan = rename_across_files(&sources, "foo", "bar", &grid()).expect("plan");
        assert_eq!(plan.changed_files()[0].syntax_ok, Some(true));
    }
}
