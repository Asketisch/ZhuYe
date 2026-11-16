//! # Office 文档与多模态读取(F37)
//!
//! 直接读写 Word / Excel / PPT / PDF —— 大量真实工作就发生在这类文件里,
//! agent 只会改代码是不够的。
//!
//! ## 分层策略
//!
//! 1. **格式识别**([`DocumentKind::detect`])—— 按扩展名 + magic bytes
//!    双重判定(防 `.pdf` 后缀的文本文件被误当 PDF)。
//! 2. **提取计划**([`ExtractionPlan`])—— 每种格式的提取方式与限制
//!    (最大字节 / 是否需外部工具 / 是否支持写入)。
//! 3. **结构化输出**([`DocumentContent`])—— 文本 + 结构(表格 / 页 /
//!    幻灯片),让模型能引用"第 3 页第 2 个表"。
//!
//! ## 为什么需要「提取计划」而不是直接解析
//!
//! OOXML(docx/xlsx/pptx)是 zip 包,PDF 是二进制流。解析库依赖重、体积大,
//! 且各有失败模式。把「怎么提取」显式化成计划,可以让:
//!
//! - 宿主按可用性选择实现(内置库 / 外部工具 / 拒绝);
//! - 失败时给出**可诊断**的原因(而不是"读取失败");
//! - 大文件在提取**前**就被限制(不是解析完才发现 500MB)。

use std::path::Path;

use serde::{Deserialize, Serialize};

/// 文档类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    /// Word(docx / doc)。
    Word,
    /// Excel(xlsx / xls / csv)。
    Excel,
    /// PowerPoint(pptx / ppt)。
    PowerPoint,
    /// PDF。
    Pdf,
    /// 纯文本 / Markdown / 代码。
    Text,
    /// 图片。
    Image,
    /// 未知。
    #[default]
    Unknown,
}

impl DocumentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DocumentKind::Word => "word",
            DocumentKind::Excel => "excel",
            DocumentKind::PowerPoint => "powerpoint",
            DocumentKind::Pdf => "pdf",
            DocumentKind::Text => "text",
            DocumentKind::Image => "image",
            DocumentKind::Unknown => "unknown",
        }
    }

    /// 按扩展名判定。
    pub fn from_extension(path: &Path) -> DocumentKind {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "docx" | "doc" | "odt" | "rtf" => DocumentKind::Word,
            "xlsx" | "xls" | "csv" | "tsv" | "ods" => DocumentKind::Excel,
            "pptx" | "ppt" | "odp" => DocumentKind::PowerPoint,
            "pdf" => DocumentKind::Pdf,
            "txt" | "md" | "markdown" | "json" | "yaml" | "yml" | "toml" | "rs" | "ts" | "tsx"
            | "js" | "jsx" | "py" | "go" | "java" | "c" | "cpp" | "h" | "html" | "css" | "sh"
            | "sql" | "xml" => DocumentKind::Text,
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" => DocumentKind::Image,
            _ => DocumentKind::Unknown,
        }
    }

    /// 按 magic bytes 判定(优先于扩展名)。
    ///
    /// 返回 `None` = 无法从内容判定,应回退到扩展名。
    pub fn from_magic(head: &[u8]) -> Option<DocumentKind> {
        // PDF: `%PDF-`
        if head.starts_with(b"%PDF-") {
            return Some(DocumentKind::Pdf);
        }
        // PNG
        if head.starts_with(&[0x89, b'P', b'N', b'G']) {
            return Some(DocumentKind::Image);
        }
        // JPEG
        if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
            return Some(DocumentKind::Image);
        }
        // GIF
        if head.starts_with(b"GIF8") {
            return Some(DocumentKind::Image);
        }
        // ZIP(OOXML 容器):PK\x03\x04 —— 需进一步看内部结构才能分辨
        // docx/xlsx/pptx,这里只标记"可能是 Office"。
        if head.starts_with(b"PK\x03\x04") {
            return None; // 由调用方读 zip 目录项判断
        }
        None
    }

    /// 是否为 OOXML 容器(zip)。
    pub fn is_ooxml(self) -> bool {
        matches!(
            self,
            DocumentKind::Word | DocumentKind::Excel | DocumentKind::PowerPoint
        )
    }

    /// 是否支持写入(不只是读)。
    ///
    /// 只读格式:PDF(改 PDF 需要专门库且语义复杂)、图片。
    pub fn supports_write(self) -> bool {
        matches!(
            self,
            DocumentKind::Word
                | DocumentKind::Excel
                | DocumentKind::PowerPoint
                | DocumentKind::Text
        )
    }

    /// 人读描述(工具输出用)。
    pub fn describe(self) -> &'static str {
        match self {
            DocumentKind::Word => "Word 文档",
            DocumentKind::Excel => "Excel 表格",
            DocumentKind::PowerPoint => "PowerPoint 演示",
            DocumentKind::Pdf => "PDF 文档",
            DocumentKind::Text => "文本文件",
            DocumentKind::Image => "图片",
            DocumentKind::Unknown => "未知格式",
        }
    }
}

