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
    /// v1.11 基建#6:goal 任务载荷。`Some` 时本任务以目标模式执行 ——
    /// 出队时先注入 `Op::EnterGoalMode`,prompt 作为首个目标 turn;
    /// turn 结束后经 [`GoalProbe`] 探测目标状态,未完成则继续注入
    /// 续跑 turn(目标模式的续作文本在 steering 队列,任意 user_input
    /// 在 turn 边界合并),直到目标 Complete / Blocked / 预算尽,
    /// 最后注入 `Op::ExitGoalMode` 收尾 —— 队列由此**顺序执行多个
    /// goal**,每个 goal 的 controller 生命周期被严格框在自身任务内,
    /// 不污染下一条任务。
    #[serde(default)]
    pub goal: Option<QueueTaskGoal>,
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

/// v1.11 基建#6:goal 任务的载荷字段(镜像 `Op::EnterGoalMode`)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueTaskGoal {
    /// 目标描述(EnterGoalMode.goal / 首个 turn 的 prompt 即此文本)。
    pub objective: String,
    /// AND 条件 verify 命令(verdict=met 且命令 exit 0 才算完成)。
    #[serde(default)]
    pub verify_command: Option<String>,
    /// 目标模式 token 预算(软停)。
    #[serde(default)]
    pub token_budget: Option<u64>,
}

/// v1.11 基建#6:goal 状态探针 —— driver 在每个 goal turn 结束后读它
/// 决定续跑 / 结清。zhuye-stream(integrations 层)不依赖 zhuye-core,
/// 由 `zhuye-exec` 装配时闭包读 `AgentConfig.goal` 的 controller 快照。
pub type GoalProbe = Arc<dyn Fn() -> GoalProbeSnapshot + Send + Sync>;

/// 探针返回的目标状态快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoalProbeSnapshot {
    pub status: GoalProbeStatus,
    /// 已跑的 goal turn 数(controller.turn_count)。
    pub turns: u32,
}

/// 目标状态(driver 只关心「是否继续」与终态归类)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalProbeStatus {
    /// 无活跃 controller(未进入 / 已退出目标模式)。
    None,
    Active,
    Paused,
    Blocked,
    BudgetLimited,
    Complete,
}

/// 单个 goal 任务的续跑 turn 上限(防探针失真时无限循环;
/// 正常路径由 controller 的 3-strike / token 预算先行兜底)。
pub const MAX_GOAL_TURNS_PER_TASK: u32 = 16;

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
    /// v1.11 基建#6:goal 状态探针。`None` = 未接线,goal 任务出队时
    /// 直接结清为 Failed(无法观测目标终态,不盲跑)。
    goal_probe: Option<GoalProbe>,
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
            goal_probe: None,
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

    /// v1.11 基建#6:接线 goal 状态探针(goal 任务续跑 / 结清的依据)。
    pub fn with_goal_probe(mut self, probe: GoalProbe) -> Self {
        self.goal_probe = Some(probe);
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
            goal: None,
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

    /// v1.11 基建#6:入队一条 **goal 任务**(FIFO 尾部)。出队时以目标
    /// 模式执行:EnterGoalMode → objective 首个 turn → 探针驱动续跑 →
    /// 终态(Complete=Done / Blocked|BudgetLimited=Failed)→ ExitGoalMode。
    /// 与普通 prompt 任务同队列混排、严格串行。
    pub fn enqueue_goal(
        &self,
        objective: impl Into<String>,
        verify_command: Option<String>,
        token_budget: Option<u64>,
        name: Option<String>,
    ) -> QueueTask {
        let objective = objective.into();
        let task = QueueTask {
            id: uuid_v4(),
            // prompt 冗余存 objective(driver 注入直接用;list 渲染也用它),
            // goal 载荷里保留全量参数。
            prompt: objective.clone(),
            name,
            goal: Some(QueueTaskGoal {
                objective,
                verify_command,
                token_budget,
            }),
            status: QueueTaskStatus::Queued,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            note: None,
        };
        self.state.write().tasks.push(task.clone());
        if self.store_error.is_some() {
            tracing::warn!(
                id = %task.id,
                "queue: store unavailable; goal task is memory-only and will be lost on restart"
            );
        }
        self.persist();
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
    /// v1.11 基建#6:goal 任务先注入 `Op::EnterGoalMode`,再注入 objective
    /// 首个 turn;续跑与结清走 [`watch_goal_task`](探针驱动)。
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
                        Some((t.id.clone(), t.prompt.clone(), t.goal.clone()))
                    }
                    None => None,
                }
            }
        };
        let Some((t_id, prompt, goal)) = dequeued else {
            return false;
        };
        self.persist();
        // goal 任务:先注入 EnterGoalMode op(mpsc 有序,先于下面的
        // user_input 处理;fire-and-forget,receiver 直接丢弃)。
        if let Some(g) = &goal {
            if self.goal_probe.is_none() {
                // 未接线探针:无法观测目标终态,不盲跑 —— 直接结清 Failed。
                self.settle_task(
                    &t_id,
                    false,
                    Some("goal probe not wired; goal task refused".into()),
                );
                tracing::warn!(task_id = %t_id, "queue: goal task refused (no probe)");
                return false;
            }
            let enter = Submission::with_id(
                uuid_v4(),
                zhuye_protocol::Op::EnterGoalMode {
                    goal: g.objective.clone(),
                    verify_command: g.verify_command.clone(),
                    token_budget: g.token_budget,
                },
            );
            if submitter(enter).await.is_none() {
                self.settle_task(&t_id, false, Some("submission channel closed".into()));
                return false;
            }
        }
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
                match goal {
                    // goal 任务:探针驱动续跑,终态结清 + ExitGoalMode 收尾。
                    Some(g) => {
                        let probe = self
                            .goal_probe
                            .clone()
                            .expect("goal probe 已在上文校验非空");
                        tokio::spawn(async move {
                            watch_goal_task(sched.clone(), submitter, probe, t_id, g, rx).await;
                        });
                    }
                    // 普通任务:终态事件或通道关闭(异常路径不总有终态
                    // 事件,但 spawned turn task 结束时通道必然关闭)都结清。
                    // 结清后唤醒 driver 出队下一条(不直接递归调 `tick`)。
                    None => {
                        tokio::spawn(async move {
                            let (ok, note) = watch_turn_terminal(rx).await;
                            sched.settle_task(&t_id, ok, note);
                            sched.wake();
                        });
                    }
                }
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

