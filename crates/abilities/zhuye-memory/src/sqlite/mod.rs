//! SQLite 条目版本库(v1.11 M5a)—— 新记忆模式的权威存储。
//!
//! 形态对齐 [`crate::store::FileMemoryStore`]:按 `(storage root,
//! scope, agent_type)` 隔离,库文件 `memory.db` 放在旧 `MEMORY.md`
//! 同目录(Project → `{workspace}/.zhuye/agent-memory/{agent}`;
//! User → `{ZHUYE_HOME}/agent-memory/{agent}`),首次打开时幂等导入
//! 既有 MEMORY.md,此后 MEMORY.md 退化为可编辑投影。
//!
//! 并发模型:单连接 + 进程内 Mutex 串行化;跨进程靠 WAL +
//! `BEGIN IMMEDIATE` + busy timeout,revision 作乐观并发控制。
//! Session scope 与旧 FileMemoryStore 同语义:不落盘。
//!
//! topic 文件(`topics/*.md`)第一版继续走文件存储(投影的一部分,
//! 检索时与 FTS 候选分路融合),见设计边界说明。

mod change;
mod project;
mod retrieve;
mod schema;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use rusqlite::Connection;

use change::apply_changes as tx_apply_changes;
pub use change::{ChangeAction, ChangeReport, EntryKind, MemoryChange};
use project::reconcile_on_open;
use retrieve::{CANDIDATE_CAP, pack_and_render, search_scope};
use schema::{init, revision as read_revision};

use crate::consolidate::ConsolidateStats;
use crate::edit::{EntryWriteOptions, MemoryEntry};
use crate::model::{MemoryError, MemoryScope};
use crate::scope::resolve_path_with_data_dir;
use crate::store::{FileMemoryStore, MemoryStore};

/// namespace 连接表:(scope, agent) → 已初始化(含首次导入对账)
/// 的库连接。
type NamespaceMap = HashMap<(MemoryScope, String), Arc<Mutex<Connection>>>;

/// SQLite 后端条目库。构造零 IO(namespace 懒打开),错误在具体
/// 操作上报告 —— 与旧 store「打开不失败」的装配体验一致。
pub struct SqliteMemoryStore {
    workspace: PathBuf,
    user_data_dir: PathBuf,
    /// topic 投影与目录扫描继续复用文件 store(第一版边界)。
    topics: FileMemoryStore,
    /// (scope, agent) → 已初始化(含首次导入对账)的库连接。
    namespaces: RwLock<NamespaceMap>,
}

impl std::fmt::Debug for SqliteMemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteMemoryStore")
            .field("workspace", &self.workspace)
            .field("user_data_dir", &self.user_data_dir)
            .finish_non_exhaustive()
    }
}

impl SqliteMemoryStore {
    /// 以与 `FileMemoryStore::with_user_data_dir` 相同的根目录口径构造。
    pub fn open(workspace: impl Into<PathBuf>, user_data_dir: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        let user_data_dir = user_data_dir.into();
        Self {
            topics: FileMemoryStore::with_user_data_dir(&workspace, &user_data_dir),
            workspace,
            user_data_dir,
            namespaces: RwLock::new(HashMap::new()),
        }
    }

    fn memory_md_path(&self, scope: MemoryScope, agent: &str) -> Result<PathBuf, MemoryError> {
        resolve_path_with_data_dir(scope, &self.workspace, &self.user_data_dir, agent)
    }

