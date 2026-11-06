//! `queue` — v1.10 功能2:队列模式(session 级 FIFO 任务队列)。
//!
//! 与 `cron`(定时触发)互补:队列模式把一组 prompt **排队**,由后台
//! driver **串行**消费 —— 每次只出队一条,作为 `Submission::user_input`
//! 注入 agent loop,等它的 turn 以 `TurnComplete` 结束(事件驱动)再把
//! 下一条出队。适合「攒一批任务让 agent 逐个跑」的场景。
//!
//! 设计(镜像 `cron.rs`):
//! - `state`(`Arc<RwLock<QueueState>>`)共享:driver task 与 `QueueTool`
//!   调用方都通过它读写;
//! - `sub_tx` 注入 agent loop 的 submission sender;`None` = 仅记录、
//!   不真正触发(测试 / 未接 loop 时);
//! - `session_id` 决定持久化文件(`~/.zhuye/queue/<session>.json`),
//!   原子落盘(经 `zhuye-fs`),重启后未完成任务回到 Queued;
//! - 完成信号走引擎 **Stop hook**(注册方 `zhuye-exec::QueueTurnEndHook`
//!   在 check_stop 节点触发时调 `settle_active` + 立即 `tick`)——
//!   `TurnComplete` 事件只发 per-submission 通道,session 订阅者看不到;
//!   `QueueScheduler::start(tick_secs)` 的周期 driver 只作兜底。注意:
//!   active 任务执行期间用户手动插入的输入若先结束,也会先结清 active
//!   任务(best-effort 语义,v1 不做 turn 归因)。

use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use zhuye_protocol::{Submission, ThreadId};

// ── 任务模型 ─────────────────────────────────────────────────────────

/// 一条队列任务(可序列化,落盘用)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueTask {
    /// 稳定 id(uuid)。remove 用它寻址。
    pub id: String,
    /// 出队执行时作为 `Submission::user_input` 注入的 prompt。
    pub prompt: String,
    /// 可选任务名(给人看)。
    #[serde(default)]
    pub name: Option<String>,
    pub status: QueueTaskStatus,
    /// 入队时间(UTC)。
    pub created_at: DateTime<Utc>,
    /// 出队(开始执行)时间。`None` = 还在排队。
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    /// 结束时间。`None` = 未结束(Queued / Running)。
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    /// 结束备注(失败原因 / turn 状态等)。
    #[serde(default)]
    pub note: Option<String>,
}

/// 任务生命周期。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueTaskStatus {
    /// 排队中(等待出队)。
    Queued,
    /// 执行中(已注入 agent loop,等 TurnComplete)。
    Running,
    /// 执行完成(turn 以 Success 收尾)。
    Done,
    /// 执行失败(turn 以非 Success 收尾:MaxIterations / Stopped /
    /// TokenBudgetExceeded)。
    Failed,
}

/// 队列整体状态(持久化根对象)。`running: false`(暂停)为缺省。
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueState {
    /// 任务列表(FIFO,`Queued` 的按入队序消费)。
    pub tasks: Vec<QueueTask>,
    /// 队列模式开关:`true` = driver 自动出队;`false` = 暂停(任务保留)。
    #[serde(default)]
    pub running: bool,
}

impl QueueState {
    /// 当前 active(执行中)任务。
    pub fn active(&self) -> Option<&QueueTask> {
        self.tasks
            .iter()
            .find(|t| t.status == QueueTaskStatus::Running)
    }

    /// 下一条待出队任务(最早入队的 Queued)。
    pub fn next_queued(&self) -> Option<&QueueTask> {
        self.tasks
            .iter()
            .find(|t| t.status == QueueTaskStatus::Queued)
    }

    /// 计数摘要(供 status 输出)。
    pub fn counts(&self) -> QueueCounts {
        let mut c = QueueCounts::default();
        for t in &self.tasks {
            match t.status {
                QueueTaskStatus::Queued => c.queued += 1,
                QueueTaskStatus::Running => c.running += 1,
                QueueTaskStatus::Done => c.done += 1,
                QueueTaskStatus::Failed => c.failed += 1,
            }
        }
        c
    }
}

/// 计数摘要。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueCounts {
    pub queued: usize,
    pub running: usize,
    pub done: usize,
    pub failed: usize,
}

// ── 调度器 ───────────────────────────────────────────────────────────