/// v1.11 基建#6:goal 任务的 turn 循环监视器。
///
/// 语义:首个 turn(已注入)终态后,**先等 `on_turn_end` 校验落定**
/// (引擎先发 TurnComplete 再跑 goal 自校验,校验含 LLM 调用可能滞后
/// 数秒 —— 探针需轮询到 `turn_count` 增量或状态离开 Active,否则会
/// 把刚完成的目标误判为 Active 空跑一轮),再按落定状态决策 ——
/// - `Active` 且未超 [`MAX_GOAL_TURNS_PER_TASK`] → 注入一条续跑
///   user_input(目标模式的续作文本在 steering 队列,turn 边界自动
///   合并进本条输入),继续监视;
/// - `Complete` → ExitGoalMode + 结清 Done;
/// - `Blocked` / `BudgetLimited` / `Paused` / `None`(未开成 /
///   中途被清)→ ExitGoalMode + 结清 Failed(note 带状态);
/// - `Active` 但超 turn 上限(探针失真兜底)→ ExitGoalMode + Failed。
///
/// ExitGoalMode 保证 controller 生命周期框在单任务内,下一条队列
/// 任务(普通 / goal)从干净状态开始。
async fn watch_goal_task(
    sched: QueueScheduler,
    submitter: TurnSubmitter,
    probe: GoalProbe,
    t_id: String,
    goal: QueueTaskGoal,
    first_rx: mpsc::Receiver<Event>,
) {
    let mut rx = first_rx;
    // 本任务已观察到的 turn 终态数;on_turn_end 每跑一次 turn_count +1,
    // `turn_count >= 已观察终态数` 即代表最新 turn 的校验已落定。
    let mut turns_seen: u32 = 0;
    loop {
        // 等当前 turn 终态(首圈是 objective turn,后续是续跑 turn)。
        let (turn_ok, note) = watch_turn_terminal(rx).await;
        if !turn_ok {
            // turn 失败(含中断):目标未竟,结清 Failed;仍发 ExitGoalMode
            // 清理 controller(submission 通道关闭时放弃 —— 会话已死)。
            submit_exit_goal(&submitter).await;
            sched.settle_task(&t_id, false, note.or(Some("goal turn failed".into())));
            sched.wake();
            return;
        }
        turns_seen += 1;
        let snap = wait_goal_settled(&probe, turns_seen).await;
        match snap.status {
            GoalProbeStatus::Active => {
                if turns_seen >= MAX_GOAL_TURNS_PER_TASK || snap.turns >= MAX_GOAL_TURNS_PER_TASK {
                    submit_exit_goal(&submitter).await;
                    sched.settle_task(
                        &t_id,
                        false,
                        Some(format!(
                            "goal turn cap reached ({MAX_GOAL_TURNS_PER_TASK}); objective: {}",
                            truncate_str(&goal.objective, 60)
                        )),
                    );
                    sched.wake();
                    return;
                }
                // 续跑:续作文本由引擎推进 steering 队列,与本条输入在
                // turn 边界合并(pre_loop 的 inject_turn_boundary_steering)。
                let cont = Submission::user_input("继续推进当前目标(队列 goal 续跑)。");
                match submitter(cont).await {
                    Some(next_rx) => {
                        rx = next_rx;
                        continue;
                    }
                    None => {
                        sched.settle_task(&t_id, false, Some("submission channel closed".into()));
                        sched.wake();
                        return;
                    }
                }
            }
            GoalProbeStatus::Complete => {
                submit_exit_goal(&submitter).await;
                sched.settle_task(&t_id, true, Some("goal complete".into()));
                sched.wake();
                return;
            }
            status => {
                submit_exit_goal(&submitter).await;
                let label = match status {
                    GoalProbeStatus::Blocked => "goal blocked",
                    GoalProbeStatus::BudgetLimited => "goal budget limited",
                    GoalProbeStatus::Paused => "goal paused",
                    _ => "goal mode inactive",
                };
                sched.settle_task(&t_id, false, Some(label.into()));
                sched.wake();
                return;
            }
        }
    }
}