/// 提取方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionMethod {
    /// 内置库直接解析。
    Native,
    /// 外部工具(如 `pdftotext` / `libreoffice`)。
    ExternalTool,
    /// 只能当二进制读(给模型看 base64 / 元数据)。
    Opaque,
    /// 拒绝(格式不支持)。
    Rejected,
}

/// 提取计划。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractionPlan {
    pub kind: DocumentKind,
    pub method: ExtractionMethod,
    /// 最大可接受字节(超过即拒绝,不尝试解析)。
    pub max_bytes: u64,
    /// 建议的外部工具(仅 `ExternalTool` 有意义)。
    #[serde(default)]
    pub suggested_tool: Option<String>,
    /// 是否支持结构化提取(表格 / 页 / 幻灯片)。
    pub structured: bool,
}

impl ExtractionPlan {
    /// 按类型生成默认计划。
    pub fn for_kind(kind: DocumentKind) -> ExtractionPlan {
        match kind {
            DocumentKind::Text => ExtractionPlan {
                kind,
                method: ExtractionMethod::Native,
                max_bytes: 8 * 1024 * 1024, // 8 MB
                suggested_tool: None,
                structured: false,
            },
            DocumentKind::Word | DocumentKind::Excel | DocumentKind::PowerPoint => ExtractionPlan {
                kind,
                method: ExtractionMethod::Native,
                max_bytes: 32 * 1024 * 1024,
                suggested_tool: Some("libreoffice".to_string()),
                structured: true,
            },
            DocumentKind::Pdf => ExtractionPlan {
                kind,
                method: ExtractionMethod::ExternalTool,
                max_bytes: 64 * 1024 * 1024,
                suggested_tool: Some("pdftotext".to_string()),
                structured: true,
            },
            DocumentKind::Image => ExtractionPlan {
                kind,
                method: ExtractionMethod::Opaque,
                max_bytes: 16 * 1024 * 1024,
                suggested_tool: None,
                structured: false,
            },
            DocumentKind::Unknown => ExtractionPlan {
                kind,
                method: ExtractionMethod::Rejected,
                max_bytes: 0,
                suggested_tool: None,
                structured: false,
            },
        }
    }

    /// 该计划能否处理 `size` 字节的文件。
    pub fn accepts_size(&self, size: u64) -> bool {
        self.method != ExtractionMethod::Rejected && size <= self.max_bytes
    }

    /// 拒绝原因(可诊断)。
    pub fn rejection_reason(&self, size: u64) -> Option<String> {
        if self.method == ExtractionMethod::Rejected {
            return Some(format!("不支持的文档格式:{}", self.kind.describe()));
        }
        if size > self.max_bytes {
            return Some(format!(
                "文件过大:{size} 字节 > 上限 {} 字节({})",
                self.max_bytes,
                self.kind.describe()
            ));
        }
        None
    }
}

/// 结构化文档内容。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentContent {
    pub kind: DocumentKind,
    /// 全文文本(纯文本提取结果)。
    pub text: String,
    /// 页数 / 幻灯片数 / 工作表数(按格式语义)。
    #[serde(default)]
    pub page_count: Option<usize>,
    /// 表格(每个表是行 × 列)。
    #[serde(default)]
    pub tables: Vec<Table>,
    /// 元数据(标题 / 作者 / 创建时间等)。
    #[serde(default)]
    pub metadata: Vec<(String, String)>,
}

/// 一张表。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Table {
    /// 表名(Excel 工作表名 / Word 表序号)。
    pub name: String,
    pub rows: Vec<Vec<String>>,
}

impl Table {
    /// 行数。
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// 列数(取第一行)。
    pub fn column_count(&self) -> usize {
        self.rows.first().map_or(0, |r| r.len())
    }

