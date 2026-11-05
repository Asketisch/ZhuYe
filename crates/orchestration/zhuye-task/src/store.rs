//! `TaskStore` trait + `InMemoryTaskStore` + `FileTaskStore`。
//!
//! 持久化后端抽象,与 `zhuye-memory::MemoryStore` 同构(一个 trait +
//! 一个 in-memory + 一个 file-backed 实现)。内置后端按列表串行化
//! 事务:内存后端在共享实例内使用异步锁,文件后端使用跨进程文件锁。
//!
//! 路径布局:
//! ```text
//! $ZHUYE_HOME/tasks/<list>/    # list 目录
//!   .highwatermark   # 整数文本,记录该 list 历史最大 task id
//!   1.json           # 各任务文件(id 命名)
//!   2.json
//!   ...
//! ```
//!
//! ## 写入语义
//!
//! - 写文件用 `tmp + rename` 原子替换,避免半写状态。
//! - `next_id` 在事务锁内校准高水位与已有任务 ID,再分配下一个 ID;
//!   文件读写由后台阻塞任务执行,调用取消时锁保持至写入结束。
//! - `list` 读 list 目录下所有 `*.json`,过滤 `.highwatermark` 与 `.lock`,
//!   按 id 升序返回(失败的文件跳过 + `tracing::warn`)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use tokio::sync::Mutex;
use tracing::warn;

use crate::error::TaskError;
use crate::model::{ListId, Task, TaskId, TaskStatus};

/// 任务存储后端的抽象接口。`Send + Sync` —— 多线程共享。
///
/// `next_id` 必须在每次创建前调用,确保 id 单调递增。`save` 是 upsert
/// 语义(覆盖已存在的 id);`delete` 真正删文件(区别于把 status 改为
/// `Deleted` 的软删除);`list` 默认按 id 升序返回,自动排除软删任务。
#[async_trait]
pub trait TaskStore: Send + Sync {
    /// 为跨管理器的读改写获取列表事务锁;自定义后端可按需实现。
    /// 持锁期间必须使用下列 `*_transactional` 方法并传入锁句柄,
    /// 避免直接调用会再次取锁的写方法;后台写入必须持有句柄直到结束。
    async fn lock_transaction(
        &self,
        _list: &ListId,
    ) -> Result<Option<Arc<dyn Send + Sync>>, TaskError> {
        Ok(None)
    }

    /// 保持事务锁到异步写入完成;自定义后端可覆盖以处理取消。
    async fn save_transactional(
        &self,
        list: &ListId,
        task: &Task,
        _guard: Option<Arc<dyn Send + Sync>>,
    ) -> Result<(), TaskError> {
        self.save(list, task).await
    }

    /// 保持事务锁到 ID 分配完成。
    async fn next_id_transactional(
        &self,
        list: &ListId,
        _guard: Option<Arc<dyn Send + Sync>>,
    ) -> Result<TaskId, TaskError> {
        self.next_id(list).await
    }

    /// 保持事务锁到删除完成。
    async fn delete_transactional(
        &self,
        list: &ListId,
        id: TaskId,
        _guard: Option<Arc<dyn Send + Sync>>,
    ) -> Result<(), TaskError> {
        self.delete(list, id).await
    }

    /// `list` 目录的绝对路径(用于 `output_path` 派生)。
    fn dir_for(&self, list: &ListId) -> PathBuf;

    /// 保存任务(`tmp + rename` 原子写)。
    async fn save(&self, list: &ListId, task: &Task) -> Result<(), TaskError>;

    /// 读取单个任务;不存在返回 `TaskError::NotFound`。
    async fn load(&self, list: &ListId, id: TaskId) -> Result<Task, TaskError>;

    /// 物理删除任务文件。
    async fn delete(&self, list: &ListId, id: TaskId) -> Result<(), TaskError>;

