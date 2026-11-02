//! v1.x:记忆条目级编辑 —— `## <key>` 标题块的增 / 删 / 整份替换。
//!
//! 收编 ReflectDesktop `src-tauri/src/memory_store.rs` 的写侧。此前框架
//! 只有**读**路径(`MemoryStore::load` / `retrieve_relevant` / pre_loop
//! 注入),写侧散在客户端各自实现 —— 桌面端用核心的 `split_sections` /
//! `resolve_path` 解析,但增删逻辑、乐观锁、歧义检测都在 GUI 仓库里,
//! 于是「谁能写记忆」取决于客户端有没有抄一份,且 CLI / SDK 完全没有
//! 写入入口。
//!
//! ## 文件格式契约(与读侧逐字同源)
//!
//! ```text
//! ## <key>            ← 条目标题(整行精确匹配)
//! <value>             ← 正文(下一个 `## ` 之前)
//! ```
//!
//! - 解析走 [`crate::retrieval::split_sections`](与检索注入同一函数);
//! - 无法安全拆分的文件(空标题 / 重复标题 / 畸形标题)整体视为单条目,
//!   key 为 [`WHOLE_DOCUMENT_KEY`],只能整份替换;
//! - 写入走 `zhuye_fs::write_atomic`(symlink 穿透 / 权限不放大 /
//!   失败保全)。
//!
//! ## 安全不变量(从桌面端原样移植,均有回归测试)
//!
//! - **整行精确匹配** `## <key>`:子串匹配会让 key `"a"` 命中
//!   `## about` 的块起点,误删别的条目;
//! - **歧义 / 重复标题直接报错**:同一 key 出现多个标题块时拒绝写入,
//!   不猜;
//* - **value 不得含 `## ` 标题**:否则一次写入会污染条目边界;
//! - **乐观锁**(`expected_value`):基于列表快照发起的修改校验原内容,
//!   避免覆盖较新的记忆。

use std::path::Path;

use crate::model::{MemoryError, MemoryScope};
use crate::retrieval::split_sections;
use crate::store::FileMemoryStore;

/// 整份文档条目的保留 key(无法安全拆分的文件)。
pub const WHOLE_DOCUMENT_KEY: &str = "(all)";

/// 一个可编辑的记忆条目。
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryEntry {
    pub scope: MemoryScope,
    /// 条目 key(`## ` 标题文本);整份文档条目为 [`WHOLE_DOCUMENT_KEY`]。
    pub key: String,
    /// 正文(不含标题行)。
    pub value: String,
    /// `true` = 整份文档(无法安全拆分,只能整份替换)。
    pub whole_document: bool,
}

/// 条目写选项(增 / 删共用)。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EntryWriteOptions {
    /// 整份文档替换 / 删除(`key` 必须为 [`WHOLE_DOCUMENT_KEY`])。
    pub replace_document: bool,
    /// 仅当条目不存在时写入(已存在 → `EntryExists`)。
    pub create_only: bool,
    /// 乐观锁:原内容必须与给定值一致,否则 `DocumentChanged`。
    pub expected_value: Option<String>,
}

impl FileMemoryStore {
    /// 列出某 `(scope, agent_type)` 的全部记忆条目。
    ///
    /// 文件不存在 → 空列表。无法安全拆分的文件 → 单个整份文档条目。
    pub fn list_entries(
        &self,
        scope: MemoryScope,
        agent_type: &str,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        if matches!(scope, MemoryScope::Session) {
            return Err(MemoryError::SessionNotPersisted);
        }
        let content = read_memory_file(&self.path(scope, agent_type)?)?;
        Ok(memory_entries(scope, &content))
    }

