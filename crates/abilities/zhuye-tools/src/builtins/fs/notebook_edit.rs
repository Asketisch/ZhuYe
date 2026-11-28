//! `notebook_edit` — Jupyter `.ipynb` cell 级别编辑工具。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zhuye_protocol::PermissionMode;

use crate::sandbox::{PinnedWrite, open_readonly};
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

/// Notebook cell 类型(简化)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CellType {
    Code,
    Markdown,
    Raw,
}

/// ipynb cell 结构(最小子集)。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct NotebookCell {
    cell_type: CellType,
    #[serde(default)]
    source: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata: Option<Value>,
}

/// ipynb 根结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Notebook {
    #[serde(default)]
    cells: Vec<NotebookCell>,
    #[serde(default)]
    metadata: Value,
    #[serde(default = "default_nbformat")]
    nbformat: u32,
    #[serde(default = "default_nbformat_minor")]
    nbformat_minor: u32,
}

fn default_nbformat() -> u32 {
    4
}

fn default_nbformat_minor() -> u32 {
    5
}

pub struct NotebookEditTool;

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum NotebookAction {
    /// 列出 cell 索引与类型。
    List { path: String },
    /// 读取 cell 源码(0-based index)。
    Read { path: String, index: usize },
    /// 替换 cell 源码。
    Edit {
        path: String,
        index: usize,
        source: String,
        #[serde(default)]
        cell_type: Option<String>,
    },
    /// 在 index 处插入新 cell。
    Insert {
        path: String,
        index: usize,
        source: String,
        #[serde(default = "default_insert_type")]
        cell_type: String,
    },
    /// 删除 cell。
    Delete { path: String, index: usize },
}

fn default_insert_type() -> String {
    "code".into()
}

fn cell_source_string(source: &Value) -> String {
    match source {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr.iter().filter_map(|v| v.as_str()).collect::<String>(),
        _ => source.to_string(),
    }
}

fn set_cell_source(cell: &mut NotebookCell, text: &str) {
    cell.source = Value::String(text.to_string());
}

fn parse_cell_type(s: &str) -> Result<CellType, ToolError> {
    match s.to_lowercase().as_str() {
        "code" => Ok(CellType::Code),
        "markdown" | "md" => Ok(CellType::Markdown),
        "raw" => Ok(CellType::Raw),
        other => Err(ToolError::InvalidArgs {
            message: format!("unknown cell_type: {other}"),
        }),
    }
}

fn open_notebook(abs: &Path, path_str: &str) -> Result<(Notebook, std::path::PathBuf), ToolError> {
    let mut pinned =
        open_readonly(abs).map_err(|e| ToolError::Io(format!("open {path_str}: {e}")))?;
    let mut raw = String::new();
    use std::io::Read;
    pinned
        .file
        .read_to_string(&mut raw)
        .map_err(|e| ToolError::Io(format!("read {path_str}: {e}")))?;
    let nb: Notebook = serde_json::from_str(&raw).map_err(|e| ToolError::InvalidArgs {
        message: format!("invalid notebook JSON: {e}"),
    })?;
    Ok((nb, abs.to_path_buf()))
}

fn load_notebook(
    workspace: &Path,
    writable: &[PathBuf],
    path_str: &str,
) -> Result<(Notebook, std::path::PathBuf), ToolError> {
    // notebook 编辑是写路径:允许根内才可改,.git 等元数据拒绝。
    let abs = crate::sandbox::resolve_writable_path(workspace, writable, Path::new(path_str))?;
    open_notebook(&abs, path_str)
}

/// 写类 action(edit / insert / delete)的加载:越界路径经审批门强制
/// 询问(与 write / edit 同款 —— 写工作区外是一次人工确认),读类
/// action 保持旧口径(`resolve_writable_path` 硬拒)。
async fn load_notebook_with_approval(
    ctx: &crate::tool::ToolContext,
    action: &str,
    path_str: &str,
) -> Result<(Notebook, std::path::PathBuf), ToolError> {
    let abs = crate::sandbox::resolve_writable_path_with_approval(
        ctx,
        "notebook_edit",
        // 审批卡展示用最小载荷(完整 args 已在此前反序列化时消费)。
        &serde_json::json!({"action": action, "path": path_str}),
        Path::new(path_str),
    )
    .await?;
    open_notebook(&abs, path_str)
}

fn save_notebook(path: &Path, nb: &Notebook) -> Result<(), ToolError> {
    let j = serde_json::to_string_pretty(nb)
        .map_err(|e| ToolError::Execution(format!("serialize notebook: {e}")))?;
    let target = PinnedWrite::prepare(path)
        .map_err(|e| ToolError::Io(format!("prepare {}: {e}", path.display())))?;
    target
        .replace(j.as_bytes())
        .map_err(|e| ToolError::Io(format!("write {}: {e}", path.display())))
}

#[async_trait]
impl Tool for NotebookEditTool {
    fn name(&self) -> &str {
        "notebook_edit"
    }