    /// 列出 list 下所有任务,按 id 升序。已软删(`status == Deleted`)的
    /// 默认排除 —— 由 `TaskManager` 在 `include_deleted=true` 时再包一层过滤。
    async fn list(&self, list: &ListId) -> Result<Vec<Task>, TaskError>;

    /// 分配下一个 task id(原子 +1)。每次调用都 +1 并持久化高水位。
    async fn next_id(&self, list: &ListId) -> Result<TaskId, TaskError>;
}

// ── InMemoryTaskStore(内存任务存储)──────────────────────────

/// 内存后端。热路径(同 session 短期)使用,跨进程不持久化。
///
/// 内部用 `parking_lot::RwLock<HashMap<(ListId, TaskId), Task>>` 容纳数据;
/// `next_id` 用第二个 `RwLock<HashMap<ListId, TaskId>>` 维护高水位。
/// 操作在 `async fn` 内取 sync lock,锁粒度很小(几纳秒),不影响并发。
pub struct InMemoryTaskStore {
    map: RwLock<HashMap<(ListId, TaskId), Task>>,
    hwm: RwLock<HashMap<ListId, TaskId>>,
    transaction_locks: Mutex<HashMap<ListId, Arc<Mutex<()>>>>,
}

impl std::fmt::Debug for InMemoryTaskStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let map = self.map.read();
        let hwm = self.hwm.read();
        f.debug_struct("InMemoryTaskStore")
            .field("task_count", &map.len())
            .field("list_count", &hwm.len())
            .finish()
    }
}

