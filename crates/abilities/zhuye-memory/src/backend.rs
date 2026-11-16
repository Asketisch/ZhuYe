//! 记忆后端选择与装配(v1.11 M5a)。
//!
//! 渐进启用口径(plan §12):新模式经 `ZHUYE_MEMORY_BACKEND=sqlite`
//! 显式开启,默认保持 Markdown 旧行为 —— 两条路径共享同一组接口
//! (`MemoryStore::retrieve` 检索路由 + [`crate::editor::MemoryEditor`]
//! 条目编辑),宿主只换装配,不换调用点。

use std::path::Path;
use std::sync::Arc;

use crate::sqlite::SqliteMemoryStore;
use crate::store::{FileMemoryStore, InMemoryStore, MemoryStore};

/// 记忆后端形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryBackend {
    /// 旧路径:MEMORY.md 文件 + BM25 分节检索(默认)。
    Markdown,
    /// 新路径:SQLite 条目版本库 + FTS5 中文检索 + MEMORY.md 投影。
    Sqlite,
}

impl std::fmt::Display for MemoryBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            MemoryBackend::Markdown => "markdown",
            MemoryBackend::Sqlite => "sqlite",
        })
    }
}

/// 从 `ZHUYE_MEMORY_BACKEND` 解析后端:`sqlite`(或别名 `v2`)开启新
/// 模式;未设 / 其他值保持 Markdown(渐进启用,默认不换行为)。
pub fn backend_from_env() -> MemoryBackend {
    match std::env::var("ZHUYE_MEMORY_BACKEND")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "sqlite" | "v2" => MemoryBackend::Sqlite,
        _ => MemoryBackend::Markdown,
    }
}

/// 一次装配的完整记忆栈:宿主(bootstrap / 库门面)从这里取全部句柄。
pub struct MemoryStack {
    pub backend: MemoryBackend,
    /// M4Deps.memory 注入的读侧(检索路由在此切换)。
    pub store: Arc<dyn MemoryStore>,
    /// 文件 store:topic 投影与旧模式条目编辑共用。
    pub file: Arc<FileMemoryStore>,
    /// `Some` = 新模式:条目写入必须路由到 SQLite 变更端口
    /// (禁止继续文件写旁路);`None` = 旧模式。
    pub sqlite: Option<Arc<SqliteMemoryStore>>,
}

/// 按环境变量装配记忆栈。旧模式与既有 bootstrap 完全同构
/// (CompositeMemoryStore:Session 走内存、Project/User 走文件);
/// 新模式下 Session scope 语义不变(不落盘,读空)。
pub fn bootstrap_stack(workspace: &Path, user_data_dir: &Path) -> MemoryStack {
    let file = Arc::new(FileMemoryStore::with_user_data_dir(
        workspace,
        user_data_dir,
    ));
    let backend = backend_from_env();
    match backend {
        MemoryBackend::Sqlite => {
            let sqlite = Arc::new(SqliteMemoryStore::open(workspace, user_data_dir));
            MemoryStack {
                backend,
                store: sqlite.clone(),
                file,
                sqlite: Some(sqlite),
            }
        }
        MemoryBackend::Markdown => {
            let store: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::with_fallback(file.clone()));
            MemoryStack {
                backend,
                store,
                file,
                sqlite: None,
            }
        }
    }
}

/// 新模式判断便捷谓词(宿主侧据此禁用未适配的旁路写入)。
pub fn is_sqlite_backend(stack: &MemoryStack) -> bool {
    matches!(stack.backend, MemoryBackend::Sqlite)
}

impl MemoryStack {
    /// 条目编辑器:新模式路由到 SQLite 变更端口,旧模式退化为文件
    /// store —— `MemoryTool` 等写侧经此获得唯一权威写入链。
    pub fn editor(&self) -> Arc<dyn crate::editor::MemoryEditor> {
        match &self.sqlite {
            Some(sqlite) => sqlite.clone(),
            None => self.file.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_is_default_and_sqlite_opt_in() {
        // 不读进程 env(测试并发安全):直接验证解析函数对字符串的口径。
        match std::env::var("ZHUYE_MEMORY_BACKEND") {
            Ok(v) if v.trim().eq_ignore_ascii_case("sqlite") => {
                assert_eq!(backend_from_env(), MemoryBackend::Sqlite);
            }
            _ => assert_eq!(backend_from_env(), MemoryBackend::Markdown),
        }
    }

    #[test]
    fn stack_assembles_both_backends() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = MemoryStack {
            backend: MemoryBackend::Markdown,
            store: Arc::new(InMemoryStore::new()),
            file: Arc::new(FileMemoryStore::new(dir.path(), dir.path())),
            sqlite: None,
        };
        assert!(!is_sqlite_backend(&legacy));
        let sqlite = MemoryStack {
            backend: MemoryBackend::Sqlite,
            store: Arc::new(SqliteMemoryStore::open(dir.path(), dir.path())),
            file: Arc::new(FileMemoryStore::new(dir.path(), dir.path())),
            sqlite: Some(Arc::new(SqliteMemoryStore::open(dir.path(), dir.path()))),
        };
        assert!(is_sqlite_backend(&sqlite));
    }
}