/// 队列调度器。持有一组 session-scoped `QueueTask`,后台 driver 串行
/// 消费:出队 → 注入 agent loop → 等 `TurnComplete` 结清 → 下一条。
#[derive(Clone)]
pub struct QueueScheduler {
    pub(crate) state: Arc<RwLock<QueueState>>,
    pub(crate) sub_tx: Option<mpsc::Sender<Submission>>,
    session_id: ThreadId,
    /// 持久化文件(`~/.zhuye/queue/<session>.json`)。`None` = 纯内存。
    store_path: Option<std::path::PathBuf>,
    /// 序列化同一调度器的全部持久化,避免旧快照覆盖新快照。
    store_lock: Arc<parking_lot::Mutex<()>>,
    /// 加载失败时禁止写入,保留磁盘上可能仍可恢复的数据。
    store_error: Option<String>,
}

impl std::fmt::Debug for QueueScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueScheduler")
            .field("session_id", &self.session_id)
            .field("tasks_count", &self.state.read().tasks.len())
            .field("has_sub_tx", &self.sub_tx.is_some())
            .field("store_path", &self.store_path)
            .finish()
    }
}

impl QueueScheduler {
    /// 构造一个空队列。`sub_tx` 注入 agent loop 的 submission sender;
    /// 传 `None` 时 driver 不真正出队(测试用)。
    pub fn new(sub_tx: Option<mpsc::Sender<Submission>>, session_id: ThreadId) -> Self {
        Self {
            state: Arc::new(RwLock::new(QueueState::default())),
            sub_tx,
            session_id,
            store_path: None,
            store_lock: Arc::new(parking_lot::Mutex::new(())),
            store_error: None,
        }
    }

    /// 启用持久化 —— 从 `path` 加载既有队列(文件缺失 = 空),之后每次
    /// 变更原子落盘。重启恢复时,上次进程退出时仍 `Running` 的任务
    /// **回到 `Queued`**(执行到一半的 turn 无法恢复,重新排队最直观)。
    pub fn with_store(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        let path = path.into();
        match load_state(&path) {
            Ok(mut loaded) => {
                self.store_error = None;
                let mut reset = 0;
                for t in loaded.tasks.iter_mut() {
                    if t.status == QueueTaskStatus::Running {
                        t.status = QueueTaskStatus::Queued;
                        t.started_at = None;
                        reset += 1;
                    }
                }
                if reset > 0 {
                    tracing::info!(
                        path = %path.display(),
                        reset,
                        "queue: interrupted running tasks re-queued on load"
                    );
                }
                if !loaded.tasks.is_empty() {
                    tracing::info!(
                        path = %path.display(),
                        count = loaded.tasks.len(),
                        "queue: tasks loaded from store"
                    );
                }
                *self.state.write() = loaded;
            }
            Err(e) => {
                // 坏文件不覆盖:保留现有内存态,只 warn(用户数据不丢)。
                tracing::warn!(path = %path.display(), error = %e, "queue: load store failed; starting empty (file left untouched)");
                self.store_error = Some(format!("{}: {e}", path.display()));
            }
        }
        self.store_path = Some(path);
        self
    }

    /// 持久化文件路径(`None` = 纯内存)。
    pub fn store_path(&self) -> Option<&std::path::Path> {
        self.store_path.as_deref()
    }

    /// 加载失败时报告存储不可用,避免把未知状态误报为空队列。
    pub fn ensure_store_available(&self) -> std::io::Result<()> {
        match &self.store_error {
            Some(error) => Err(std::io::Error::other(error.clone())),
            None => Ok(()),
        }
    }

    /// 落盘当前状态(纯内存时 noop;失败仅 warn)。
    pub fn persist(&self) {
        if self.store_error.is_some() {
            return;
        }
        if let Some(path) = self.store_path.as_deref() {
            let _guard = self.store_lock.lock();
            if let Err(e) = save_state(path, &self.state.read()) {
                tracing::warn!(path = %path.display(), error = %e, "queue: persist failed");
            }
        }
    }

    /// 入队一条任务(FIFO 尾部)。返回入队后的任务(含 id)。
    /// 入队时若队列已在跑(running),driver 下个 tick 自动出队。
    pub fn enqueue(&self, prompt: impl Into<String>, name: Option<String>) -> QueueTask {
        let task = QueueTask {
            id: uuid_v4(),
            prompt: prompt.into(),
            name,
            status: QueueTaskStatus::Queued,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            note: None,
        };
        self.state.write().tasks.push(task.clone());
        self.persist();
        task
    }