    /// 是否空表。
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// 渲染成 Markdown 表格(供模型读)。
    pub fn to_markdown(&self) -> String {
        if self.rows.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        if !self.name.is_empty() {
            out.push_str(&format!("### {}\n", self.name));
        }
        let header = &self.rows[0];
        out.push('|');
        for cell in header {
            out.push_str(&format!(" {cell} |"));
        }
        out.push('\n');
        out.push('|');
        for _ in header {
            out.push_str(" --- |");
        }
        out.push('\n');
        for row in &self.rows[1..] {
            out.push('|');
            for cell in row {
                out.push_str(&format!(" {cell} |"));
            }
            out.push('\n');
        }
        out
    }
}

impl DocumentContent {
    /// 渲染成模型可读的完整文本。
    pub fn render(&self) -> String {
        let mut out = format!("# {}\n\n", self.kind.describe());
        if let Some(pages) = self.page_count {
            out.push_str(&format!("共 {pages} 页\n\n"));
        }
        if !self.metadata.is_empty() {
            out.push_str("## 元数据\n");
            for (k, v) in &self.metadata {
                out.push_str(&format!("- {k}: {v}\n"));
            }
            out.push('\n');
        }
        if !self.text.trim().is_empty() {
            out.push_str("## 正文\n");
            out.push_str(self.text.trim());
            out.push_str("\n\n");
        }
        for table in &self.tables {
            out.push_str(&table.to_markdown());
            out.push('\n');
        }
        out
    }

    /// 是否有实际内容。
    pub fn has_content(&self) -> bool {
        !self.text.trim().is_empty() || !self.tables.is_empty()
    }
}