impl Default for InMemoryTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryTaskStore {
    /// 新建空 store。
    pub fn new() -> Self {
        Self {
            map: RwLock::new(HashMap::new()),
            hwm: RwLock::new(HashMap::new()),
            transaction_locks: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl TaskStore for InMemoryTaskStore {
    async fn lock_transaction(
        &self,
        list: &ListId,
    ) -> Result<Option<Arc<dyn Send + Sync>>, TaskError> {
        let lock = {
            let mut locks = self.transaction_locks.lock().await;
            locks
                .entry(list.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        Ok(Some(Arc::new(lock.lock_owned().await)))
    }

    fn dir_for(&self, list: &ListId) -> PathBuf {
        PathBuf::from(format!("<in-memory>/{list}"))
    }

    async fn save(&self, list: &ListId, task: &Task) -> Result<(), TaskError> {
        let guard = self.lock_transaction(list).await?;
        self.save_transactional(list, task, guard).await
    }

    async fn save_transactional(
        &self,
        list: &ListId,
        task: &Task,
        _guard: Option<Arc<dyn Send + Sync>>,
    ) -> Result<(), TaskError> {
        if task.list_id != *list {
            return Err(TaskError::Invalid("任务列表不匹配".into()));
        }
        self.map
            .write()
            .insert((task.list_id.clone(), task.id), task.clone());
        Ok(())
    }

    async fn load(&self, list: &ListId, id: TaskId) -> Result<Task, TaskError> {
        self.map
            .read()
            .get(&(list.clone(), id))
            .cloned()
            .ok_or_else(|| TaskError::NotFound {
                list: list.clone(),
                id,
            })
    }

    async fn delete(&self, list: &ListId, id: TaskId) -> Result<(), TaskError> {
        let guard = self.lock_transaction(list).await?;
        self.delete_transactional(list, id, guard).await
    }

    async fn delete_transactional(
        &self,
        list: &ListId,
        id: TaskId,
        _guard: Option<Arc<dyn Send + Sync>>,
    ) -> Result<(), TaskError> {
        self.map.write().remove(&(list.clone(), id));
        Ok(())
    }

    async fn list(&self, list: &ListId) -> Result<Vec<Task>, TaskError> {
        let mut out: Vec<Task> = self
            .map
            .read()
            .values()
            .filter(|t| t.list_id == *list && t.status != TaskStatus::Deleted)
            .cloned()
            .collect();
        out.sort_by_key(|t| t.id);
        Ok(out)
    }

    async fn next_id(&self, list: &ListId) -> Result<TaskId, TaskError> {
        let guard = self.lock_transaction(list).await?;
        self.next_id_transactional(list, guard).await
    }

    async fn next_id_transactional(
        &self,
        list: &ListId,
        _guard: Option<Arc<dyn Send + Sync>>,
    ) -> Result<TaskId, TaskError> {
        let mut hwm = self.hwm.write();
        let cur = hwm.get(list).copied().unwrap_or(0);
        let next = cur
            .checked_add(1)
            .ok_or_else(|| TaskError::Invalid("task id overflow".into()))?;
        hwm.insert(list.clone(), next);
        Ok(next)
    }
}

// ── FileTaskStore(文件任务存储)────────────────────────────────

/// 文件后端。冷路径(重启 / 跨 session)使用,持久化到磁盘。
///
/// 路径:`<home>/tasks/<list>/<id>.json`(`<home>` 来自 `ZHUYE_HOME`
/// 或 `$HOME/.zhuye`)。写入使用列表文件锁,高水位写回
/// `.highwatermark`,支持跨进程安全分配。
pub struct FileTaskStore {
    home: PathBuf,
    /// 每 list 一把 mutex,串行化 `next_id` 与 `save` 操作,避免
    /// 两个并发调用读到同一个高水位。
    list_locks: Mutex<HashMap<ListId, Arc<Mutex<()>>>>,
}

impl std::fmt::Debug for FileTaskStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileTaskStore")
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

impl FileTaskStore {
    /// 用显式 `home` 构造(测试友好)。
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            list_locks: Mutex::new(HashMap::new()),
        }
    }

    /// 用默认数据目录构造(`$ZHUYE_HOME` > `$HOME/.zhuye`)。
    ///
    /// home 口径统一走 [`zhuye_config::data_dir`](zhuye_config::data_dir) ——
    /// 与 config / notes / prompt 落同一个根;空串 `ZHUYE_HOME` 按未设置
    /// 处理(此前会解析出空路径)。
    pub fn with_default_home() -> Result<Self, TaskError> {
        let home = zhuye_config::data_dir()
            .ok_or_else(|| TaskError::Invalid("ZHUYE_HOME / HOME not set".into()))?;
        Ok(Self::new(home))
    }

    /// list-level 锁:同 list 的 `next_id` + `save` 串行执行。
    async fn lock_for(&self, list: &ListId) -> Arc<Mutex<()>> {
        let mut map = self.list_locks.lock().await;
        map.entry(list.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

// 列表标识必须是单个路径组件,拒绝绝对路径及多层路径。
pub(crate) fn validate_list_id(list: &str) -> Result<(), TaskError> {
    let path = Path::new(list);
    let mut parts = path.components();
    if list.contains(['/', '\\'])
        || !matches!(parts.next(), Some(std::path::Component::Normal(_)))
        || parts.next().is_some()
    {
        return Err(TaskError::Invalid(format!(
            "invalid task list id: {list:?}"
        )));
    }
    // 白名单字符集 + 长度上限(来自 main 分支安全修复):`list` 会成为
    // 文件路径的一段,白名单比纯路径组件黑名单更严 —— 连空格、点号等
    // 怪异文件名字符一并拒绝,杜绝注入类字符进入文件系统。
    if list.is_empty() || list.len() > 128 {
        return Err(TaskError::Invalid(
            "task list id length must be 1..=128 chars".into(),
        ));
    }
    if !list
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(TaskError::Invalid(
            "task list id must match [a-z0-9_-]+".into(),
        ));
    }
    Ok(())
}

#[async_trait]
impl TaskStore for FileTaskStore {
    async fn lock_transaction(
        &self,
        list: &ListId,
    ) -> Result<Option<Arc<dyn Send + Sync>>, TaskError> {
        validate_list_id(list)?;
        let dir = self.dir_for(list);
        let file = tokio::task::spawn_blocking(move || -> Result<std::fs::File, TaskError> {
            std::fs::create_dir_all(&dir)?;
            Ok(std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(".task-transaction.lock"))?)
        })
        .await
        .map_err(|error| TaskError::Invalid(format!("join error: {error}")))??;
        loop {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(Some(Arc::new(file))),
                Err(error)
                    if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
                {
                    // 等待锁时不占用阻塞线程池,避免写入者因线程耗尽而无法完成。
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn dir_for(&self, list: &ListId) -> PathBuf {
        self.home.join("tasks").join(list)
    }

    async fn save(&self, list: &ListId, task: &Task) -> Result<(), TaskError> {
        let guard = self.lock_transaction(list).await?;
        self.save_transactional(list, task, guard).await
    }

    async fn save_transactional(
        &self,
        list: &ListId,
        task: &Task,
        guard: Option<Arc<dyn Send + Sync>>,
    ) -> Result<(), TaskError> {
        validate_list_id(list)?;
        if task.list_id != *list {
            return Err(TaskError::Invalid("任务列表不匹配".into()));
        }
        let lock = self.lock_for(list).await;
        let _guard = lock.lock().await;
        let dir = self.dir_for(list);
        let task = task.clone();
        run_guarded_blocking(guard, move || {
            let current = read_hwm_blocking(&dir)?;
            if task.id > current {
                write_hwm_blocking(&dir, task.id)?;
            }
            write_task_blocking(&dir, &task)
        })
        .await
    }

    async fn load(&self, list: &ListId, id: TaskId) -> Result<Task, TaskError> {
        validate_list_id(list)?;
        let p = self.dir_for(list).join(format!("{id}.json"));
        let s = tokio::fs::read_to_string(&p)
            .await
            .map_err(|_| TaskError::NotFound {
                list: list.clone(),
                id,
            })?;
        let t: Task = serde_json::from_str(&s)?;
        Ok(t)
    }

    async fn delete(&self, list: &ListId, id: TaskId) -> Result<(), TaskError> {
        let guard = self.lock_transaction(list).await?;
        self.delete_transactional(list, id, guard).await
    }

    async fn delete_transactional(
        &self,
        list: &ListId,
        id: TaskId,
        guard: Option<Arc<dyn Send + Sync>>,
    ) -> Result<(), TaskError> {
        validate_list_id(list)?;
        let path = self.dir_for(list).join(format!("{id}.json"));
        run_guarded_blocking(guard, move || match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(TaskError::Io(error)),
        })
        .await
    }

    async fn list(&self, list: &ListId) -> Result<Vec<Task>, TaskError> {
        validate_list_id(list)?;
        let dir = self.dir_for(list);
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(TaskError::Io(e)),
        };
        let mut out = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(".json") || name.starts_with('.') {
                continue;
            }
            match tokio::fs::read_to_string(&path).await {
                Ok(s) => match serde_json::from_str::<Task>(&s) {
                    Ok(t) if t.status != TaskStatus::Deleted => out.push(t),
                    Ok(_) => {} // skip soft-deleted
                    Err(e) => warn!(?path, error = %e, "skip malformed task json"),
                },
                Err(e) => warn!(?path, error = %e, "skip unreadable task json"),
            }
        }
        out.sort_by_key(|t| t.id);
        Ok(out)
    }

    async fn next_id(&self, list: &ListId) -> Result<TaskId, TaskError> {
        let guard = self.lock_transaction(list).await?;
        self.next_id_transactional(list, guard).await
    }

    async fn next_id_transactional(
        &self,
        list: &ListId,
        guard: Option<Arc<dyn Send + Sync>>,
    ) -> Result<TaskId, TaskError> {
        validate_list_id(list)?;
        let lock = self.lock_for(list).await;
        let _guard = lock.lock().await;
        let dir = self.dir_for(list);
        run_guarded_blocking(guard, move || {
            let next = read_hwm_blocking(&dir)?
                .max(max_task_id_blocking(&dir)?)
                .checked_add(1)
                .ok_or_else(|| TaskError::Invalid("task id overflow".into()))?;
            write_hwm_blocking(&dir, next)?;
            Ok(next)
        })
        .await
    }
}

// 阻塞任务持有事务锁,即使等待它的异步调用被取消也不提前解锁。
async fn run_guarded_blocking<T: Send + 'static>(
    guard: Option<Arc<dyn Send + Sync>>,
    operation: impl FnOnce() -> Result<T, TaskError> + Send + 'static,
) -> Result<T, TaskError> {
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        operation()
    })
    .await
    .map_err(|error| TaskError::Invalid(format!("join error: {error}")))?
}

fn read_hwm_blocking(dir: &Path) -> Result<TaskId, TaskError> {
    match std::fs::read_to_string(dir.join(".highwatermark")) {
        Ok(value) => value
            .trim()
            .parse()
            .map_err(|_| TaskError::Invalid("任务高水位文件损坏".into())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => max_task_id_blocking(dir),
        Err(error) => Err(error.into()),
    }
}

// 校准落后或缺失的高水位,不能覆盖目录中已有的任务。
fn max_task_id_blocking(dir: &Path) -> Result<TaskId, TaskError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let mut maximum = 0;
    for entry in entries {
        let name = entry?.file_name();
        if let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_suffix(".json"))
            .and_then(|name| name.parse::<TaskId>().ok())
        {
            maximum = maximum.max(id);
        }
    }
    Ok(maximum)
}