    /// 新增 / 更新记忆条目。
    ///
    /// - `replace_document`(key = `(all)`):整份替换文件内容;
    /// - 常规路径:key 对应标题块存在 → 替换正文;不存在 → 追加
    ///   `\n## <key>\n<value>\n`;
    /// - `create_only`:已存在 → `EntryExists`;
    /// - `expected_value`:原内容不匹配 → `DocumentChanged`。
    pub fn add_entry(
        &self,
        scope: MemoryScope,
        agent_type: &str,
        key: &str,
        value: &str,
        opts: &EntryWriteOptions,
    ) -> Result<(), MemoryError> {
        validate_key(key)?;
        if opts.replace_document && opts.create_only {
            return Err(MemoryError::Invalid(
                "cannot create a whole-document replacement".into(),
            ));
        }
        // `(all)` 是整份文档保留 key:非整份形态写入会造出一个字面量
        // `## (all)` 标题块,与整份文档条目语义混淆(下次读取时该文件
        // 既不是纯整份文档、又含保留 key 条目),直接拒绝。
        if !opts.replace_document && key == WHOLE_DOCUMENT_KEY {
            return Err(MemoryError::Invalid(format!(
                "key `{WHOLE_DOCUMENT_KEY}` is reserved for whole-document replacement; \
                 pass replace_document = true"
            )));
        }
        let path = self.path(scope, agent_type)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut current = read_memory_file(&path)?;
        check_expected_value(scope, key, &current, opts)?;
        if opts.replace_document {
            ensure_whole_document(scope, key, &current)?;
            zhuye_fs::write_atomic(&path, value.as_bytes())?;
            return Ok(());
        }
        if value.lines().any(|line| line.starts_with("## ")) {
            return Err(MemoryError::HeadingInValue);
        }
        // create_only 双重校验:整份文档条目的 key 是 "(all)",条目 key
        // 集合里命中即算已存在(第一道);标题块计数命中算第二道。
        if opts.create_only
            && memory_entries(scope, &current)
                .iter()
                .any(|entry| entry.key == key)
        {
            return Err(MemoryError::EntryExists {
                key: key.to_string(),
            });
        }
        let heading = format!("## {key}");
        let matching = split_sections(scope, &current)
            .iter()
            .filter(|section| section.title == key)
            .count();
        if matching > 0 && opts.create_only {
            return Err(MemoryError::EntryExists {
                key: key.to_string(),
            });
        }
        if matching > 1 || (matching == 1 && find_exact_heading_line(&current, &heading).is_none())
        {
            return Err(MemoryError::AmbiguousHeadings {
                key: key.to_string(),
            });
        }
        if let Some((_start, line_end)) = find_exact_heading_line(&current, &heading) {
            let end = current[line_end..]
                .find("\n## ")
                .map(|i| line_end + i)
                .unwrap_or(current.len());
            if find_exact_heading_line(&current[end..], &heading).is_some() {
                return Err(MemoryError::DuplicateHeadings {
                    key: key.to_string(),
                });
            }
            let updated = format!("{}\n{}{}", &current[..line_end], value, &current[end..]);
            zhuye_fs::write_atomic(&path, updated.as_bytes())?;
            return Ok(());
        }
        if !current.is_empty() && !current.ends_with('\n') {
            current.push('\n');
        }
        current.push_str(&format!("\n## {key}\n{value}\n"));
        zhuye_fs::write_atomic(&path, current.as_bytes())?;
        Ok(())
    }

    /// 删除记忆条目。
    ///
    /// `replace_document`(key = `(all)`):删除整个 MEMORY.md 文件。
    /// 常规路径:整行精确匹配 `## <key>` 标题块并整块移除;条目不存在 →
    /// 静默成功(幂等删除),除非传了 `expected_value`(此时按缺失处理,
    /// 由乐观锁决定是否报错)。
    pub fn remove_entry(
        &self,
        scope: MemoryScope,
        agent_type: &str,
        key: &str,
        opts: &EntryWriteOptions,
    ) -> Result<(), MemoryError> {
        validate_key(key)?;
        if !opts.replace_document && key == WHOLE_DOCUMENT_KEY {
            // 与 add_entry 对称:保留 key 只接受整份删除形态。
            return Err(MemoryError::Invalid(format!(
                "key `{WHOLE_DOCUMENT_KEY}` is reserved for whole-document removal; \
                 pass replace_document = true"
            )));
        }
        let path = self.path(scope, agent_type)?;
        let current = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if opts.expected_value.is_none() {
                    return Ok(());
                }
                String::new()
            }
            Err(error) => return Err(error.into()),
        };
        check_expected_value(scope, key, &current, opts)?;
        if opts.replace_document {
            ensure_whole_document(scope, key, &current)?;
            std::fs::remove_file(path)?;
            return Ok(());
        }
        let heading = format!("## {key}");
        let matching = split_sections(scope, &current)
            .iter()
            .filter(|section| section.title == key)
            .count();
        if matching > 1 || (matching == 1 && find_exact_heading_line(&current, &heading).is_none())
        {
            return Err(MemoryError::AmbiguousHeadings {
                key: key.to_string(),
            });
        }
        if let Some((start, line_end)) = find_exact_heading_line(&current, &heading) {
            let end = current[line_end..]
                .find("\n## ")
                .map(|index| line_end + index)
                .unwrap_or(current.len());
            if find_exact_heading_line(&current[end..], &heading).is_some() {
                return Err(MemoryError::DuplicateHeadings {
                    key: key.to_string(),
                });
            }
            let mut updated = String::with_capacity(current.len());
            updated.push_str(&current[..start]);
            updated.push_str(&current[end..]);
            zhuye_fs::write_atomic(&path, updated.as_bytes())?;
        }
        Ok(())
    }
}