    /// 删除一条任务(按 id)。`Queued` 直接删;`Running` 的任务删不掉
    /// (turn 已在跑,等它自己结清)。
    pub fn remove(&self, id: &str) -> Result<bool, String> {
        self.ensure_store_available().map_err(|e| e.to_string())?;
        let mut st = self.state.write();
        let Some(idx) = st.tasks.iter().position(|t| t.id == id) else {
            return Ok(false);
        };
        if st.tasks[idx].status == QueueTaskStatus::Running {
            return Err("running 任务不能删除,等当前 turn 结束或先 pause".into());
        }
        st.tasks.remove(idx);
        self.persist_locked(&st);
        drop(st);
        Ok(true)
    }

    /// 清空所有**未在执行**的任务(Queued / Done / Failed),保留 Running。
    /// 返回删除条数。
    pub fn clear(&self) -> usize {
        let mut st = self.state.write();
        let before = st.tasks.len();
        st.tasks.retain(|t| t.status == QueueTaskStatus::Running);
        let removed = before - st.tasks.len();
        drop(st);
        if removed > 0 {
            self.persist();
        }
        removed
    }

    /// 任务列表快照(按入队序)。
    pub fn list(&self) -> Vec<QueueTask> {
        self.state.read().tasks.clone()
    }

    /// 队列状态摘要。
    pub fn status(&self) -> QueueStatusSummary {
        let st = self.state.read();
        QueueStatusSummary {
            running: st.running,
            counts: st.counts(),
            active: st.active().map(|t| t.id.clone()),
            next: st.next_queued().map(|t| t.id.clone()),
        }
    }

    /// 队列模式开关:`true` = driver 自动出队;`false` = 暂停
    /// (Running 任务照常结清,只是不再出队下一条)。
    pub fn set_running(&self, running: bool) {
        {
            self.state.write().running = running;
        }
        self.persist();
        tracing::info!(running, "queue: mode toggled");
    }

    /// 结清当前 active 任务(收到 `TurnComplete` 时由 driver 调)。
    /// `success` = turn 以 `TurnStatus::Success` 收尾。没有 active 任务
    /// 时是 no-op(普通用户输入的 turn 结束不碰队列)。返回是否结清。
    pub fn settle_active(&self, success: bool, note: Option<String>) -> bool {
        let mut st = self.state.write();
        let Some(t) = st
            .tasks
            .iter_mut()
            .find(|t| t.status == QueueTaskStatus::Running)
        else {
            return false;
        };
        t.status = if success {
            QueueTaskStatus::Done
        } else {
            QueueTaskStatus::Failed
        };
        t.finished_at = Some(Utc::now());
        if t.note.is_none() {
            t.note = note;
        }
        drop(st);
        self.persist();
        true
    }

    /// 单次出队尝试:running && 无 active && 有 Queued → 首条出队,
    /// 标 Running 并注入 `Submission::user_input`。返回是否出队。
    /// 抽成独立 fn 便于单测(不必真起 driver)。
    pub async fn tick(&self) -> bool {
        // 先判断可出队(不持锁),等通道容量,再持锁复核 —— 镜像
        // `CronScheduler::tick` 的锁纪律,避免 await 持锁。
        let next_id = {
            let st = self.state.read();
            if !st.running || st.active().is_some() {
                return false;
            }
            st.next_queued().map(|t| t.id.clone())
        };
        let Some(id) = next_id else {
            return false;
        };
        let permit = match &self.sub_tx {
            Some(tx) => match tx.reserve().await {
                Ok(p) => Some(p),
                Err(_) => return false,
            },
            // 纯内存(测试):不出队,但推进状态机让语义可测。
            None => None,
        };
        let started_at = Utc::now();
        let mut st = self.state.write();
        // 复核:等容量期间可能被 pause / remove。
        let can_start = st.running
            && st.active().is_none()
            && st
                .tasks
                .iter()
                .any(|t| t.id == id && t.status == QueueTaskStatus::Queued);
        if !can_start {
            return false;
        }
        let Some(t) = st
            .tasks
            .iter_mut()
            .find(|t| t.id == id && t.status == QueueTaskStatus::Queued)
        else {
            return false;
        };
        t.status = QueueTaskStatus::Running;
        t.started_at = Some(started_at);
        let prompt = t.prompt.clone();
        let t_id = t.id.clone();
        drop(st);
        self.persist();
        if let Some(p) = permit {
            p.send(Submission::user_input(prompt));
        }
        tracing::info!(task_id = %t_id, "queue: task dequeued and injected");
        true
    }