    /// 拿到(或懒打开 + 首次对账导入)某 namespace 的连接。
    fn namespace(
        &self,
        scope: MemoryScope,
        agent: &str,
    ) -> Result<Arc<Mutex<Connection>>, MemoryError> {
        if matches!(scope, MemoryScope::Session) {
            return Err(MemoryError::SessionNotPersisted);
        }
        if let Some(ns) = self.namespaces.read().get(&(scope, agent.to_string())) {
            return Ok(ns.clone());
        }
        let mut guard = self.namespaces.write();
        if let Some(ns) = guard.get(&(scope, agent.to_string())) {
            return Ok(ns.clone());
        }
        let memory_md = self.memory_md_path(scope, agent)?;
        let db_path = memory_md
            .parent()
            .map(|dir| dir.join("memory.db"))
            .ok_or_else(|| MemoryError::Invalid("memory 路径无父目录".into()))?;
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(&db_path).map_err(schema::sql_err)?;
        schema::apply_pragmas(&conn)?;
        init(&conn)?;
        // 首次导入 / 外部编辑对账(幂等;投影 hash 一致时零动作)。
        if let Some(stats) = reconcile_on_open(&mut conn, &memory_md, scope)? {
            tracing::info!(
                scope = %scope,
                agent = %agent,
                added = stats.added,
                updated = stats.updated,
                unchanged = stats.unchanged,
                skipped_tombstoned = stats.skipped_tombstoned,
                kept_missing = stats.kept_missing,
                "memory SQLite 库完成投影对账导入"
            );
        }
        let ns = Arc::new(Mutex::new(conn));
        guard.insert((scope, agent.to_string()), ns.clone());
        Ok(ns)
    }

    /// 结构化变更入口(唯一权威写入链):单事务提交条目版本 + FTS +
    /// revision;提交成功后导出 MEMORY.md 投影(失败置 dirty,检索
    /// 仍以 DB 为准,不回滚已提交数据)。
    pub fn apply_changes(
        &self,
        scope: MemoryScope,
        agent: &str,
        changes: &[MemoryChange],
        expected_revision: Option<u64>,
    ) -> Result<ChangeReport, MemoryError> {
        let ns = self.namespace(scope, agent)?;
        let mut conn = ns.lock();
        let report = tx_apply_changes(&mut conn, changes, expected_revision)?;
        if report.revision_after > report.revision_before {
            let memory_md = self.memory_md_path(scope, agent)?;
            if let Err(e) = project::export_projection(&mut conn, &memory_md) {
                project::mark_projection_dirty(&conn);
                tracing::warn!(error = %e, "记忆投影导出失败;DB 为准,投影待补偿导出");
            }
        }
        Ok(report)
    }

    /// 当前库 revision(缓存键 / 乐观并发基准)。
    pub fn current_revision(&self, scope: MemoryScope, agent: &str) -> Result<u64, MemoryError> {
        let ns = self.namespace(scope, agent)?;
        let conn = ns.lock();
        read_revision(&conn)
    }

    /// 当前活跃条目(编辑器列表 / 投影渲染共用口径)。
    pub fn active_entries(
        &self,
        scope: MemoryScope,
        agent: &str,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        let ns = self.namespace(scope, agent)?;
        let conn = ns.lock();
        let mut stmt = conn
            .prepare(
                "SELECT title, content FROM entry_versions
                 WHERE state = 'active' AND recorded_to IS NULL
                   AND NOT EXISTS (SELECT 1 FROM tombstones t WHERE t.title = entry_versions.title)
                 ORDER BY entry_id",
            )
            .map_err(schema::sql_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(MemoryEntry {
                    scope,
                    key: row.get(0)?,
                    value: row.get(1)?,
                    whole_document: false,
                })
            })
            .map_err(schema::sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(schema::sql_err)?);
        }
        Ok(out)
    }
}

impl MemoryStore for SqliteMemoryStore {
    fn load(&self, scope: MemoryScope, agent_type: &str) -> Result<String, MemoryError> {
        if matches!(scope, MemoryScope::Session) {
            return Ok(String::new());
        }
        let ns = self.namespace(scope, agent_type)?;
        let conn = ns.lock();
        project::render_projection(&conn)
    }

    /// 整文覆盖在 sqlite 模式不可用(plan §9:旧 save 无基线,不能假装
    /// 具备 CAS 后静默覆盖)—— 返回明确的能力错误与迁移指引。
    fn save(
        &self,
        scope: MemoryScope,
        _agent_type: &str,
        _content: &str,
    ) -> Result<(), MemoryError> {
        if matches!(scope, MemoryScope::Session) {
            return Err(MemoryError::SessionNotPersisted);
        }
        Err(MemoryError::Unsupported(
            "sqlite 后端不支持整文 save;使用 write_entry / apply_changes 条目接口".into(),
        ))
    }

