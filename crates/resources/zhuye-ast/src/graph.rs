//! # 代码知识图谱(F14)
//!
//! 把全仓的「定义 / 引用 / 导入」抽成图,回答两个问题:
//!
//! 1. **改这里会波及谁** —— [`CodeGraph::callers_of`] / [`CodeGraph::affected_files`];
//! 2. **这个符号在哪定义** —— [`CodeGraph::find_definitions`].
//!
//! ## 为什么用「名字级」而不是「类型级」
//!
//! 精确的调用图需要类型解析(等价于半个编译器)。本实现走 **tree-sitter
//! 语法级**抽取:抓 `function_definition` / `function_item` / `method_definition`
//! 等定义节点 + 所有 `identifier` 出现位置。代价是会有同名误报(`get` 在
//! 十个类里都有),收益是**零配置、跨语言、无编译依赖**。误报由调用方
//! 按文件路径过滤 —— 这对"改这个文件会波及哪些文件"这个主要用途足够。
//!
//! ## 增量索引
//!
//! [`CodeGraph::index_file`] 可对单文件重索引(内容哈希未变则跳过)。
//! 文件级粒度对 agent 场景够用:改一个文件重索引一个文件,O(该文件大小)。
//!
//! ## 与 F13 的关系
//!
//! F13 做**改写**,F14 做**影响面分析** —— 改之前先查 `affected_files`,
//! 就知道要跑哪些测试、要不要通知别的模块。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::grid::{AstError, LanguageGrid, LanguageId};

/// 一个符号定义。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SymbolDef {
    pub name: String,
    pub path: PathBuf,
    /// 定义所在行(1 起)。
    pub line: usize,
    /// 定义种类(`function` / `class` / `method` / `struct` / `trait` 等)。
    pub kind: String,
}

/// 一处符号引用。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SymbolRef {
    pub name: String,
    pub path: PathBuf,
    pub line: usize,
}

/// 一条导入边(`from` 文件 import 了 `module`)。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ImportEdge {
    pub path: PathBuf,
    pub module: String,
    pub line: usize,
}

/// 单文件索引结果。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIndex {
    pub defs: Vec<SymbolDef>,
    pub refs: Vec<SymbolRef>,
    pub imports: Vec<ImportEdge>,
    /// 内容指纹(用于增量索引跳过)。
    pub content_hash: u64,
}

/// 影响面分析结果。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactReport {
    /// 被改符号的定义位置。
    pub definitions: Vec<SymbolDef>,
    /// 引用了这些符号的文件(不含定义所在文件)。
    pub affected_files: Vec<PathBuf>,
    /// 引用总数。
    pub reference_count: usize,
}

impl ImpactReport {
    /// 是否会影响别的文件。
    pub fn has_impact(&self) -> bool {
        !self.affected_files.is_empty()
    }
}

/// 代码图。
#[derive(Debug, Default)]
pub struct CodeGraph {
    /// `path -> FileIndex`
    files: BTreeMap<PathBuf, FileIndex>,
    /// `symbol -> 定义位置集合`(倒排索引,查询 O(1))
    defs_by_name: HashMap<String, BTreeSet<(PathBuf, usize, String)>>,
}

impl CodeGraph {
    pub fn new() -> CodeGraph {
        CodeGraph::default()
    }

    /// 已索引的文件数。
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// 全部符号名(排序)。
    pub fn symbol_names(&self) -> Vec<String> {
        let mut out: Vec<String> = self.defs_by_name.keys().cloned().collect();
        out.sort();
        out
    }

    /// 索引单个文件。内容哈希未变则跳过(返回 `false` = 未变)。
    pub fn index_file(
        &mut self,
        path: &Path,
        source: &str,
        grid: &LanguageGrid,
    ) -> Result<bool, AstError> {
        let hash = content_hash(source);
        if let Some(existing) = self.files.get(path)
            && existing.content_hash == hash
        {
            return Ok(false);
        }
        let Some(lang) = grid.for_ext(path) else {
            // 无 grammar:记一个空索引(避免每次重试),但不产生符号。
            self.replace_index(
                path.to_path_buf(),
                FileIndex {
                    content_hash: hash,
                    ..FileIndex::default()
                },
            );
            return Ok(true);
        };
        let index = extract_index(path, source, grid, lang)?;
        self.replace_index(path.to_path_buf(), index);
        Ok(true)
    }