/// 轮询等待最新 turn 的 `on_turn_end` 校验落定:
/// `turn_count >= min_turns`(校验已跑)或状态离开 Active(终态)即返回。
/// 超时(校验 LLM 卡死 / 探针失真)按当时快照返回,由调用方按上限兜底。
async fn wait_goal_settled(probe: &GoalProbe, min_turns: u32) -> GoalProbeSnapshot {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);
    const SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
    let deadline = tokio::time::Instant::now() + SETTLE_TIMEOUT;
    loop {
        let snap = probe();
        if snap.status != GoalProbeStatus::Active || snap.turns >= min_turns {
            return snap;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                min_turns,
                "queue: goal settle poll timed out; using stale snapshot"
            );
            return snap;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// 注入 `Op::ExitGoalMode`(fire-and-forget;通道关闭时静默放弃)。
async fn submit_exit_goal(submitter: &TurnSubmitter) {
    let exit = Submission::with_id(uuid_v4(), zhuye_protocol::Op::ExitGoalMode);
    let _ = submitter(exit).await;
}

/// 简单按字符截断(note 用)。
fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
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
    // ── v1.11 基建#6:goal 任务(多 goal 顺序执行) ────────────────────

    /// 可脚本化的 fake turn 注入器:把每次 submission 与它的 rx 转发给
    /// 测试驱动侧,由测试模拟引擎(发 TurnComplete / 改探针状态)。
    fn fake_submitter() -> (
        TurnSubmitter,
        mpsc::UnboundedReceiver<(Submission, mpsc::Sender<Event>)>,
    ) {
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let submitter: TurnSubmitter = Arc::new(move |sub| {
            let (tx, rx) = mpsc::channel(4);
            let _ = out_tx.send((sub, tx));
            Box::pin(async move { Some(rx) })
        });
        (submitter, out_rx)
    }

    /// 驱动一次「引擎模拟」:等下一个 submission;若是 UserInput turn →
    /// 发 TurnComplete(Success) 并按脚本推进探针;若是 EnterGoalMode /
    /// ExitGoalMode op → 记录。返回这次 submission 的 op 类型标签。
    async fn drive_one(
        rx: &mut mpsc::UnboundedReceiver<(Submission, mpsc::Sender<Event>)>,
        probe_state: &Arc<std::sync::Mutex<GoalProbeSnapshot>>,
        next_snapshot: GoalProbeSnapshot,
    ) -> String {
        use tokio::time::Duration;
        let (sub, tx) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("submitter 未收到新 submission")
            .expect("submitter 通道关闭");
        match sub.op {
            zhuye_protocol::Op::EnterGoalMode { .. } => "enter_goal".into(),
            zhuye_protocol::Op::ExitGoalMode => "exit_goal".into(),
            zhuye_protocol::Op::UserInput { .. } => {
                // 模拟引擎:turn 成功收尾,随后 on_turn_end 落定(推进探针)。
                let _ = tx.send(turn_complete_event(true)).await;
                *probe_state.lock().unwrap() = next_snapshot;
                "user_turn".into()
            }
            other => panic!("未预期的 Op:{other:?}"),
        }
    }

    /// goal 任务完整生命周期:EnterGoalMode → objective turn(unmet,
    /// Active)→ 续跑 turn(met,Complete)→ ExitGoalMode → 结清 Done。
    /// 验证「一个队列排 goal,顺序跑到底才轮到下一条」。
    #[tokio::test]
    async fn goal_task_runs_to_terminal_then_settles_done() {
        let (submitter, mut loop_rx) = fake_submitter();
        // 探针状态由 drive_one 按脚本推进(模拟 on_turn_end 落定)。
        let probe_state: Arc<std::sync::Mutex<GoalProbeSnapshot>> =
            Arc::new(std::sync::Mutex::new(GoalProbeSnapshot {
                status: GoalProbeStatus::None,
                turns: 0,
            }));
        let ps = probe_state.clone();
        let probe: GoalProbe = Arc::new(move || *ps.lock().unwrap());
        let q = QueueScheduler::new(None, ThreadId::new())
            .with_turn_submitter(submitter)
            .with_goal_probe(probe);
        q.enqueue_goal("目标A", Some("true".into()), Some(1000), Some("g1".into()));
        q.set_running(true);
        assert!(q.tick().await, "goal 任务应出队");

        // 1) 先 EnterGoalMode(objective / verify / budget 全量透传)。
        let (sub, _) = tokio::time::timeout(std::time::Duration::from_secs(5), loop_rx.recv())
            .await
            .expect("等 EnterGoalMode")
            .expect("通道关闭");
        match sub.op {
            zhuye_protocol::Op::EnterGoalMode {
                goal,
                verify_command,
                token_budget,
            } => {
                assert_eq!(goal, "目标A");
                assert_eq!(verify_command.as_deref(), Some("true"));
                assert_eq!(token_budget, Some(1000));
            }
            other => panic!("首个 submission 应是 EnterGoalMode,得到 {other:?}"),
        }
        // 2) objective turn → 未达成(Active, turns=1)。
        let step = drive_one(
            &mut loop_rx,
            &probe_state,
            GoalProbeSnapshot {
                status: GoalProbeStatus::Active,
                turns: 1,
            },
        )
        .await;
        assert_eq!(step, "user_turn");
        // 3) 续跑 turn → 达成(Complete, turns=2)。
        let step = drive_one(
            &mut loop_rx,
            &probe_state,
            GoalProbeSnapshot {
                status: GoalProbeStatus::Complete,
                turns: 2,
            },
        )
        .await;
        assert_eq!(step, "user_turn", "Active 后应注入续跑 turn");
        // 4) ExitGoalMode 收尾。
        let (sub, _) = tokio::time::timeout(std::time::Duration::from_secs(5), loop_rx.recv())
            .await
            .expect("等 ExitGoalMode")
            .expect("通道关闭");
        assert!(matches!(sub.op, zhuye_protocol::Op::ExitGoalMode));
        // 5) 任务结清 Done,note 标完成。
        wait_for(|| {
            q.list().first().is_some_and(|t| {
                t.status == QueueTaskStatus::Done && t.note.as_deref() == Some("goal complete")
            })
        })
        .await;
    }

    /// 两个 goal 任务顺序执行:第一个跑完 ExitGoalMode 后,第二个才
    /// EnterGoalMode(严格串行,controller 生命周期不互相污染)。
    #[tokio::test]
    async fn two_goal_tasks_run_strictly_serial() {
        let (submitter, mut loop_rx) = fake_submitter();
        let probe_state: Arc<std::sync::Mutex<GoalProbeSnapshot>> =
            Arc::new(std::sync::Mutex::new(GoalProbeSnapshot {
                status: GoalProbeStatus::None,
                turns: 0,
            }));
        let ps = probe_state.clone();
        let probe: GoalProbe = Arc::new(move || *ps.lock().unwrap());
        let q = QueueScheduler::new(None, ThreadId::new())
            .with_turn_submitter(submitter)
            .with_goal_probe(probe);
        q.enqueue_goal("目标一", None, None, None);
        q.enqueue_goal("目标二", None, None, None);
        q.set_running(true);
        // 后台 driver:第一个 goal 结清后自动出队第二个(测试只驱动引擎侧)。
        let _driver = q.clone().start(1);

        // 驱动序列:enter → turn(Active,1)→ 续跑(Complete,2)→ exit
        // → (第二任务)enter → turn(Complete,2)→ exit。
        let script = [
            GoalProbeSnapshot {
                status: GoalProbeStatus::Active,
                turns: 1,
            },
            GoalProbeSnapshot {
                status: GoalProbeStatus::Complete,
                turns: 2,
            },
            GoalProbeSnapshot {
                status: GoalProbeStatus::Complete,
                turns: 2,
            },
        ];
        let mut script_iter = script.iter().copied();
        let mut seen: Vec<String> = Vec::new();
        // 两个任务共:2×(enter + N turns + exit);用有限步数驱动。
        for _ in 0..10 {
            let next = script_iter.next().unwrap_or(GoalProbeSnapshot {
                status: GoalProbeStatus::Complete,
                turns: 2,
            });
            let label = drive_one(&mut loop_rx, &probe_state, next).await;
            seen.push(label.clone());
            // 两个任务都 Done 即停。
            let all_done = q.list().iter().all(|t| t.status == QueueTaskStatus::Done);
            if all_done && seen.iter().filter(|s| *s == "exit_goal").count() >= 2 {
                break;
            }
        }
        // enter 与 exit 各恰好两次,且第二次 enter 在第一次 exit 之后。
        let enter_positions: Vec<usize> = seen
            .iter()
            .enumerate()
            .filter(|(_, s)| s.as_str() == "enter_goal")
            .map(|(i, _)| i)
            .collect();
        let exit_positions: Vec<usize> = seen
            .iter()
            .enumerate()
            .filter(|(_, s)| s.as_str() == "exit_goal")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(enter_positions.len(), 2, "两个 goal 各 enter 一次:{seen:?}");
        assert_eq!(exit_positions.len(), 2, "两个 goal 各 exit 一次:{seen:?}");
        assert!(
            exit_positions[0] < enter_positions[1],
            "第二任务必须在第一任务 exit 后才 enter:{seen:?}"
        );
        wait_for(|| q.list().iter().all(|t| t.status == QueueTaskStatus::Done)).await;
    }

    /// 未接线探针时,goal 任务出队被拒(结清 Failed,不盲跑)。
    #[tokio::test]
    async fn goal_task_without_probe_refused() {
        let (submitter, _loop_rx) = fake_submitter();
        let q = QueueScheduler::new(None, ThreadId::new()).with_turn_submitter(submitter);
        q.enqueue_goal("无探针目标", None, None, None);
        q.set_running(true);
        assert!(!q.tick().await, "未接线探针时不应真正出队");
        wait_for(|| {
            q.list().first().is_some_and(|t| {
                t.status == QueueTaskStatus::Failed
                    && t.note.as_deref().is_some_and(|n| n.contains("probe"))
            })
        })
        .await;
    }

    /// goal turn 失败(turn 非 Success)→ ExitGoalMode + 结清 Failed。
    #[tokio::test]
    async fn goal_task_failed_turn_settles_failed() {
        let (submitter, mut loop_rx) = fake_submitter();
        let probe_state: Arc<std::sync::Mutex<GoalProbeSnapshot>> =
            Arc::new(std::sync::Mutex::new(GoalProbeSnapshot {
                status: GoalProbeStatus::Active,
                turns: 1,
            }));
        let ps = probe_state.clone();
        let probe: GoalProbe = Arc::new(move || *ps.lock().unwrap());
        let q = QueueScheduler::new(None, ThreadId::new())
            .with_turn_submitter(submitter)
            .with_goal_probe(probe);
        q.enqueue_goal("会失败的目标", None, None, None);
        q.set_running(true);
        assert!(q.tick().await);
        // 消费 EnterGoalMode。
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), loop_rx.recv())
            .await
            .expect("等 EnterGoalMode");
        // objective turn 以 MaxIterations 失败收尾。
        let (_, tx) = tokio::time::timeout(std::time::Duration::from_secs(5), loop_rx.recv())
            .await
            .expect("等 objective turn")
            .expect("通道关闭");
        let _ = tx.send(turn_complete_event(false)).await;
        // ExitGoalMode 收尾 + Failed 结清。
        let (sub, _) = tokio::time::timeout(std::time::Duration::from_secs(5), loop_rx.recv())
            .await
            .expect("等 ExitGoalMode")
            .expect("通道关闭");
        assert!(matches!(sub.op, zhuye_protocol::Op::ExitGoalMode));
        wait_for(|| {
            q.list()
                .first()
                .is_some_and(|t| t.status == QueueTaskStatus::Failed)
        })
        .await;
    }
}
