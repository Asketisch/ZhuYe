//! `MemoryScope` 与错误类型。

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 三种 memory scope。
///
/// - `Project` —— 相对 workspace,可入 VCS 共享。
/// - `User` —— 相对 home,跨项目共享。
/// - `Session` —— 仅内存;`FileMemoryStore` 从不持久化。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    Project,
    User,
    Session,
}

impl std::fmt::Display for MemoryScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            MemoryScope::Project => "project",
            MemoryScope::User => "user",
            MemoryScope::Session => "session",
        })
    }
}

/// memory 相关错误。
#[derive(Debug, Error)]
pub enum MemoryError {
    /// I/O 错误(文件不存在、权限拒绝等)。
    #[error("memory io error: {0}")]
    Io(#[from] std::io::Error),
    /// `Session` scope 无法持久化到磁盘。
    #[error("session memory cannot be persisted to disk")]
    SessionNotPersisted,
    /// scope / agent_type 组合非法。
    #[error("invalid memory configuration: {0}")]
    Invalid(String),
    /// 条目 key 非法(空 / 带首尾空白 / 含换行)。
    #[error("invalid memory key: {0:?}")]
    InvalidKey(String),
    /// 条目已存在(`create_only` 写入)。
    #[error("memory entry already exists: {key}")]
    EntryExists { key: String },
    /// 同一 key 对应多个标题块,拒绝猜测。
    #[error("memory entry has ambiguous headings: {key}")]
    AmbiguousHeadings { key: String },
    /// 同一 key 的标题块在文件内重复出现。
    #[error("memory entry has duplicate headings: {key}")]
    DuplicateHeadings { key: String },
    /// 原内容已变(乐观锁失败 / 整份替换前置校验失败)。
    #[error("memory document changed; reload before retrying")]
    DocumentChanged,
    /// value 里含 `## ` 标题,会污染条目边界。
    #[error("memory value cannot contain a level-two heading")]
    HeadingInValue,
    /// v1.11 M5a:目标条目已被用户删除(tombstone),自动导入 / 延迟
    /// 写入不得复活;显式工具写入经审批授权后可越过(留痕)。
    #[error("memory entry `{key}` was deleted by user; explicit revival required")]
    Tombstoned { key: String },
    /// v1.11 M5a:库 revision 与调用方期望不一致(乐观事务冲突),
    /// 重读后重新决策,不能套用旧结论。
    #[error("memory revision changed: expected {expected}, actual {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    /// v1.11 M5a:目标条目不存在(需要存在条目的结构化操作)。
    #[error("memory entry `{key}` not found")]
    EntryNotFound { key: String },
    /// v1.11 M5a:SQLite 后端错误(打开 / 事务 / 查询)。
    #[error("memory sqlite error: {0}")]
    Sqlite(String),
    /// v1.11 M5a:操作在当前后端不可用,附迁移指引(如 sqlite 模式下
    /// 的整文覆盖 save / 结构化文档的 replace_document)。
    #[error("memory operation unsupported in this backend: {0}")]
    Unsupported(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_display_matches_serde() {
        assert_eq!(MemoryScope::Project.to_string(), "project");
        assert_eq!(MemoryScope::User.to_string(), "user");
        assert_eq!(MemoryScope::Session.to_string(), "session");
    }

    #[test]
    fn scope_serde_roundtrip() {
        for s in [
            MemoryScope::Project,
            MemoryScope::User,
            MemoryScope::Session,
        ] {
            let j = serde_json::to_string(&s).unwrap();
            let back: MemoryScope = serde_json::from_str(&j).unwrap();
            assert_eq!(s, back);
        }
    }
}