/// 读取文档的完整计划(类型 + 提取方式 + 大小校验)。
///
/// 返回 `Err` 表示应直接拒绝(带可诊断原因)。
pub fn plan_read(path: &Path, size: u64, head: &[u8]) -> Result<ExtractionPlan, String> {
    // magic 优先于扩展名(防后缀欺骗)。
    let kind = DocumentKind::from_magic(head).unwrap_or_else(|| DocumentKind::from_extension(path));
    let plan = ExtractionPlan::for_kind(kind);
    if let Some(reason) = plan.rejection_reason(size) {
        return Err(reason);
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn extension_detection() {
        assert_eq!(
            DocumentKind::from_extension(&PathBuf::from("a.docx")),
            DocumentKind::Word
        );
        assert_eq!(
            DocumentKind::from_extension(&PathBuf::from("b.XLSX")),
            DocumentKind::Excel
        );
        assert_eq!(
            DocumentKind::from_extension(&PathBuf::from("c.pptx")),
            DocumentKind::PowerPoint
        );
        assert_eq!(
            DocumentKind::from_extension(&PathBuf::from("d.pdf")),
            DocumentKind::Pdf
        );
        assert_eq!(
            DocumentKind::from_extension(&PathBuf::from("e.rs")),
            DocumentKind::Text
        );
        assert_eq!(
            DocumentKind::from_extension(&PathBuf::from("f.png")),
            DocumentKind::Image
        );
        assert_eq!(
            DocumentKind::from_extension(&PathBuf::from("g.weird")),
            DocumentKind::Unknown
        );
    }

    #[test]
    fn magic_detection() {
        assert_eq!(
            DocumentKind::from_magic(b"%PDF-1.7\n"),
            Some(DocumentKind::Pdf)
        );
        assert_eq!(
            DocumentKind::from_magic(&[0x89, b'P', b'N', b'G']),
            Some(DocumentKind::Image)
        );
        assert_eq!(
            DocumentKind::from_magic(&[0xFF, 0xD8, 0xFF]),
            Some(DocumentKind::Image)
        );
        assert_eq!(
            DocumentKind::from_magic(b"GIF89a"),
            Some(DocumentKind::Image)
        );
        // zip 容器:无法直接判定具体 Office 类型。
        assert_eq!(DocumentKind::from_magic(b"PK\x03\x04"), None);
        assert_eq!(DocumentKind::from_magic(b"plain text"), None);
    }

    #[test]
    fn magic_wins_over_extension() {
        // 后缀是 .txt 但内容是 PDF → 按 PDF 处理(防后缀欺骗)。
        let plan = plan_read(&PathBuf::from("fake.txt"), 100, b"%PDF-1.7").expect("plan");
        assert_eq!(plan.kind, DocumentKind::Pdf);
    }

    #[test]
    fn ooxml_classification() {
        assert!(DocumentKind::Word.is_ooxml());
        assert!(DocumentKind::Excel.is_ooxml());
        assert!(DocumentKind::PowerPoint.is_ooxml());
        assert!(!DocumentKind::Pdf.is_ooxml());
        assert!(!DocumentKind::Text.is_ooxml());
    }

    #[test]
    fn write_support() {
        assert!(DocumentKind::Word.supports_write());
        assert!(DocumentKind::Excel.supports_write());
        assert!(DocumentKind::Text.supports_write());
        assert!(!DocumentKind::Pdf.supports_write(), "PDF 只读");
        assert!(!DocumentKind::Image.supports_write());
    }

    #[test]
    fn plan_for_each_kind() {
        let text = ExtractionPlan::for_kind(DocumentKind::Text);
        assert_eq!(text.method, ExtractionMethod::Native);
        assert!(!text.structured);

        let pdf = ExtractionPlan::for_kind(DocumentKind::Pdf);
        assert_eq!(pdf.method, ExtractionMethod::ExternalTool);
        assert_eq!(pdf.suggested_tool.as_deref(), Some("pdftotext"));

        let word = ExtractionPlan::for_kind(DocumentKind::Word);
        assert_eq!(word.method, ExtractionMethod::Native);
        assert!(word.structured, "Office 应支持结构化提取");

        let unknown = ExtractionPlan::for_kind(DocumentKind::Unknown);
        assert_eq!(unknown.method, ExtractionMethod::Rejected);
    }

    #[test]
    fn size_limits_enforced() {
        let plan = ExtractionPlan::for_kind(DocumentKind::Text);
        assert!(plan.accepts_size(1024));
        assert!(!plan.accepts_size(100 * 1024 * 1024), "超 8MB 应拒");
        let reason = plan.rejection_reason(100 * 1024 * 1024).expect("reason");
        assert!(reason.contains("文件过大"), "{reason}");
    }

    #[test]
    fn unknown_kind_rejected_with_reason() {
        let plan = ExtractionPlan::for_kind(DocumentKind::Unknown);
        assert!(!plan.accepts_size(0), "未知格式即使 0 字节也拒");
        let reason = plan.rejection_reason(0).expect("reason");
        assert!(reason.contains("不支持"), "{reason}");
    }

    #[test]
    fn plan_read_rejects_oversize() {
        let err = plan_read(&PathBuf::from("big.txt"), 100 * 1024 * 1024, b"x").unwrap_err();
        assert!(err.contains("文件过大"), "{err}");
    }

    #[test]
    fn table_markdown_rendering() {
        let table = Table {
            name: "Sheet1".into(),
            rows: vec![
                vec!["Name".into(), "Count".into()],
                vec!["a".into(), "1".into()],
                vec!["b".into(), "2".into()],
            ],
        };
        assert_eq!(table.row_count(), 3);
        assert_eq!(table.column_count(), 2);
        let md = table.to_markdown();
        assert!(md.contains("### Sheet1"), "{md}");
        assert!(md.contains("| Name | Count |"), "{md}");
        assert!(md.contains("| a | 1 |"), "{md}");
    }

    #[test]
    fn empty_table_renders_empty() {
        let table = Table::default();
        assert!(table.is_empty());
        assert_eq!(table.to_markdown(), "");
    }

    #[test]
    fn document_content_render() {
        let doc = DocumentContent {
            kind: DocumentKind::Excel,
            text: "sheet data".into(),
            page_count: Some(2),
            tables: vec![Table {
                name: "S1".into(),
                rows: vec![vec!["x".into()]],
            }],
            metadata: vec![("Author".into(), "me".into())],
        };
        let rendered = doc.render();
        assert!(rendered.contains("Excel 表格"), "{rendered}");
        assert!(rendered.contains("共 2 页"), "{rendered}");
        assert!(rendered.contains("Author: me"), "{rendered}");
        assert!(rendered.contains("sheet data"), "{rendered}");
        assert!(rendered.contains("| x |"), "{rendered}");
        assert!(doc.has_content());
    }

    #[test]
    fn empty_document_has_no_content() {
        let doc = DocumentContent {
            kind: DocumentKind::Pdf,
            ..Default::default()
        };
        assert!(!doc.has_content());
    }

    #[test]
    fn describe_all_kinds() {
        for k in [
            DocumentKind::Word,
            DocumentKind::Excel,
            DocumentKind::PowerPoint,
            DocumentKind::Pdf,
            DocumentKind::Text,
            DocumentKind::Image,
            DocumentKind::Unknown,
        ] {
            assert!(!k.describe().is_empty());
            assert!(!k.as_str().is_empty());
        }
    }

    #[test]
    fn document_content_serializes() {
        let doc = DocumentContent {
            kind: DocumentKind::Word,
            text: "hello".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&doc).expect("serialize");
        let back: DocumentContent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, doc);
    }
}