fn write_task_blocking(dir: &Path, task: &Task) -> Result<(), TaskError> {
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_string_pretty(task)?;
    let tmp = dir.join(format!("{}.json.tmp", task.id));
    let final_p = dir.join(format!("{}.json", task.id));
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, final_p)?;
    Ok(())
}

fn write_hwm_blocking(dir: &Path, id: TaskId) -> Result<(), TaskError> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".highwatermark.{id}.tmp"));
    let final_p = dir.join(".highwatermark");
    std::fs::write(&tmp, id.to_string())?;
    std::fs::rename(&tmp, final_p)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn task(id: TaskId, list: &str, subject: &str) -> Task {
        let now = std::time::SystemTime::UNIX_EPOCH;
        Task {
            id,
            list_id: list.into(),
            subject: subject.into(),
            description: format!("desc for {subject}"),
            active_form: None,
            owner: None,
            status: TaskStatus::Pending,
            blocks: vec![],
            blocked_by: vec![],
            metadata: serde_json::json!({}),
            output_path: None,
            claimed_by: None,
            claimed_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_keeps_file_transaction_until_blocking_write_finishes() {
        let dir = TempDir::new().unwrap();
        let first = FileTaskStore::new(dir.path());
        let second = FileTaskStore::new(dir.path());
        let list = "cancel-test".to_string();
        let guard = first.lock_transaction(&list).await.unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let operation = tokio::spawn(run_guarded_blocking(guard, move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        }));
        started_rx.await.unwrap();
        operation.abort();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                second.lock_transaction(&list)
            )
            .await
            .is_err()
        );
        release_tx.send(()).unwrap();
        let acquired = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            second.lock_transaction(&list),
        )
        .await
        .expect("后台写入结束后应释放事务锁")
        .unwrap();
        assert!(acquired.is_some());
    }

    #[tokio::test]
    async fn in_memory_save_load_roundtrip() {
        let s = InMemoryTaskStore::new();
        s.save(&"L".into(), &task(1, "L", "a")).await.unwrap();
        let t = s.load(&"L".into(), 1).await.unwrap();
        assert_eq!(t.subject, "a");
    }

    #[tokio::test]
    async fn in_memory_load_missing_errors() {
        let s = InMemoryTaskStore::new();
        let err = s.load(&"L".into(), 99).await.unwrap_err();
        assert!(matches!(err, TaskError::NotFound { .. }));
    }

    #[tokio::test]
    async fn in_memory_list_filters_deleted_and_sorts() {
        let s = InMemoryTaskStore::new();
        let mut t3 = task(3, "L", "c");
        t3.status = TaskStatus::Deleted;
        s.save(&"L".into(), &task(1, "L", "a")).await.unwrap();
        s.save(&"L".into(), &task(2, "L", "b")).await.unwrap();
        s.save(&"L".into(), &t3).await.unwrap();
        let v = s.list(&"L".into()).await.unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].id, 1);
        assert_eq!(v[1].id, 2);
    }

    #[tokio::test]
    async fn in_memory_next_id_increments_per_list() {
        let s = InMemoryTaskStore::new();
        assert_eq!(s.next_id(&"L1".into()).await.unwrap(), 1);
        assert_eq!(s.next_id(&"L1".into()).await.unwrap(), 2);
        assert_eq!(s.next_id(&"L2".into()).await.unwrap(), 1);
        assert_eq!(s.next_id(&"L1".into()).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn file_store_save_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let s = FileTaskStore::new(dir.path());
        s.save(&"L".into(), &task(1, "L", "alpha")).await.unwrap();
        let t = s.load(&"L".into(), 1).await.unwrap();
        assert_eq!(t.subject, "alpha");
    }

    #[tokio::test]
    async fn file_store_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let s = FileTaskStore::new(dir.path());
        s.save(&"L".into(), &task(1, "L", "x")).await.unwrap();
        assert!(dir.path().join("tasks/L/1.json").exists());
    }

    #[tokio::test]
    async fn file_store_load_missing_errors() {
        let dir = TempDir::new().unwrap();
        let s = FileTaskStore::new(dir.path());
        let err = s.load(&"L".into(), 99).await.unwrap_err();
        assert!(matches!(err, TaskError::NotFound { .. }));
    }

    #[tokio::test]
    async fn file_store_delete_removes_file() {
        let dir = TempDir::new().unwrap();
        let s = FileTaskStore::new(dir.path());
        s.save(&"L".into(), &task(1, "L", "x")).await.unwrap();
        s.delete(&"L".into(), 1).await.unwrap();
        let err = s.load(&"L".into(), 1).await.unwrap_err();
        assert!(matches!(err, TaskError::NotFound { .. }));
    }

    #[tokio::test]
    async fn file_store_list_sorts_and_filters() {
        let dir = TempDir::new().unwrap();
        let s = FileTaskStore::new(dir.path());
        s.save(&"L".into(), &task(2, "L", "b")).await.unwrap();
        s.save(&"L".into(), &task(1, "L", "a")).await.unwrap();
        let mut t3 = task(3, "L", "c");
        t3.status = TaskStatus::Deleted;
        s.save(&"L".into(), &t3).await.unwrap();
        let v = s.list(&"L".into()).await.unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].id, 1);
        assert_eq!(v[1].id, 2);
    }

    #[tokio::test]
    async fn file_store_next_id_persists_across_stores() {
        let dir = TempDir::new().unwrap();
        {
            let s = FileTaskStore::new(dir.path());
            assert_eq!(s.next_id(&"L".into()).await.unwrap(), 1);
            assert_eq!(s.next_id(&"L".into()).await.unwrap(), 2);
        }
        // 新建 store 读同一目录,高水位应恢复为 2。
        let s2 = FileTaskStore::new(dir.path());
        assert_eq!(s2.next_id(&"L".into()).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn file_store_lists_isolated_per_list() {
        let dir = TempDir::new().unwrap();
        let s = FileTaskStore::new(dir.path());
        s.save(&"L1".into(), &task(1, "L1", "a")).await.unwrap();
        s.save(&"L2".into(), &task(1, "L2", "x")).await.unwrap();
        assert_eq!(s.next_id(&"L1".into()).await.unwrap(), 2);
        assert_eq!(s.next_id(&"L2".into()).await.unwrap(), 2);
    }
}
