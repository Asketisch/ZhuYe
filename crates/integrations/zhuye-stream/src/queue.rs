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
//! - 完成信号走 **turn 终态监视**:出队时经 [`TurnSubmitter`] 拿到该
//!   submission 的 per-submission 事件通道,spawn 监视任务在
//!   `TurnComplete` / `TurnAborted` / 通道关闭(异常静默丢弃)时**按任务
//!   id** 结清并出队下一条。此前唯一结清信号是 Stop hook,但引擎存在
//!   绕过 check_stop 的结束路径(凭证耗尽 / 重试上限 / 预算耗尽 /
//!   `Op::Interrupt` / 流取消),会让 active 任务永久卡 `Running`、整条
//!   队列死锁到重启;且 Stop hook 无法归因 turn,手动输入先结束会误结清
//!   active 任务。id 归因 + 终态事件监视两者都修复;
//!   `QueueScheduler::start(tick_secs)` 的周期 driver 只作兜底。

use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use zhuye_protocol::{Event, EventMsg, Submission, ThreadId};

/// turn 注入器:提交 submission 并返回该 submission 的 per-turn 事件
/// 接收端(由 `zhuye-exec` 用 `AgentThread::submit` +
/// `TurnHandle::into_receiver` 接线)。返回 `None` = 提交失败
/// (agent loop 已关闭)。driver 用它观察 turn 终态,按任务 id 结清。
pub type TurnSubmitter = Arc<
    dyn Fn(
            Submission,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Option<mpsc::Receiver<Event>>> + Send>,
        > + Send
        + Sync,
>;

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
    /// turn 注入器(出队 → 观察 per-turn 事件终态)。`None` = 回退纯
    /// `sub_tx` 注入(测试 / 未接线场景,结清走 best-effort 语义)。
    submitter: Option<TurnSubmitter>,
    /// driver 唤醒信号:结清 / 入队 / start 时 `notify_one`,driver 不必
    /// 等周期 tick。监视任务用它代替直接递归调 `tick`(递归 async 会让
    /// rustc 在 Send 证明上陷入循环义务)。
    wake: Arc<tokio::sync::Notify>,
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
            submitter: None,
            wake: Arc::new(tokio::sync::Notify::new()),
            session_id,
            store_path: None,
            store_lock: Arc::new(parking_lot::Mutex::new(())),
            store_error: None,
        }
    }

    /// 接线 turn 注入器(出队时提交 submission 并观察其 per-turn 事件
    /// 终态,按任务 id 结清)。接线后 `tick` 不再走 `sub_tx` 直发。
    pub fn with_turn_submitter(mut self, submitter: TurnSubmitter) -> Self {
        self.submitter = Some(submitter);
        self
    }

    /// 唤醒 driver 立即尝试一次出队(不等周期 tick)。未启动 driver 时
    /// 唤醒信号保留到下次 `notified()`(`Notify` 存单个 permit),无副作用。
    pub fn wake(&self) {
        self.wake.notify_one();
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
        // 存储不可用时任务只存内存(重启即丢),显式 warn 而非静默 ——
        // 与 remove 的硬失败不对称会让人误以为已持久化。
        if self.store_error.is_some() {
            tracing::warn!(
                id = %task.id,
                "queue: store unavailable; task is memory-only and will be lost on restart"
            );
        }
        self.persist();
        // 队列已在跑且无 active 时让 driver 立即出队,不等周期 tick。
        self.wake();
        task
    }

    /// 删除一条任务(按 id)。`Queued` 直接删;`Running` 的任务删不掉
    /// (turn 已在跑,等它自己结清)。
    pub fn remove(&self, id: &str) -> Result<bool, String> {
        self.ensure_store_available().map_err(|e| e.to_string())?;
        // 注意:先改内存、drop 写锁,再落盘 —— 全局锁序是
        // state → store_lock(单向)。若在此持 state 写锁调 persist
        // (store_lock → state.read()),与 persist()(store_lock →
        // state.read())构成 ABBA 反序,两条 tokio worker 互等即死锁。
        let removed = {
            let mut st = self.state.write();
            let Some(idx) = st.tasks.iter().position(|t| t.id == id) else {
                return Ok(false);
            };
            if st.tasks[idx].status == QueueTaskStatus::Running {
                return Err("running 任务不能删除,等当前 turn 结束或先 pause".into());
            }
            st.tasks.remove(idx);
            true
        };
        self.persist();
        Ok(removed)
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
        if running {
            self.wake();
        }
        tracing::info!(running, "queue: mode toggled");
    }

    /// 结清当前 active 任务(收到 `TurnComplete` 时由 driver 调)。
    /// `success` = turn 以 `TurnStatus::Success` 收尾。没有 active 任务
    /// 时是 no-op(普通用户输入的 turn 结束不碰队列)。返回是否结清。
    ///
    /// 注意:**无法归因 turn** —— 任何 turn 结束都会结清 active 任务
    /// (包括用户手动输入先结束的场景)。生产路径请用按 id 结清的
    /// [`QueueScheduler::settle_task`]。
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

    /// 按任务 id 结清(仅当该任务仍为 `Running`;已结清 / 已删除时
    /// no-op)。turn 终态监视器用它归因,避免误结清别的任务。
    pub fn settle_task(&self, id: &str, success: bool, note: Option<String>) -> bool {
        let mut st = self.state.write();
        let Some(t) = st
            .tasks
            .iter_mut()
            .find(|t| t.id == id && t.status == QueueTaskStatus::Running)
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
    ///
    /// 接线了 [`TurnSubmitter`] 时:注入后 spawn 监视任务观察该 turn 的
    /// per-submission 事件,在终态(`TurnComplete` / `TurnAborted`)或
    /// 通道关闭(异常静默丢弃)时按任务 id 结清并立即尝试出队下一条。
    pub async fn tick(&self) -> bool {
        // 先判断可出队(不持锁),再持锁改状态 —— 避免 await 持锁。
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
        if let Some(submitter) = self.submitter.clone() {
            return self.tick_with_submitter(submitter, id).await;
        }
        // 回退路径(未接线 submitter):直发 sub_tx,不观察终态。
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

    /// `tick` 的 submitter 接线版:标 Running → 提交并观察 turn 终态。
    async fn tick_with_submitter(&self, submitter: TurnSubmitter, id: String) -> bool {
        // 先占位(标 Running),提交失败再结清为 Failed —— 占位期间
        // active().is_some() 阻止并发出队。
        let started_at = Utc::now();
        // 占位结果(块内拿写锁;guard 不跨 await)。
        let dequeued = {
            let mut st = self.state.write();
            if !st.running || st.active().is_some() {
                None
            } else {
                match st
                    .tasks
                    .iter_mut()
                    .find(|t| t.id == id && t.status == QueueTaskStatus::Queued)
                {
                    Some(t) => {
                        t.status = QueueTaskStatus::Running;
                        t.started_at = Some(started_at);
                        Some((t.id.clone(), t.prompt.clone()))
                    }
                    None => None,
                }
            }
        };
        let Some((t_id, prompt)) = dequeued else {
            return false;
        };
        self.persist();
        let submission = Submission::user_input(prompt);
        match submitter(submission).await {
            None => {
                // agent loop 已关闭(Shutdown 后):结清为 Failed,不让
                // 任务卡 Running(重启恢复语义只兜底崩溃场景)。
                self.settle_task(&t_id, false, Some("submission channel closed".into()));
                tracing::warn!(task_id = %t_id, "queue: submit failed; task marked failed");
                false
            }
            Some(rx) => {
                tracing::info!(task_id = %t_id, "queue: task dequeued and injected");
                let sched = self.clone();
                // 监视 turn 终态:终态事件或通道关闭(异常路径不总有终态
                // 事件,但 spawned turn task 结束时通道必然关闭)都结清。
                // 结清后唤醒 driver 出队下一条(不直接递归调 `tick`)。
                tokio::spawn(async move {
                    let (ok, note) = watch_turn_terminal(rx).await;
                    sched.settle_task(&t_id, ok, note);
                    sched.wake();
                });
                true
            }
        }
    }

    /// 启动后台 driver:每 `tick_secs` 秒尝试一次出队(周期兜底)。
    ///
    /// **完成信号走 turn 终态监视**(见 [`TurnSubmitter`]):出队时拿到
    /// 该 submission 的 per-submission 事件通道,终态事件 / 通道关闭时
    /// 按任务 id 结清并立即触发下一条出队。本 driver 的 interval 只兜底
    /// (监视器丢失 / pause 后重新 start 的场景)。
    /// 返回的 `QueueDriverHandle` drop 时停止 driver。
    pub fn start(self, tick_secs: u64) -> QueueDriverHandle {
        let scheduler = self;
        let join = tokio::spawn(async move {
            let period = std::time::Duration::from_secs(tick_secs.max(1));
            loop {
                // 周期兜底 **或** 被 `wake()` 唤醒(结清 / 入队 / start),
                // 二者任一到达即尝试出队。timeout 包住 notified 避免引入
                // select! 宏;Notify 存单个 permit,唤醒不会丢失。
                let _ = tokio::time::timeout(period, scheduler.wake.notified()).await;
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

/// 消费一条已注入 turn 的 per-submission 事件流,直到终态:
/// - `TurnComplete` → `(status == Success, None)`,非 Success 带 note;
/// - `TurnAborted` → `(false, Some("turn aborted…"))`;
/// - 通道关闭但未见终态(错误静默丢弃路径)→ `(false, Some(…))`。
async fn watch_turn_terminal(mut rx: mpsc::Receiver<Event>) -> (bool, Option<String>) {
    use zhuye_protocol::TurnStatus;
    while let Some(ev) = rx.recv().await {
        match ev.msg {
            EventMsg::TurnComplete(tc) => {
                return match tc.status {
                    TurnStatus::Success => (true, None),
                    status => (
                        false,
                        Some(format!("turn status: {}", turn_status_str(status))),
                    ),
                };
            }
            EventMsg::TurnAborted(_) => {
                return (false, Some("turn aborted (interrupt / shutdown)".into()));
            }
            _ => {}
        }
    }
    (false, Some("turn ended without terminal event".into()))
}

/// `TurnStatus` 的稳定字符串(note 用;不依赖 Debug 输出)。
fn turn_status_str(s: zhuye_protocol::TurnStatus) -> &'static str {
    use zhuye_protocol::TurnStatus;
    match s {
        TurnStatus::Success => "success",
        TurnStatus::MaxIterations => "max_iterations",
        TurnStatus::Stopped => "stopped",
        TurnStatus::TokenBudgetExceeded => "token_budget_exceeded",
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

    /// submitter 接线版 tick:注入后监视 turn 终态,TurnComplete(Success)
    /// → Done;TurnAborted / 通道关闭 → Failed(带 note)。
    #[tokio::test]
    async fn submitter_tick_settles_on_turn_terminal_events() {
        // 每次提交给一条独立事件通道(把事件发送端交给测试,接收端回传给
        // 监视器);本测试手动喂终态事件,并手动推进 tick(未起 driver)。
        let (task_tx, mut task_rx) = mpsc::channel::<mpsc::Sender<Event>>(4);
        let submitter: TurnSubmitter = Arc::new(move |sub| {
            let tx = task_tx.clone();
            Box::pin(async move {
                let (etx, erx) = mpsc::channel(8);
                let _ = tx.send(etx).await;
                let _ = sub; // 不真正投递 loop,只回传观察通道
                Some(erx)
            })
        });
        let q = QueueScheduler::new(None, ThreadId::new()).with_turn_submitter(submitter);
        q.set_running(true);
        let t1 = q.enqueue("ok-task", None);
        assert!(q.tick().await);
        // 提交成功 → Running;喂 Success 终态 → Done。
        let etx1 = task_rx.recv().await.expect("submitter called");
        etx1.send(turn_complete_event(true)).await.unwrap();
        drop(etx1);
        wait_for(|| q.list()[0].status == QueueTaskStatus::Done).await;
        let _ = t1;
        // Aborted → Failed(带 note)。
        let t2 = q.enqueue("aborted-task", None);
        assert!(q.tick().await);
        let etx2 = task_rx.recv().await.expect("second submitter call");
        etx2.send(turn_abort_event()).await.unwrap();
        drop(etx2);
        wait_for(|| {
            q.list()
                .iter()
                .any(|t| t.id == t2.id && t.status == QueueTaskStatus::Failed)
        })
        .await;
        let t2_final = {
            let list = q.list();
            list.iter().find(|t| t.id == t2.id).cloned().unwrap()
        };
        assert!(
            t2_final
                .note
                .as_deref()
                .is_some_and(|n| n.contains("aborted")),
            "Aborted 结清应带 note:{:?}",
            t2_final.note
        );
        // 通道关闭但无终态(异常静默丢弃)→ Failed + note。
        let t3 = q.enqueue("dropped-task", None);
        assert!(q.tick().await);
        let etx3 = task_rx.recv().await.expect("third submitter call");
        etx3.send(make_event("irrelevant")).await.unwrap();
        drop(etx3);
        wait_for(|| {
            q.list()
                .iter()
                .any(|t| t.id == t3.id && t.status == QueueTaskStatus::Failed)
        })
        .await;
        let t3_final = {
            let list = q.list();
            list.iter().find(|t| t.id == t3.id).cloned().unwrap()
        };
        assert!(
            t3_final
                .note
                .as_deref()
                .is_some_and(|n| n.contains("without terminal")),
            "静默丢弃结清应带 note:{:?}",
            t3_final.note
        );
    }

    /// 归因回归:别的 turn 的终态(监视器之外的 settle_active 语义)
    /// 不得误结清 Running 任务 —— settle_task 只按 id 生效。
    #[tokio::test]
    async fn settle_task_is_id_attributed() {
        let q = QueueScheduler::new(None, ThreadId::new());
        q.set_running(true);
        let a = q.enqueue("a", None);
        q.state.write().tasks[0].status = QueueTaskStatus::Running;
        // 未知 id / 已结清 id → no-op,不碰 Running。
        assert!(!q.settle_task("nope", true, None));
        assert!(q.settle_task(&a.id, false, Some("boom".into())));
        assert_eq!(q.status().counts.failed, 1);
        // 再结清同 id(已 Failed)→ no-op,note 不被覆盖。
        assert!(!q.settle_task(&a.id, true, None));
        assert_eq!(q.list()[0].note.as_deref(), Some("boom"));
    }

    /// submitter 返回 None(通道关闭)→ 任务结清 Failed,不卡 Running。
    #[tokio::test]
    async fn submitter_closed_channel_marks_failed() {
        let submitter: TurnSubmitter = Arc::new(|_| Box::pin(async move { None }));
        let q = QueueScheduler::new(None, ThreadId::new()).with_turn_submitter(submitter);
        q.set_running(true);
        let t = q.enqueue("lost", None);
        assert!(!q.tick().await, "提交失败不算成功出队");
        let final_t = q.list().into_iter().find(|x| x.id == t.id).unwrap();
        assert_eq!(final_t.status, QueueTaskStatus::Failed);
        assert_eq!(q.status().counts.running, 0, "不得残留 Running");
    }

    // ── 测试辅助 ─────────────────────────────────────────────────────

    fn make_event(kind_text: &str) -> Event {
        Event::new(
            zhuye_protocol::EVENT_ID_NONE,
            EventMsg::AgentMessageDelta(zhuye_protocol::AgentMessageDelta {
                delta: kind_text.into(),
            }),
        )
    }

    fn turn_complete_event(success: bool) -> Event {
        Event::new(
            zhuye_protocol::EVENT_ID_NONE,
            EventMsg::TurnComplete(zhuye_protocol::TurnCompleteEvent {
                turn_id: zhuye_protocol::TurnId::new(),
                usage: Default::default(),
                status: if success {
                    zhuye_protocol::TurnStatus::Success
                } else {
                    zhuye_protocol::TurnStatus::MaxIterations
                },
            }),
        )
    }

    fn turn_abort_event() -> Event {
        Event::new(
            zhuye_protocol::EVENT_ID_NONE,
            EventMsg::TurnAborted(zhuye_protocol::TurnAbortedEvent {
                turn_id: zhuye_protocol::TurnId::new(),
                reason: zhuye_protocol::AbortReason::UserInterrupt,
            }),
        )
    }

    /// 轮询断言(watcher 是 spawn 的异步任务,给一点调度时间)。
    async fn wait_for(f: impl Fn() -> bool) {
        for _ in 0..200 {
            if f() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(f(), "条件在 1s 内未满足");
    }
}