    /// 启动后台 driver:每 `tick_secs` 秒尝试一次出队(周期兜底)。
    ///
    /// **完成信号走 Stop hook**(注册方在 `zhuye-exec`:`QueueTurnEndHook`
    /// 在引擎 check_stop 节点收到 `Stop { reason }` 时结清 active 任务并
    /// 立即触发下一条出队)—— `TurnComplete` 事件只发往 per-submission
    /// 通道,session 订阅者看不到,不能作为 driver 的驱动源。本 driver
    /// 的 interval 只兜底(hook 丢失 / pause 后重新 start 的场景)。
    /// 返回的 `QueueDriverHandle` drop 时停止 driver。
    pub fn start(self, tick_secs: u64) -> QueueDriverHandle {
        let scheduler = self;
        let join = tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(tick_secs.max(1)));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval.tick().await; // 首个 tick 立即返回,跳过
            loop {
                interval.tick().await;
                let _ = scheduler.tick().await;
            }
        });
        QueueDriverHandle { join: Some(join) }
    }
}

/// 状态摘要(`QueueTool status` / 诊断输出用)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueStatusSummary {
    pub running: bool,
    pub counts: QueueCounts,
    /// 当前执行中的任务 id。
    pub active: Option<String>,
    /// 下一条待出队任务 id。
    pub next: Option<String>,
}

// ── 持久化 ───────────────────────────────────────────────────────────

/// `remove` 持锁落盘(写锁内调用)。
impl QueueScheduler {
    fn persist_locked(&self, st: &QueueState) {
        if self.store_error.is_some() {
            return;
        }
        if let Some(path) = self.store_path.as_deref() {
            let _guard = self.store_lock.lock();
            if let Err(e) = save_state(path, st) {
                tracing::warn!(path = %path.display(), error = %e, "queue: persist failed");
            }
        }
    }
}

/// 默认持久化根目录:`<data_dir>/queue`(`~/.zhuye/queue`)。HOME 缺失 → `None`。
pub fn default_queue_base() -> Option<std::path::PathBuf> {
    zhuye_config::data_path(&["queue"])
}

/// 某 session 的持久化文件路径:`<base>/<session>.json`。
pub fn queue_store_path_for(base: &std::path::Path, session_id: &ThreadId) -> std::path::PathBuf {
    base.join(format!("{session_id}.json"))
}

/// 读取持久化的队列状态。文件缺失 → 空;其它 IO / 解析错误透传。
pub fn load_state(path: &std::path::Path) -> std::io::Result<QueueState> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).map_err(std::io::Error::other),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(QueueState::default()),
        Err(e) => Err(e),
    }
}

/// 原子写入队列状态(保留 symlink / 权限不放大 / 失败保全,经 zhuye-fs)。
pub fn save_state(path: &std::path::Path, state: &QueueState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    zhuye_fs::write_atomic(path, &body)
}

/// driver task 句柄。drop 时 abort 底层 task。
pub struct QueueDriverHandle {
    join: Option<tokio::task::JoinHandle<()>>,
}

impl QueueDriverHandle {
    /// 主动停止 driver。
    pub fn stop(&mut self) {
        if let Some(j) = self.join.take() {
            j.abort();
        }
    }
}