    fn list_agent_types(&self, scope: MemoryScope) -> Result<Vec<String>, MemoryError> {
        // 目录口径与文件 store 相同(memory.db 与 MEMORY.md 同目录,
        // 迁移后两者并存,按 MEMORY.md 扫描即可)。
        self.topics.list_agent_types(scope)
    }

    fn list_topics(
        &self,
        scope: MemoryScope,
        agent_type: &str,
    ) -> Result<Vec<String>, MemoryError> {
        self.topics.list_topics(scope, agent_type)
    }

    fn load_topic(
        &self,
        scope: MemoryScope,
        agent_type: &str,
        topic: &str,
    ) -> Result<String, MemoryError> {
        self.topics.load_topic(scope, agent_type, topic)
    }

    /// FTS5 检索路径:中文 bigram 召回 + 覆盖度门控 + 统一预算。
    /// topic 文件条目并入候选池(与旧路径同覆盖面,分路融合)。
    fn retrieve(
        &self,
        scopes: &[MemoryScope],
        agent_type: &str,
        query: &str,
        budget: usize,
    ) -> Result<String, MemoryError> {
        let now = chrono::Utc::now();
        // FTS 候选:每 scope 独立检索(库内排序)。
        let mut fts_batches: Vec<Vec<retrieve::ScoredEntry>> = Vec::new();
        for scope in scopes {
            if matches!(scope, MemoryScope::Session) {
                continue;
            }
            let ns = self.namespace(*scope, agent_type)?;
            let conn = ns.lock();
            fts_batches.push(search_scope(&conn, *scope, query, &now, CANDIDATE_CAP)?);
        }
        // topic 候选:文件侧 wiki 知识库,沿用 BM25 词法(已升级 CJK
        // bigram)—— 与 FTS 候选同构包装后交给统一装箱。
        let mut topic_entries: Vec<retrieve::ScoredEntry> = Vec::new();
        let query_terms = zhuye_bm25::tokenize(query);
        if !query_terms.is_empty() {
            for scope in scopes {
                if matches!(scope, MemoryScope::Session) {
                    continue;
                }
                for topic in self.topics.list_topics(*scope, agent_type)? {
                    let body = self.topics.load_topic(*scope, agent_type, &topic)?;
                    if body.trim().is_empty() {
                        continue;
                    }
                    for sec in crate::retrieval::split_sections(*scope, &body) {
                        let combined = format!("{} {}", sec.title, sec.body);
                        let doc_terms = zhuye_bm25::tokenize(&combined);
                        let doc_set: std::collections::HashSet<&String> =
                            doc_terms.iter().collect();
                        let matched = query_terms
                            .iter()
                            .filter(|t| {
                                doc_set.contains(t)
                                    || (retrieve::is_single_cjk_term(t)
                                        && combined.contains(t.as_str()))
                            })
                            .count();
                        if !retrieve::coverage_enough(matched, query_terms.len()) {
                            continue;
                        }
                        let coverage = matched as f64 / query_terms.len() as f64;
                        topic_entries.push(retrieve::ScoredEntry {
                            scope: *scope,
                            title: if sec.title.is_empty() {
                                format!("(topic:{topic})")
                            } else {
                                format!("(topic:{topic}) {}", sec.title)
                            },
                            content: sec.body,
                            coverage,
                            title_hit: query_terms.iter().any(|t| sec.title.contains(t.as_str())),
                            rank: 0,
                        });
                    }
                }
            }
            topic_entries.sort_by(|a, b| {
                b.coverage
                    .partial_cmp(&a.coverage)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(b.title_hit.cmp(&a.title_hit))
                    .then(a.title.cmp(&b.title))
            });
            for (idx, entry) in topic_entries.iter_mut().enumerate() {
                entry.rank = idx;
            }
        }
        if !topic_entries.is_empty() {
            fts_batches.push(topic_entries);
        }
        Ok(pack_and_render(fts_batches, scopes, budget))
    }

    fn revision(&self, scope: MemoryScope, agent_type: &str) -> Result<u64, MemoryError> {
        if matches!(scope, MemoryScope::Session) {
            return Ok(0);
        }
        self.current_revision(scope, agent_type)
    }
}