    fn replace_index(&mut self, path: PathBuf, index: FileIndex) {
        // 先摘掉旧索引里的符号。
        if let Some(old) = self.files.remove(&path) {
            for def in old.defs {
                if let Some(set) = self.defs_by_name.get_mut(&def.name) {
                    set.remove(&(def.path.clone(), def.line, def.kind.clone()));
                    if set.is_empty() {
                        self.defs_by_name.remove(&def.name);
                    }
                }
            }
        }
        for def in &index.defs {
            self.defs_by_name
                .entry(def.name.clone())
                .or_default()
                .insert((def.path.clone(), def.line, def.kind.clone()));
        }
        self.files.insert(path, index);
    }

    /// 查符号定义。
    pub fn find_definitions(&self, name: &str) -> Vec<SymbolDef> {
        // map_or 而非 map+unwrap_or_default:新版 clippy 的 map_unwrap_or
        // 已提级为默认告警,CI(-D warnings)拒收。
        self.defs_by_name.get(name).map_or(Vec::new(), |set| {
            set.iter()
                .map(|(path, line, kind)| SymbolDef {
                    name: name.to_string(),
                    path: path.clone(),
                    line: *line,
                    kind: kind.clone(),
                })
                .collect()
        })
    }

    /// 查引用了 `name` 的所有位置。
    pub fn find_references(&self, name: &str) -> Vec<SymbolRef> {
        let mut out: Vec<SymbolRef> = self
            .files
            .values()
            .flat_map(|f| f.refs.iter())
            .filter(|r| r.name == name)
            .cloned()
            .collect();
        out.sort();
        out
    }

    /// `name` 的调用者文件(排除定义所在文件 —— 同文件内改不算"波及")。
    pub fn callers_of(&self, name: &str) -> Vec<PathBuf> {
        let def_paths: BTreeSet<PathBuf> = self
            .find_definitions(name)
            .into_iter()
            .map(|d| d.path)
            .collect();
        let mut out: BTreeSet<PathBuf> = self
            .find_references(name)
            .into_iter()
            .map(|r| r.path)
            .filter(|p| !def_paths.contains(p))
            .collect();
        out.remove(Path::new(""));
        out.into_iter().collect()
    }

    /// 影响面:改了 `name` 会波及哪些文件。
    pub fn impact_of(&self, name: &str) -> ImpactReport {
        let definitions = self.find_definitions(name);
        let refs = self.find_references(name);
        let def_paths: BTreeSet<PathBuf> = definitions.iter().map(|d| d.path.clone()).collect();
        let mut affected: BTreeSet<PathBuf> = refs
            .iter()
            .map(|r| r.path.clone())
            .filter(|p| !def_paths.contains(p))
            .collect();
        affected.remove(Path::new(""));
        ImpactReport {
            definitions,
            affected_files: affected.into_iter().collect(),
            reference_count: refs.len(),
        }
    }

    /// 某个文件 import 的模块。
    pub fn imports_of(&self, path: &Path) -> Vec<String> {
        self.files.get(path).map_or(Vec::new(), |f| {
            f.imports.iter().map(|i| i.module.clone()).collect()
        })
    }

    /// 被 `module` 导入的文件(反向导入边)。
    pub fn imported_by(&self, module: &str) -> Vec<PathBuf> {
        let mut out: BTreeSet<PathBuf> = self
            .files
            .values()
            .flat_map(|f| f.imports.iter())
            .filter(|i| i.module == module)
            .map(|i| i.path.clone())
            .collect();
        out.remove(Path::new(""));
        out.into_iter().collect()
    }