impl Drop for QueueDriverHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 无 uuid crate 依赖时的简易 v4 风格 id(时间戳 + 计数器 + 随机熵)。
/// zhuye-protocol 的 ThreadId 用 uuid;这里复用其依赖面最小化原则,
/// 用计数器 + 时间戳保证同进程内唯一,跨进程由 session 文件隔离。
fn uuid_v4() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts = Utc::now().timestamp_micros() as u64;
    format!("q-{ts:x}-{n:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_fifo_and_counts() {
        let q = QueueScheduler::new(None, ThreadId::new());
        let a = q.enqueue("task-a", Some("A".into()));
        let b = q.enqueue("task-b", None);
        assert_eq!(q.list().len(), 2);
        let c = q.status().counts;
        assert_eq!((c.queued, c.done), (2, 0));
        assert_eq!(q.status().next, Some(a.id));
        // FIFO:next_queued 是最早入队的。
        assert_ne!(q.status().next, Some(b.id));
    }

    #[tokio::test]
    async fn tick_dequeues_one_at_a_time_only_when_running() {
        let (tx, mut rx) = mpsc::channel(8);
        let q = QueueScheduler::new(Some(tx), ThreadId::new());
        q.enqueue("first", None);
        q.enqueue("second", None);
        // 未开启队列模式 → 不出队。
        assert!(!q.tick().await);
        assert_eq!(q.status().counts.queued, 2);
        // 开启 → 出队第一条。
        q.set_running(true);
        assert!(q.tick().await);
        let sub = rx.recv().await.expect("first task injected");
        match sub.op {
            zhuye_protocol::Op::UserInput { items, .. } => {
                assert!(items.len() == 1);
            }
            other => panic!("expected UserInput, got {other:?}"),
        }
        // 已有 active → 第二条不出队(串行语义)。
        assert!(!q.tick().await);
        assert_eq!(q.status().counts.running, 1);
        // 结清(active Done)→ 可再出队。
        assert!(q.settle_active(true, None));
        assert_eq!(q.status().counts.done, 1);
        assert!(q.tick().await);
        assert!(rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn settle_without_active_is_noop() {
        let q = QueueScheduler::new(None, ThreadId::new());
        // 普通用户输入的 turn 结束不碰队列。
        assert!(!q.settle_active(true, None));
        q.enqueue("t", None);
        assert!(!q.settle_active(false, Some("stopped".into())));
        assert_eq!(q.status().counts.queued, 1);
    }

    #[tokio::test]
    async fn failed_settle_marks_failed_with_note() {
        let q = QueueScheduler::new(None, ThreadId::new());
        q.set_running(true);
        q.enqueue("risky", None);
        assert!(q.tick().await);
        assert!(q.settle_active(false, Some("turn status: Stopped".into())));
        let list = q.list();
        assert_eq!(list[0].status, QueueTaskStatus::Failed);
        assert_eq!(list[0].note.as_deref(), Some("turn status: Stopped"));
    }

    #[test]
    fn remove_and_clear_semantics() {
        let q = QueueScheduler::new(None, ThreadId::new());
        let a = q.enqueue("a", None);
        let b = q.enqueue("b", None);
        assert!(q.remove(&a.id).unwrap());
        assert_eq!(q.list().len(), 1);
        // 未知 id → Ok(false)。
        assert!(!q.remove("nope").unwrap());
        // Running 不可删。
        q.set_running(true);
        q.state.write().tasks.first_mut().unwrap().status = QueueTaskStatus::Running;
        assert!(q.remove(&b.id).is_err());
        // clear 保留 Running。
        q.enqueue("c", None);
        assert_eq!(q.clear(), 1);
        assert_eq!(q.list().len(), 1);
        assert_eq!(q.status().counts.running, 1);
    }

    #[test]
    fn persistence_roundtrip_and_requeue_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        {
            let q = QueueScheduler::new(None, ThreadId::new()).with_store(&path);
            q.enqueue("pending", Some("P".into()));
            q.set_running(true);
            // 模拟进程中断:一条任务卡在 Running。
            q.enqueue("interrupted", None);
            q.state.write().tasks[1].status = QueueTaskStatus::Running;
        }
        // 重新加载:Running 回到 Queued。
        let q2 = QueueScheduler::new(None, ThreadId::new()).with_store(&path);
        let list = q2.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].status, QueueTaskStatus::Queued);
        assert_eq!(
            list[1].status,
            QueueTaskStatus::Queued,
            "Running 重启后重新排队"
        );
        assert!(list[1].started_at.is_none());
        assert!(q2.status().running, "开关状态随队列持久化");
        // 坏文件 → store_error,ensure_store_available 报错。
        std::fs::write(&path, "not json").unwrap();
        let q3 = QueueScheduler::new(None, ThreadId::new()).with_store(&path);
        assert!(q3.ensure_store_available().is_err());
    }

    #[test]
    fn default_base_under_zhuye_home() {
        // data_path 走 ZHUYE_HOME / HOME;只断言目录名拼进去了。
        if let Some(base) = default_queue_base() {
            assert!(
                base.ends_with("queue"),
                "base 应落在 <data>/queue,实际 {base:?}"
            );
        }
    }
}