impl crate::editor::MemoryEditor for SqliteMemoryStore {
    fn list_entries(
        &self,
        scope: MemoryScope,
        agent_type: &str,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        if matches!(scope, MemoryScope::Session) {
            return Err(MemoryError::SessionNotPersisted);
        }
        self.active_entries(scope, agent_type)
    }

    /// 条目写入:同标题活跃条目存在 → UPDATE(新版本);不存在 →
    /// ADD。`create_only` / `expected_value` 语义与旧文件路径一致。
    /// 命中墓碑时**越过墓碑重建**(工具写入经 Prompt 审批 = 显式授权,
    /// change 端口留痕 source)。
    fn write_entry(
        &self,
        scope: MemoryScope,
        agent_type: &str,
        key: &str,
        value: &str,
        opts: &EntryWriteOptions,
    ) -> Result<(), MemoryError> {
        crate::edit::validate_key(key)?;
        if value.lines().any(|line| line.starts_with("## ")) {
            return Err(MemoryError::HeadingInValue);
        }
        let ns = self.namespace(scope, agent_type)?;
        let mut conn = ns.lock();
        let current = change::active_by_title(&conn, key)?;
        // 先按旧路径同款语义构造变更(乐观锁 / create_only 校验都在
        // 提交前完成),再一次提交 + 导出投影。
        let ch = if opts.replace_document {
            // 整份替换只保留给「导入产生的 (all) 单块文档」这一种形态;
            // 结构化条目文档要求逐条写(明确错误 + 迁移指引)。
            if key != crate::edit::WHOLE_DOCUMENT_KEY {
                return Err(MemoryError::Invalid(format!(
                    "key `{}` is reserved for whole-document replacement",
                    crate::edit::WHOLE_DOCUMENT_KEY
                )));
            }
            match current {
                Some(cur) => {
                    check_expected(&cur.content, opts)?;
                    let mut ch = MemoryChange::add(key, value, "tool:replace-document");
                    ch.action = ChangeAction::Update;
                    ch
                }
                None => {
                    return Err(MemoryError::Unsupported(
                        "结构化文档不支持整份替换;逐条 write 条目".into(),
                    ));
                }
            }
        } else {
            match current {
                Some(cur) => {
                    if opts.create_only {
                        return Err(MemoryError::EntryExists {
                            key: key.to_string(),
                        });
                    }
                    check_expected(&cur.content, opts)?;
                    let mut ch = MemoryChange::add(key, value, "tool");
                    ch.action = ChangeAction::Update;
                    ch
                }
                None => {
                    if opts.expected_value.is_some() {
                        // 期望基线给了但条目不存在:与旧路径同判(失配)。
                        return Err(MemoryError::DocumentChanged);
                    }
                    let mut ch = MemoryChange::add(key, value, "tool");
                    ch.revive = true; // Prompt 审批过的显式写入可越过墓碑
                    ch
                }
            }
        };
        tx_apply_changes(&mut conn, std::slice::from_ref(&ch), None)
            .map_err(map_unique_violation)?;
        let memory_md = self.memory_md_path(scope, agent_type)?;
        if let Err(e) = project::export_projection(&mut conn, &memory_md) {
            project::mark_projection_dirty(&conn);
            tracing::warn!(error = %e, "记忆投影导出失败(写入后)");
        }
        Ok(())
    }

    /// 条目删除:软失效 + 墓碑;不存在幂等成功(与旧路径一致)。
    fn delete_entry(
        &self,
        scope: MemoryScope,
        agent_type: &str,
        key: &str,
        opts: &EntryWriteOptions,
    ) -> Result<(), MemoryError> {
        crate::edit::validate_key(key)?;
        if !opts.replace_document && key == crate::edit::WHOLE_DOCUMENT_KEY {
            return Err(MemoryError::Invalid(format!(
                "key `{}` is reserved for whole-document removal; pass replace_document = true",
                crate::edit::WHOLE_DOCUMENT_KEY
            )));
        }
        let ns = self.namespace(scope, agent_type)?;
        let mut conn = ns.lock();
        let current = change::active_by_title(&conn, key)?;
        match current {
            Some(cur) => {
                check_expected(&cur.content, opts)?;
                let mut ch = MemoryChange::add(key, String::new(), "tool:delete");
                ch.action = ChangeAction::Delete;
                let report = tx_apply_changes(&mut conn, std::slice::from_ref(&ch), None)?;
                let _ = report;
                let memory_md = self.memory_md_path(scope, agent_type)?;
                if let Err(e) = project::export_projection(&mut conn, &memory_md) {
                    project::mark_projection_dirty(&conn);
                    tracing::warn!(error = %e, "记忆投影导出失败(删除后)");
                }
                Ok(())
            }
            None => {
                if opts.expected_value.is_some() {
                    return Err(MemoryError::DocumentChanged);
                }
                Ok(()) // 幂等删除
            }
        }
    }