/// 旧 agent 目录迁移:把 `from_agent` 名下的 MEMORY.md 移到 `to_agent`。
///
/// 收编桌面端 `migrate_legacy_memory_dir`(桌面端曾把 agent 段写死
/// `reflect`,运行时按 agent 定义名读,两边对不上;首次启动时把旧数据
/// 搬到当前活跃 agent 目录)。只在**目标不存在、源存在**时 move ——
/// 不覆盖用户新写的记忆;两个 scope 各迁一次;失败只记日志不阻断
/// (记忆不是启动前提)。
pub fn migrate_legacy_agent_dir(workspace: &Path, home: &Path, from_agent: &str, to_agent: &str) {
    if from_agent == to_agent {
        return;
    }
    for scope in [MemoryScope::Project, MemoryScope::User] {
        let (Ok(legacy), Ok(target)) = (
            crate::scope::resolve_path(scope, workspace, home, from_agent),
            crate::scope::resolve_path(scope, workspace, home, to_agent),
        ) else {
            continue;
        };
        if !legacy.exists() || target.exists() {
            continue;
        }
        if let Some(parent) = target.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(error = %e, "创建迁移目标目录失败,跳过记忆迁移");
            continue;
        }
        match std::fs::rename(&legacy, &target) {
            Ok(()) => tracing::info!(
                from = %legacy.display(),
                to = %target.display(),
                "已把旧 agent 名记忆迁移到当前 agent 目录"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                from = %legacy.display(),
                "记忆迁移失败,保留原文件"
            ),
        }
    }
}

fn read_memory_file(path: &Path) -> Result<String, MemoryError> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

fn validate_key(key: &str) -> Result<(), MemoryError> {
    if key.is_empty() || key.trim() != key || key.contains('\n') || key.contains('\r') {
        return Err(MemoryError::InvalidKey(key.to_string()));
    }
    Ok(())
}

/// 乐观锁校验:基于列表快照发起的修改,原内容必须仍与 `expected_value`
/// 一致(同 key、同 whole_document 形态),否则拒绝覆盖较新的记忆。
fn check_expected_value(
    scope: MemoryScope,
    key: &str,
    content: &str,
    opts: &EntryWriteOptions,
) -> Result<(), MemoryError> {
    let Some(expected) = opts.expected_value.as_deref() else {
        return Ok(());
    };
    let matches = memory_entries(scope, content).into_iter().any(|entry| {
        entry.key == key && entry.value == expected && entry.whole_document == opts.replace_document
    });
    if !matches {
        return Err(MemoryError::DocumentChanged);
    }
    Ok(())
}

/// 把 memory 正文解析为条目列表;无法安全拆分(空标题 / 重复标题 /
/// 畸形标题)时整体视为单条目(`whole_document = true`)。
fn memory_entries(scope: MemoryScope, content: &str) -> Vec<MemoryEntry> {
    if content.trim().is_empty() {
        return Vec::new();
    }
    let sections: Vec<_> = split_sections(scope, content)
        .into_iter()
        .filter(|section| !section.title.is_empty() || !section.body.trim().is_empty())
        .collect();
    let mut keys = std::collections::HashSet::new();
    let malformed_heading = content
        .lines()
        .filter_map(|line| line.strip_prefix("## "))
        .any(|title| title.is_empty() || title.trim() != title);
    if malformed_heading
        || sections
            .iter()
            .any(|section| section.title.is_empty() || !keys.insert(section.title.clone()))
    {
        return vec![MemoryEntry {
            scope,
            key: WHOLE_DOCUMENT_KEY.to_string(),
            value: content.to_string(),
            whole_document: true,
        }];
    }
    sections
        .into_iter()
        .map(|section| {
            let heading = format!("## {}", section.title);
            let (_, line_end) =
                find_exact_heading_line(content, &heading).expect("validated heading");
            let end = content[line_end..]
                .find("\n## ")
                .map(|i| line_end + i)
                .unwrap_or(content.len());
            let body = if end > line_end {
                &content[line_end + 1..end]
            } else {
                ""
            };
            MemoryEntry {
                scope,
                key: section.title,
                value: body.to_string(),
                whole_document: false,
            }
        })
        .collect()
}

/// 整份替换 / 删除的前置校验:文件当前必须是「单个整份文档条目」形态。
fn ensure_whole_document(scope: MemoryScope, key: &str, content: &str) -> Result<(), MemoryError> {
    let entries = memory_entries(scope, content);
    if key != WHOLE_DOCUMENT_KEY || entries.len() != 1 || !entries[0].whole_document {
        return Err(MemoryError::DocumentChanged);
    }
    Ok(())
}

/// 在文本中查找整行恰为 `heading` 的行,返回 (行起始字节, 行尾字节,不含换行)。
///
/// 整行匹配(行首 + `trim_end` 后全等)是关键:子串匹配会让 key `"a"`
/// 命中 `## about` 的块起点,误删别的条目。
fn find_exact_heading_line(text: &str, heading: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(rel) = text[from..].find(heading) {
        let start = from + rel;
        let at_line_start = start == 0 || text.as_bytes()[start - 1] == b'\n';
        let line_end = text[start..]
            .find('\n')
            .map(|i| start + i)
            .unwrap_or(text.len());
        if at_line_start && text[start..line_end].trim_end() == heading {
            return Some((start, line_end));
        }
        from = start + heading.len();
    }
    None
}

#[cfg(test)]
mod tests;