    fn description(&self) -> &str {
        zhuye_prompt::copy("tool.notebook_edit")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "read", "edit", "insert", "delete"],
                    "description": "操作类型:list / read / edit / insert / delete"
                },
                "path": {
                    "type": "string",
                    "description": "Jupyter notebook(.ipynb)路径,相对 workspace"
                },
                "index": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "0-based cell 索引(read/edit/insert/delete 需要)"
                },
                "source": {
                    "type": "string",
                    "description": "新 cell 内容(edit: 完整替换;insert: 新 cell 内容)"
                },
                "cell_type": {
                    "type": "string",
                    "enum": ["code", "markdown", "raw"],
                    "description": "新 cell 类型(insert 时需要,默认 'code')"
                }
            },
            "required": ["action", "path"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Prompt
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let action: NotebookAction =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
                message: format!("invalid notebook_edit args: {e}"),
            })?;
        match action {
            NotebookAction::List { path } => {
                let (nb, _) =
                    load_notebook(&ctx.workspace_path(), &ctx.os_sandbox_writable, &path)?;
                let lines: Vec<String> = nb
                    .cells
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        format!(
                            "[{i}] {:?}",
                            serde_json::to_value(&c.cell_type).unwrap_or(Value::Null)
                        )
                    })
                    .collect();
                Ok(ToolOutput {
                    content: vec![zhuye_protocol::ContentBlock::text(lines.join("\n"))],
                    is_error: false,
                    metadata: serde_json::Value::Null,
                    elapsed_ms: 0,
                })
            }
            NotebookAction::Read { path, index } => {
                let (nb, _) =
                    load_notebook(&ctx.workspace_path(), &ctx.os_sandbox_writable, &path)?;
                let cell = nb.cells.get(index).ok_or_else(|| ToolError::InvalidArgs {
                    message: format!("cell index {index} out of range"),
                })?;
                Ok(ToolOutput {
                    content: vec![zhuye_protocol::ContentBlock::text(cell_source_string(
                        &cell.source,
                    ))],
                    is_error: false,
                    metadata: serde_json::Value::Null,
                    elapsed_ms: 0,
                })
            }
            NotebookAction::Edit {
                path,
                index,
                source,
                cell_type,
            } => {
                let (mut nb, abs) =
                    load_notebook_with_approval(&ctx, "edit", &path).await?;
                let cell = nb
                    .cells
                    .get_mut(index)
                    .ok_or_else(|| ToolError::InvalidArgs {
                        message: format!("cell index {index} out of range"),
                    })?;
                set_cell_source(cell, &source);
                if let Some(ct) = cell_type {
                    cell.cell_type = parse_cell_type(&ct)?;
                }
                save_notebook(&abs, &nb)?;
                Ok(ToolOutput {
                    content: vec![zhuye_protocol::ContentBlock::text(format!(
                        "edited cell {index}"
                    ))],
                    is_error: false,
                    metadata: serde_json::Value::Null,
                    elapsed_ms: 0,
                })
            }
            NotebookAction::Insert {
                path,
                index,
                source,
                cell_type,
            } => {
                let (mut nb, abs) =
                    load_notebook_with_approval(&ctx, "insert", &path).await?;
                if index > nb.cells.len() {
                    return Err(ToolError::InvalidArgs {
                        message: format!("insert index {index} out of range"),
                    });
                }
                let cell = NotebookCell {
                    cell_type: parse_cell_type(&cell_type)?,
                    source: Value::String(source),
                    metadata: None,
                };
                nb.cells.insert(index, cell);
                save_notebook(&abs, &nb)?;
                Ok(ToolOutput {
                    content: vec![zhuye_protocol::ContentBlock::text(format!(
                        "inserted cell at {index}"
                    ))],
                    is_error: false,
                    metadata: serde_json::Value::Null,
                    elapsed_ms: 0,
                })
            }
            NotebookAction::Delete { path, index } => {
                let (mut nb, abs) =
                    load_notebook_with_approval(&ctx, "delete", &path).await?;
                if index >= nb.cells.len() {
                    return Err(ToolError::InvalidArgs {
                        message: format!("cell index {index} out of range"),
                    });
                }
                nb.cells.remove(index);
                save_notebook(&abs, &nb)?;
                Ok(ToolOutput {
                    content: vec![zhuye_protocol::ContentBlock::text(format!(
                        "deleted cell {index}"
                    ))],
                    is_error: false,
                    metadata: serde_json::Value::Null,
                    elapsed_ms: 0,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn sample_nb() -> String {
        r##"{
            "cells": [
                {"cell_type": "markdown", "source": "# Hi"},
                {"cell_type": "code", "source": "1+1"}
            ],
            "metadata": {},
            "nbformat": 4,
            "nbformat_minor": 5
        }"##
        .to_string()
    }

    #[tokio::test]
    async fn list_cells() {
        use parking_lot::RwLock;
        use std::sync::Arc;
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "{}", sample_nb()).unwrap();
        let path = f.path().to_path_buf();
        let ws = path.parent().unwrap().to_path_buf();
        let rel = path.file_name().unwrap().to_string_lossy().to_string();
        let tool = NotebookEditTool;
        let out = tool
            .execute(
                ToolContext {
                    workspace: Arc::new(RwLock::new(ws)),
                    ..ToolContext::default()
                },
                serde_json::json!({"action": "list", "path": rel}),
            )
            .await
            .unwrap();
        assert!(format!("{:?}", out.content).contains("[0]"));
    }
}