    /// v2 模式下条目在写入路径已按标题唯一、导入时去重 —— 确定性
    /// 整理无事可做,如实报告零合并(语义合并属 M5b 决策端口)。
    fn consolidate_entries(
        &self,
        scope: MemoryScope,
        agent_type: &str,
    ) -> Result<Option<ConsolidateStats>, MemoryError> {
        if matches!(scope, MemoryScope::Session) {
            return Err(MemoryError::SessionNotPersisted);
        }
        let count = self.active_entries(scope, agent_type)?.len();
        Ok(Some(ConsolidateStats {
            merged: 0,
            dropped_empty: 0,
            sections_before: count,
            sections_after: count,
        }))
    }
}

/// expected_value 乐观锁校验(条目当前正文必须与期望一致)。
fn check_expected(current: &str, opts: &EntryWriteOptions) -> Result<(), MemoryError> {
    match opts.expected_value.as_deref() {
        Some(expected) if expected != current => Err(MemoryError::DocumentChanged),
        _ => Ok(()),
    }
}

/// SQLite 唯一索引冲突 → 语义化错误(并发下两个写入者同时 ADD 同名
/// 条目时,后提交者收到 EntryExists 而非裸 SQLite 错误)。
fn map_unique_violation(e: MemoryError) -> MemoryError {
    match &e {
        MemoryError::Sqlite(msg) if msg.contains("UNIQUE constraint failed") => {
            MemoryError::EntryExists { key: String::new() }
        }
        _ => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::MemoryEditor;

    fn store(dir: &std::path::Path) -> SqliteMemoryStore {
        SqliteMemoryStore::open(dir.to_path_buf(), dir.to_path_buf())
    }

    #[test]
    fn roundtrip_write_list_load_projection() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.write_entry(
            MemoryScope::Project,
            "agent",
            "数据库",
            "项目使用 postgres 16",
            &EntryWriteOptions::default(),
        )
        .unwrap();
        let entries = s.list_entries(MemoryScope::Project, "agent").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "数据库");
        // 投影落盘:MEMORY.md 上是可读投影。
        let md = dir.path().join(".zhuye/agent-memory/agent/MEMORY.md");
        let body = std::fs::read_to_string(&md).unwrap();
        assert_eq!(body, "## 数据库\n项目使用 postgres 16\n");
        // MemoryStore::load 返回投影。
        assert_eq!(s.load(MemoryScope::Project, "agent").unwrap(), body);
    }

    #[test]
    fn update_creates_version_and_projection_follows() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.write_entry(
            MemoryScope::Project,
            "agent",
            "部署",
            "github actions",
            &EntryWriteOptions::default(),
        )
        .unwrap();
        s.write_entry(
            MemoryScope::Project,
            "agent",
            "部署",
            "github actions + 签名",
            &EntryWriteOptions::default(),
        )
        .unwrap();
        let entries = s.list_entries(MemoryScope::Project, "agent").unwrap();
        assert_eq!(entries.len(), 1, "同名条目仍是单活跃");
        assert!(entries[0].value.contains("签名"));
        let md = dir.path().join(".zhuye/agent-memory/agent/MEMORY.md");
        assert!(std::fs::read_to_string(&md).unwrap().contains("签名"));
        // revision 随提交推进(缓存键可用)。
        assert!(s.current_revision(MemoryScope::Project, "agent").unwrap() >= 2);
    }

    #[test]
    fn delete_then_write_revives_with_prompt_authorization() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.write_entry(
            MemoryScope::Project,
            "agent",
            "部署",
            "actions",
            &EntryWriteOptions::default(),
        )
        .unwrap();
        s.delete_entry(
            MemoryScope::Project,
            "agent",
            "部署",
            &EntryWriteOptions::default(),
        )
        .unwrap();
        // 重复删除幂等。
        s.delete_entry(
            MemoryScope::Project,
            "agent",
            "部署",
            &EntryWriteOptions::default(),
        )
        .unwrap();
        // 工具路径(显式授权)重写同名 → 复活成功。
        s.write_entry(
            MemoryScope::Project,
            "agent",
            "部署",
            "actions v2",
            &EntryWriteOptions::default(),
        )
        .unwrap();
        assert!(
            s.list_entries(MemoryScope::Project, "agent")
                .unwrap()
                .iter()
                .any(|e| e.key == "部署")
        );
    }

    #[test]
    fn expected_value_and_create_only_semantics_kept() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.write_entry(
            MemoryScope::Project,
            "agent",
            "A",
            "v1",
            &EntryWriteOptions::default(),
        )
        .unwrap();
        let err = s
            .write_entry(
                MemoryScope::Project,
                "agent",
                "A",
                "v2",
                &EntryWriteOptions {
                    create_only: true,
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, MemoryError::EntryExists { .. }));
        let err = s
            .write_entry(
                MemoryScope::Project,
                "agent",
                "A",
                "v3",
                &EntryWriteOptions {
                    expected_value: Some("过期基线".into()),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, MemoryError::DocumentChanged));
        // 正确基线 → 更新成功。
        s.write_entry(
            MemoryScope::Project,
            "agent",
            "A",
            "v3",
            &EntryWriteOptions {
                expected_value: Some("v1".into()),
                ..Default::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn whole_document_save_rejected_with_migration_hint() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let err = s
            .save(MemoryScope::Project, "agent", "整文覆盖")
            .unwrap_err();
        assert!(matches!(err, MemoryError::Unsupported(_)));
    }

    #[test]
    fn scopes_are_isolated_by_directory() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let s = SqliteMemoryStore::open(dir.path(), home.path());
        s.write_entry(
            MemoryScope::Project,
            "a",
            "同名",
            "项目值",
            &EntryWriteOptions::default(),
        )
        .unwrap();
        s.write_entry(
            MemoryScope::User,
            "a",
            "同名",
            "用户值",
            &EntryWriteOptions::default(),
        )
        .unwrap();
        assert_eq!(
            s.load(MemoryScope::Project, "a").unwrap(),
            "## 同名\n项目值\n"
        );
        assert_eq!(s.load(MemoryScope::User, "a").unwrap(), "## 同名\n用户值\n");
        // agent 维度同样隔离。
        s.write_entry(
            MemoryScope::Project,
            "b",
            "同名",
            "另一 agent",
            &EntryWriteOptions::default(),
        )
        .unwrap();
        assert!(
            s.load(MemoryScope::Project, "a")
                .unwrap()
                .contains("项目值")
        );
    }

    #[test]
    fn chinese_retrieve_via_trait_default_routing() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.write_entry(
            MemoryScope::Project,
            "agent",
            "数据库",
            "项目使用 postgres 16,连接池 pgbouncer",
            &EntryWriteOptions::default(),
        )
        .unwrap();
        s.write_entry(
            MemoryScope::Project,
            "agent",
            "代码风格",
            "提交前必须跑 cargo fmt",
            &EntryWriteOptions::default(),
        )
        .unwrap();
        let dyn_store: Arc<dyn MemoryStore> = Arc::new(store(dir.path()));
        // 复用同一库文件:namespace 以 (scope,agent) 为键,两个实例
        // 打开同一 db —— 验证跨实例可见(WAL + 重新打开)。
        let out = dyn_store
            .retrieve(&[MemoryScope::Project], "agent", "数据库连接池", 8000)
            .unwrap();
        assert!(
            out.contains("postgres 16"),
            "中文查询经 trait 路由命中:{out}"
        );
        let gated = dyn_store
            .retrieve(
                &[MemoryScope::Project],
                "agent",
                "无关 查询 审批 流水线",
                8000,
            )
            .unwrap();
        assert_eq!(gated, "", "低覆盖查询不注入");
    }
}