    /// 全图统计(诊断 / UI)。
    pub fn stats(&self) -> GraphStats {
        GraphStats {
            files: self.files.len(),
            symbols: self.defs_by_name.len(),
            definitions: self.files.values().map(|f| f.defs.len()).sum(),
            references: self.files.values().map(|f| f.refs.len()).sum(),
            imports: self.files.values().map(|f| f.imports.len()).sum(),
        }
    }
}

/// 图统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphStats {
    pub files: usize,
    pub symbols: usize,
    pub definitions: usize,
    pub references: usize,
    pub imports: usize,
}

/// 从源码抽索引。
fn extract_index(
    path: &Path,
    source: &str,
    grid: &LanguageGrid,
    lang: LanguageId,
) -> Result<FileIndex, AstError> {
    let tree = grid.parse(lang, source)?;
    let mut defs = Vec::new();
    let mut refs = Vec::new();
    let mut imports = Vec::new();
    let bytes = source.as_bytes();
    walk(
        tree.root_node(),
        bytes,
        path,
        lang,
        &mut defs,
        &mut refs,
        &mut imports,
    );
    Ok(FileIndex {
        defs,
        refs,
        imports,
        content_hash: content_hash(source),
    })
}

/// 递归 walk,按节点类型归类。
fn walk(
    node: tree_sitter::Node<'_>,
    src: &[u8],
    path: &Path,
    lang: LanguageId,
    defs: &mut Vec<SymbolDef>,
    refs: &mut Vec<SymbolRef>,
    imports: &mut Vec<ImportEdge>,
) {
    let kind = node.kind();
    let line = node.start_position().row + 1;

    // 1) 定义节点:取它的 name 子节点。
    if is_definition_kind(kind) {
        if let Some(name_node) = node.child_by_field_name("name")
            && let Ok(name) = name_node.utf8_text(src)
            && !name.is_empty()
        {
            defs.push(SymbolDef {
                name: name.to_string(),
                path: path.to_path_buf(),
                line,
                kind: normalize_def_kind(kind),
            });
        }
    }

    // 2) 导入节点:取模块路径文本。
    if is_import_kind(kind, lang) {
        if let Ok(text) = node.utf8_text(src) {
            for module in extract_module_names(text, lang) {
                imports.push(ImportEdge {
                    path: path.to_path_buf(),
                    module,
                    line,
                });
            }
        }
    }

    // 3) 引用:identifier 出现(定义名本身也记一次,便于查"同名多处")。
    if kind == "identifier"
        && let Ok(name) = node.utf8_text(src)
        && !name.is_empty()
        && !name.starts_with('_')
    {
        refs.push(SymbolRef {
            name: name.to_string(),
            path: path.to_path_buf(),
            line,
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, src, path, lang, defs, refs, imports);
    }
}

/// 各语言的「定义」节点类型。
fn is_definition_kind(kind: &str) -> bool {
    matches!(
        kind,
        // rust
        "function_item"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "impl_item"
            | "type_item"
            | "const_item"
            | "static_item"
            | "mod_item"
            // python
            | "function_definition"
            | "class_definition"
            // js / ts
            | "function_declaration"
            | "class_declaration"
            | "method_definition"
            | "interface_declaration"
            | "type_alias_declaration"
            | "variable_declarator"
            // go
            | "method_declaration"
            | "type_declaration"
    )
}

fn normalize_def_kind(kind: &str) -> String {
    match kind {
        "function_item" | "function_definition" | "function_declaration" => "function",
        "method_definition" | "method_declaration" => "method",
        "struct_item" => "struct",
        "enum_item" => "enum",
        "trait_item" | "interface_declaration" => "trait",
        "impl_item" => "impl",
        "type_item" | "type_alias_declaration" | "type_declaration" => "type",
        "const_item" | "static_item" => "const",
        "class_definition" | "class_declaration" => "class",
        "mod_item" => "module",
        "variable_declarator" => "variable",
        other => other,
    }
    .to_string()
}

fn is_import_kind(kind: &str, lang: LanguageId) -> bool {
    match lang.as_str() {
        "rust" => kind == "use_declaration",
        "python" => matches!(kind, "import_statement" | "import_from_statement"),
        "typescript" | "javascript" => kind == "import_statement",
        "go" => kind == "import_spec",
        _ => false,
    }
}

/// 从导入语句文本里抽模块名(粗粒度:按常见分隔符切)。
fn extract_module_names(text: &str, lang: LanguageId) -> Vec<String> {
    let mut out = Vec::new();
    match lang.as_str() {
        "rust" => {
            // `use a::b::c;` → "a::b::c";`use a::{b, c};` → "a::b","a::c"
            let body = text
                .trim()
                .trim_start_matches("use ")
                .trim_end_matches(';')
                .trim();
            if let Some((prefix, inner)) = body.split_once("::{") {
                let inner = inner.trim_end_matches('}');
                for part in inner.split(',') {
                    let p = part.trim();
                    if !p.is_empty() {
                        out.push(format!("{prefix}::{p}"));
                    }
                }
            } else if !body.is_empty() {
                out.push(body.to_string());
            }
        }
        "python" => {
            // `from a.b import c` → "a.b";`import a.b` → "a.b"
            let t = text.trim();
            if let Some(rest) = t.strip_prefix("from ") {
                if let Some((module, _)) = rest.split_once(" import ") {
                    out.push(module.trim().to_string());
                }
            } else if let Some(rest) = t.strip_prefix("import ") {
                for part in rest.split(',') {
                    let p = part.trim();
                    if !p.is_empty() {
                        out.push(p.to_string());
                    }
                }
            }
        }
        "typescript" | "javascript" => {
            // `import x from "mod"` / `import "mod"`
            if let Some(idx) = text.find("from ") {
                let rest = &text[idx + 5..];
                if let Some(mod_name) = first_quoted(rest) {
                    out.push(mod_name);
                }
            } else if let Some(mod_name) = first_quoted(text) {
                out.push(mod_name);
            }
        }
        "go" => {
            if let Some(mod_name) = first_quoted(text) {
                out.push(mod_name);
            }
        }
        _ => {}
    }
    out
}

fn first_quoted(s: &str) -> Option<String> {
    let start = s.find(['"', '`'])?;
    let quote = s.as_bytes()[start] as char;
    let rest = &s[start + 1..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

/// 稳定的内容哈希(FNV-1a 64,足够做变更检测)。
fn content_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn grid() -> Arc<LanguageGrid> {
        LanguageGrid::with_defaults()
    }

    fn index(graph: &mut CodeGraph, path: &str, src: &str) {
        graph
            .index_file(Path::new(path), src, &grid())
            .expect("index");
    }

    #[test]
    fn indexes_rust_definitions() {
        let mut g = CodeGraph::new();
        index(
            &mut g,
            "src/lib.rs",
            "pub struct Foo;\npub fn bar() {}\nfn baz() { bar(); }\n",
        );
        assert_eq!(g.file_count(), 1);
        assert_eq!(g.find_definitions("Foo")[0].kind, "struct");
        assert_eq!(g.find_definitions("bar")[0].kind, "function");
        assert_eq!(g.find_definitions("baz")[0].kind, "function");
    }

    #[test]
    fn python_definitions() {
        let mut g = CodeGraph::new();
        index(&mut g, "a.py", "class C:\n    pass\n\ndef f():\n    pass\n");
        assert_eq!(g.find_definitions("C")[0].kind, "class");
        assert_eq!(g.find_definitions("f")[0].kind, "function");
    }

    #[test]
    fn callers_exclude_definition_file() {
        let mut g = CodeGraph::new();
        index(
            &mut g,
            "a.rs",
            "pub fn target() {}\nfn local() { target(); }\n",
        );
        index(&mut g, "b.rs", "fn other() { target(); }\n");
        let callers = g.callers_of("target");
        assert_eq!(callers, vec![PathBuf::from("b.rs")], "同文件不算波及");
    }

    #[test]
    fn impact_report_aggregates() {
        let mut g = CodeGraph::new();
        index(&mut g, "a.rs", "pub fn target() {}\n");
        index(&mut g, "b.rs", "fn x() { target(); target(); }\n");
        index(&mut g, "c.rs", "fn y() { target(); }\n");
        let impact = g.impact_of("target");
        assert_eq!(impact.definitions.len(), 1);
        assert_eq!(impact.affected_files.len(), 2);
        assert!(impact.reference_count >= 3);
        assert!(impact.has_impact());
    }

    #[test]
    fn unused_symbol_has_no_impact() {
        let mut g = CodeGraph::new();
        index(&mut g, "a.rs", "pub fn lonely() {}\n");
        let impact = g.impact_of("lonely");
        assert!(!impact.has_impact(), "无引用则无波及文件");
        assert_eq!(impact.definitions.len(), 1);
    }

    #[test]
    fn rust_import_extraction() {
        let mut g = CodeGraph::new();
        index(&mut g, "a.rs", "use std::collections::HashMap;\n");
        assert_eq!(
            g.imports_of(Path::new("a.rs")),
            vec!["std::collections::HashMap"]
        );
    }

    #[test]
    fn rust_braced_import_extraction() {
        let mut g = CodeGraph::new();
        index(&mut g, "a.rs", "use crate::{alpha, beta};\n");
        let imports = g.imports_of(Path::new("a.rs"));
        assert!(imports.contains(&"crate::alpha".to_string()), "{imports:?}");
        assert!(imports.contains(&"crate::beta".to_string()), "{imports:?}");
    }

    #[test]
    fn python_import_extraction() {
        let mut g = CodeGraph::new();
        index(&mut g, "a.py", "from os.path import join\nimport sys\n");
        let imports = g.imports_of(Path::new("a.py"));
        assert!(imports.contains(&"os.path".to_string()), "{imports:?}");
        assert!(imports.contains(&"sys".to_string()), "{imports:?}");
    }

    #[test]
    fn imported_by_reverse_edges() {
        let mut g = CodeGraph::new();
        index(&mut g, "a.py", "import requests\n");
        index(&mut g, "b.py", "import requests\n");
        let users = g.imported_by("requests");
        assert_eq!(users.len(), 2);
    }

    #[test]
    fn incremental_index_skips_unchanged() {
        let mut g = CodeGraph::new();
        assert!(
            g.index_file(Path::new("a.rs"), "fn f() {}", &grid())
                .unwrap()
        );
        // 同内容再索引 → false(跳过)
        assert!(
            !g.index_file(Path::new("a.rs"), "fn f() {}", &grid())
                .unwrap()
        );
        // 内容变了 → true
        assert!(
            g.index_file(Path::new("a.rs"), "fn g() {}", &grid())
                .unwrap()
        );
        assert!(g.find_definitions("f").is_empty(), "旧符号应被摘除");
        assert!(!g.find_definitions("g").is_empty());
    }

    #[test]
    fn unknown_extension_indexed_empty() {
        let mut g = CodeGraph::new();
        assert!(
            g.index_file(Path::new("x.weird"), "anything", &grid())
                .unwrap()
        );
        assert_eq!(g.file_count(), 1);
        assert!(g.symbol_names().is_empty());
    }

    #[test]
    fn stats_counts() {
        let mut g = CodeGraph::new();
        index(&mut g, "a.rs", "pub fn f() {}\n");
        index(&mut g, "b.rs", "fn g() { f(); }\n");
        let s = g.stats();
        assert_eq!(s.files, 2);
        assert!(s.definitions >= 2);
        assert!(s.references >= 2);
    }

    #[test]
    fn symbol_names_sorted() {
        let mut g = CodeGraph::new();
        index(&mut g, "a.rs", "fn zebra() {}\nfn apple() {}\n");
        let names = g.symbol_names();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        assert!(names.contains(&"apple".to_string()));
    }
}
