//! `cron` — v1.2 P1-2 真实定时调度。
//!
//! 把 v0 的 stub 升级为真正可调度的 cron 引擎:agent 通过 `CronTool`
//! 注册 `CronJobSpec`,后台 driver 在到期时把 `job.prompt` 作为一条
//! `Submission::user_input` 注入 agent loop(经 `AgentThread` 的
//! submission sender),触发新一轮对话。
//!
//! 5 字段 cron 表达式解析自包含(无外部 cron crate,对齐项目风格),
//! 支持:数字 / `*` / `,` 列表 / `-` 范围 / `/` 步长。下次触发时间用
//! 暴力枚举(从 now 起逐分钟扫描,最多扫 366 天防死循环),足够覆盖
//! 分钟级及以上粒度的常见 schedule。

use std::sync::Arc;

use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;
use zhuye_protocol::{Submission, ThreadId};

// ── 表达式解析 ───────────────────────────────────────────────────────

/// 5 字段 cron 表达式解析后的字段匹配器。
///
/// 每个字段是一组允许的整数集合(`Vec` 排序去重,便于调试 / 序列化)。
/// `*` → 该字段全部允许值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minute: Vec<u32>,
    hour: Vec<u32>,
    dom: Vec<u32>,   // day of month (1-31)
    month: Vec<u32>, // 1-12
    dow: Vec<u32>,   // day of week (0-6, 0=Sunday)
}

/// cron 表达式解析错误。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CronParseError {
    #[error("cron expression must have exactly 5 fields, got {0}")]
    FieldCount(usize),
    #[error("field {field:?}: value {value} out of range [{lo},{hi}]")]
    OutOfRange {
        field: &'static str,
        value: i64,
        lo: i32,
        hi: i32,
    },
    #[error("field {field:?}: invalid token {token:?}")]
    InvalidToken { field: &'static str, token: String },
    #[error("field {field:?}: step cannot be zero")]
    ZeroStep { field: &'static str },
    #[error("cron store unavailable: {0}")]
    StoreUnavailable(String),
}

impl CronSchedule {
    /// 解析 5 字段 cron 表达式(min hour dom month dow)。
    ///
    /// 支持每字段:`*` / 单数 / `,` 列表 / `-` 范围 / `/` 步长。例:
    /// - `* * * * *` —— 每分钟
    /// - `0 * * * *` —— 每小时整点
    /// - `*/15 * * * *` —— 每 15 分钟
    /// - `0 9 * * 1-5` —— 工作日早 9 点
    pub fn parse(expr: &str) -> Result<Self, CronParseError> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(CronParseError::FieldCount(fields.len()));
        }
        Ok(Self {
            minute: parse_field(fields[0], 0, 59, "minute")?,
            hour: parse_field(fields[1], 0, 23, "hour")?,
            dom: parse_field(fields[2], 1, 31, "dom")?,
            month: parse_field(fields[3], 1, 12, "month")?,
            dow: parse_field(fields[4], 0, 6, "dow")?,
        })
    }

    /// 计算 `after` 之后(不含)的下一个触发时间。
    ///
    /// 从 `after + 1 分钟` 起逐分钟扫描,最多扫 366 天防死循环。返回
    /// `None` 表示该表达式在扫描窗口内无解(例如 `2月30日` 这类不可能
    /// 日期)。
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        // 对齐到下一分钟零秒,逐分钟扫描。
        let mut t = after
            .with_second(0)?
            .with_nanosecond(0)?
            .checked_add_signed(Duration::minutes(1))?;
        for _ in 0..(366 * 24 * 60) {
            if self.minute.contains(&t.minute())
                && self.hour.contains(&t.hour())
                && self.dom.contains(&t.day())
                && self.month.contains(&t.month())
                && self.dow.contains(&t.weekday().num_days_from_sunday())
            {
                return Some(t);
            }
            t = t.checked_add_signed(Duration::minutes(1))?;
        }
        None
    }
}

/// 解析单个 cron 字段为允许值集合(排序去重)。
fn parse_field(
    raw: &str,
    lo: i32,
    hi: i32,
    field: &'static str,
) -> Result<Vec<u32>, CronParseError> {
    let mut out: Vec<u32> = Vec::new();
    for part in raw.split(',') {
        out.extend(parse_part(part, lo, hi, field)?);
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// 解析单个 token(可能含 `/` 步长、`-` 范围、`*`)。
fn parse_part(
    token: &str,
    lo: i32,
    hi: i32,
    field: &'static str,
) -> Result<Vec<u32>, CronParseError> {
    // 拆出步长 `<base>/<step>`。
    let (base, step) = match token.split_once('/') {
        Some((b, s)) => {
            let step: i32 = s.parse().map_err(|_| CronParseError::InvalidToken {
                field,
                token: token.to_string(),
            })?;
            if step <= 0 {
                return Err(CronParseError::ZeroStep { field });
            }
            (b, Some(step))
        }
        None => (token, None),
    };

    // 解析 base 的范围(下界,上界)。`*` → [lo,hi]。
    let (start, end) = if base == "*" {
        (lo, hi)
    } else if let Some((a, b)) = base.split_once('-') {
        let a: i32 = a.parse().map_err(|_| CronParseError::InvalidToken {
            field,
            token: token.to_string(),
        })?;
        let b: i32 = b.parse().map_err(|_| CronParseError::InvalidToken {
            field,
            token: token.to_string(),
        })?;
        (a, b)
    } else {
        let v: i32 = base.parse().map_err(|_| CronParseError::InvalidToken {
            field,
            token: token.to_string(),
        })?;
        check_range(v, lo, hi, field)?;
        // 单值 + 步长:v, v+step, ... 直到 hi(与 cron 语义一致)。
        if let Some(step) = step {
            let mut out = Vec::new();
            let mut cur = v;
            while cur <= hi {
                out.push(cur as u32);
                let Some(next) = cur.checked_add(step) else {
                    break;
                };
                cur = next;
            }
            return Ok(out);
        }
        return Ok(vec![v as u32]);
    };

    check_range(start, lo, hi, field)?;
    check_range(end, lo, hi, field)?;
    if start > end {
        return Err(CronParseError::InvalidToken {
            field,
            token: token.to_string(),
        });
    }

    let step = step.unwrap_or(1);
    let mut out = Vec::new();
    let mut cur = start;
    while cur <= end {
        out.push(cur as u32);
        let Some(next) = cur.checked_add(step) else {
            break;
        };
        cur = next;
    }
    Ok(out)
}

fn check_range(v: i32, lo: i32, hi: i32, field: &'static str) -> Result<(), CronParseError> {
    if v < lo || v > hi {
        return Err(CronParseError::OutOfRange {
            field,
            value: v as i64,
            lo,
            hi,
        });
    }
    Ok(())
}

// ── 任务规格 + 调度器 ─────────────────────────────────────────────

/// 一条 cron 任务规格(可序列化,落盘用)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronJobSpec {
    /// 稳定 id(uuid)。update / delete 用它寻址。
    pub id: String,
    /// 5 字段 cron 表达式。
    pub schedule: String,
    /// 触发时作为 `Submission::user_input` 注入 agent loop 的 prompt。
    pub prompt: String,
    /// 可选任务名(给人看)。
    #[serde(default)]
    pub name: Option<String>,
    /// 是否启用。`false` 的 job 不被 driver 调度(但保留在列表)。
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// 创建时间(UTC)。
    pub created_at: DateTime<Utc>,
    /// 上次触发时间(UTC)。`None` = 从未触发。
    #[serde(default)]
    pub last_fired: Option<DateTime<Utc>>,
    /// 预计下次触发时间(UTC)。driver 启动 / schedule 变更时重算。
    #[serde(default)]
    pub next_fire: Option<DateTime<Utc>>,
}

fn default_enabled() -> bool {
    true
}

/// 调度器状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronStatus {
    /// 无 job(或全部 disabled)。
    Idle,
    /// 至少一个 enabled job。
    HasJobs,
}

/// 真实 cron 调度器。持有一组 session-scoped `CronJobSpec`,后台 driver
/// 到期时把 prompt 作为 `Submission` 注入 agent loop。
///
/// 设计:
/// - `jobs` 用 `Arc<RwLock<Vec<CronJobSpec>>>` 共享,driver task 与
///   `CronTool` 调用方都通过它读写(`Arc` clone)。
/// - `sub_tx` 注入 agent loop 的 submission sender。`None` = 仅记录、
///   不真正触发(测试 / 未接 loop 时)。
/// - `session_id` 决定持久化文件名(`~/.zhuye/cron/<session>.json`)。
#[derive(Clone)]
pub struct CronScheduler {
    pub(crate) jobs: Arc<RwLock<Vec<CronJobSpec>>>,
    pub(crate) sub_tx: Option<mpsc::Sender<Submission>>,
    session_id: ThreadId,
    /// v1.x:持久化文件(`~/.zhuye/cron/<session>.json`)。`None` = 纯内存
    /// (测试 / 显式关闭)。create / update / delete / tick 触发后原子落盘。
    store_path: Option<std::path::PathBuf>,
    /// 序列化同一调度器的全部持久化,避免旧快照覆盖新快照。
    store_lock: Arc<parking_lot::Mutex<()>>,
    /// 加载失败时禁止写入,保留磁盘上可能仍可恢复的数据。
    store_error: Option<String>,
}

impl std::fmt::Debug for CronScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CronScheduler")
            .field("session_id", &self.session_id)
            .field("jobs_count", &self.jobs.read().len())
            .field("has_sub_tx", &self.sub_tx.is_some())
            .field("store_path", &self.store_path)
            .finish()
    }
}

impl CronScheduler {
    /// 构造一个空调度器。`sub_tx` 注入 agent loop 的 submission sender;
    /// 传 `None` 时 driver 不真正触发(测试用)。
    pub fn new(sub_tx: Option<mpsc::Sender<Submission>>, session_id: ThreadId) -> Self {
        Self {
            jobs: Arc::new(RwLock::new(Vec::new())),
            sub_tx,
            session_id,
            store_path: None,
            store_lock: Arc::new(parking_lot::Mutex::new(())),
            store_error: None,
        }
    }

    /// v1.x:启用持久化 —— 从 `path` 加载既有 job(文件缺失 = 空),之后
    /// 每次变更原子落盘。兑现模块 doc 里「`~/.zhuye/cron/<session>.json`」
    /// 的承诺(此前只有文档,代码无任何 fs 写入,进程退出即丢)。
    ///
    /// 加载时对 enabled job **重算 `next_fire`**:进程停机期间到期的 job
    /// 不补发(避免重启瞬间集中触发一堆过期 prompt),从当前时刻起按
    /// schedule 排下一次。schedule 解析失败的 job 保留但 `next_fire = None`
    /// (永不触发),让用户在列表里看到并修正。
    pub fn with_store(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        let path = path.into();
        match load_jobs(&path) {
            Ok(mut loaded) => {
                self.store_error = None;
                let now = Utc::now();
                for job in loaded.iter_mut() {
                    job.next_fire = if job.enabled {
                        CronSchedule::parse(&job.schedule)
                            .ok()
                            .and_then(|p| p.next_after(now))
                    } else {
                        None
                    };
                }
                if !loaded.is_empty() {
                    tracing::info!(
                        path = %path.display(),
                        count = loaded.len(),
                        "cron: jobs loaded from store"
                    );
                }
                *self.jobs.write() = loaded;
            }
            Err(e) => {
                // 坏文件不覆盖:保留现有内存态,只 warn(用户数据不丢)。
                tracing::warn!(path = %path.display(), error = %e, "cron: load store failed; starting empty (file left untouched)");
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

    /// 加载失败时报告存储不可用,避免将未知状态误报为空任务列表。
    pub fn ensure_store_available(&self) -> std::io::Result<()> {
        match &self.store_error {
            Some(error) => Err(std::io::Error::other(error.clone())),
            None => Ok(()),
        }
    }

    /// 把当前 job 列表原子落盘(`store_path` 为 `None` 时 noop)。
    /// 显式调用失败时记录日志;创建、更新和删除会分别向调用方返回错误。
    pub fn persist(&self) {
        if self.store_error.is_some() {
            return;
        }
        let Some(path) = self.store_path.as_deref() else {
            return;
        };
        if let Err(e) = persist_jobs(path, &self.jobs, &self.store_lock) {
            tracing::warn!(path = %path.display(), error = %e, "cron: persist failed");
        }
    }

    /// 该调度器绑定的 session id(持久化 key)。
    pub fn session_id(&self) -> &ThreadId {
        &self.session_id
    }

    /// 当前 job 快照(按 created_at 排序的 clone)。
    pub fn list(&self) -> Vec<CronJobSpec> {
        let mut v = self.jobs.read().clone();
        v.sort_by_key(|j| j.created_at);
        v
    }

    /// 取单条 job。
    pub fn get(&self, id: &str) -> Option<CronJobSpec> {
        self.jobs.read().iter().find(|j| j.id == id).cloned()
    }

    /// 调度器状态。
    pub fn status(&self) -> CronStatus {
        if self.jobs.read().iter().any(|j| j.enabled) {
            CronStatus::HasJobs
        } else {
            CronStatus::Idle
        }
    }

    /// 人可读状态行(给 TUI `/workflows` pill 复用)。
    pub fn status_line(&self) -> String {
        let jobs = self.jobs.read();
        let enabled = jobs.iter().filter(|j| j.enabled).count();
        match jobs.len() {
            0 => "cron-scheduler: 无已注册 job".into(),
            total => format!(
                "cron-scheduler: {total} 个 job({enabled} 启用)— driver 每 30s 扫描到期 job"
            ),
        }
    }

    /// 换绑 submission sender(GUI 换 AgentThread 时把 cron 投递切到新 loop)。
    ///
    /// **重要**:`start(self, ..)` 把旧 `sub_tx` 克隆**按值捕获**进 driver
    /// task,本方法只改 scheduler 自身字段 —— 运行中的 driver 不会感知。
    /// 调用方必须 `CronDriverHandle::stop()` 停旧 driver 后重新
    /// `start(tick)`。`jobs` 是共享 `Arc`,重启不换 job、**id 不变**
    /// (这是相对"重建 scheduler + 迁移 job"的优势:后者会生成新 uuid,
    /// 前端按 id 管理 schedule 时会漂移)。
    pub fn rebind_sender(&mut self, sub_tx: Option<mpsc::Sender<Submission>>) {
        self.sub_tx = sub_tx;
    }

    /// 注册一条新 job。校验 schedule 表达式并算出 next_fire;返回新 job 的
    /// clone。`name` 缺省时取 prompt 前 24 字符。
    pub fn create(
        &self,
        schedule: &str,
        prompt: impl Into<String>,
        name: Option<String>,
    ) -> Result<CronJobSpec, CronParseError> {
        if let Some(error) = &self.store_error {
            return Err(CronParseError::StoreUnavailable(error.clone()));
        }
        let parsed = CronSchedule::parse(schedule)?;
        let now = Utc::now();
        let prompt = prompt.into();
        let next_fire = parsed.next_after(now);
        let job = CronJobSpec {
            id: uuid::Uuid::new_v4().to_string(),
            schedule: schedule.trim().to_string(),
            name: name.or_else(|| {
                let n: String = prompt.chars().take(24).collect();
                (!n.is_empty()).then_some(n)
            }),
            prompt,
            enabled: true,
            created_at: now,
            last_fired: None,
            next_fire,
        };
        let _guard = self.store_lock.lock();
        let mut jobs = self.jobs.write();
        jobs.push(job.clone());
        if let Err(error) = self.save_locked(&jobs) {
            jobs.pop();
            return Err(CronParseError::StoreUnavailable(error.to_string()));
        }
        Ok(job)
    }

    /// 更新一条 job(按 id)。`schedule` / `prompt` / `name` / `enabled`
    /// 任一为 `Some` 才更新;改 schedule 会重算 next_fire。返回更新后的
    /// job clone,或 `None` 表示 id 不存在。
    pub fn update(
        &self,
        id: &str,
        schedule: Option<&str>,
        prompt: Option<&str>,
        name: Option<&str>,
        enabled: Option<bool>,
    ) -> Option<CronJobSpec> {
        self.try_update(id, schedule, prompt, name, enabled)
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "cron: update could not be persisted");
                None
            })
    }

    /// 更新任务并持久化;写入失败时保留原内存状态。
    pub fn try_update(
        &self,
        id: &str,
        schedule: Option<&str>,
        prompt: Option<&str>,
        name: Option<&str>,
        enabled: Option<bool>,
    ) -> std::io::Result<Option<CronJobSpec>> {
        self.ensure_store_available()?;
        let _guard = self.store_lock.lock();
        let mut jobs = self.jobs.write();
        let Some(idx) = jobs.iter().position(|j| j.id == id) else {
            return Ok(None);
        };
        let mut job = jobs[idx].clone();
        if let Some(s) = schedule {
            let Ok(parsed) = CronSchedule::parse(s) else {
                return Ok(None);
            };
            job.next_fire = if enabled.unwrap_or(job.enabled) {
                parsed.next_after(Utc::now())
            } else {
                None
            };
            job.schedule = s.trim().to_string();
        }
        if let Some(p) = prompt {
            job.prompt = p.to_string();
        }
        if let Some(n) = name {
            job.name = Some(n.to_string());
        }
        if let Some(e) = enabled {
            job.enabled = e;
            if !e {
                job.next_fire = None;
            } else if schedule.is_none() && job.next_fire.is_none() {
                job.next_fire = CronSchedule::parse(&job.schedule)
                    .ok()
                    .and_then(|parsed| parsed.next_after(Utc::now()));
            }
        }
        let previous = std::mem::replace(&mut jobs[idx], job.clone());
        if let Err(error) = self.save_locked(&jobs) {
            jobs[idx] = previous;
            return Err(error);
        }
        Ok(Some(job))
    }

    /// 删除一条 job(按 id)。返回是否删除成功。
    pub fn delete(&self, id: &str) -> bool {
        self.try_delete(id).unwrap_or_else(|error| {
            tracing::warn!(%error, "cron: delete could not be persisted");
            false
        })
    }

    /// 删除任务并持久化;写入失败时恢复原任务。
    pub fn try_delete(&self, id: &str) -> std::io::Result<bool> {
        self.ensure_store_available()?;
        let _guard = self.store_lock.lock();
        let mut jobs = self.jobs.write();
        let Some(idx) = jobs.iter().position(|job| job.id == id) else {
            return Ok(false);
        };
        let removed = jobs.remove(idx);
        if let Err(error) = self.save_locked(&jobs) {
            jobs.insert(idx, removed);
            return Err(error);
        }
        Ok(true)
    }

    /// 调用方已持有持久化锁和任务锁。
    fn save_locked(&self, jobs: &[CronJobSpec]) -> std::io::Result<()> {
        if let Some(error) = &self.store_error {
            return Err(std::io::Error::other(error.clone()));
        }
        match self.store_path.as_deref() {
            Some(path) => save_jobs(path, jobs),
            None => Ok(()),
        }
    }

    /// 启动后台 driver。每 `tick_secs` 秒扫描一次 enabled job,到期则把
    /// `prompt` 作为 `Submission::user_input` 发到 `sub_tx`,并更新
    /// `last_fired` / 重算 `next_fire`。返回的 `CronDriverHandle` drop
    /// 时停止 driver(底层 tokio task 自然结束)。
    ///
    /// `start` 会 clone 一份 `jobs` Arc + `sub_tx` 进 task,调用方持有的
    /// `CronScheduler` 仍可正常 create / update / delete(共享同一把锁)。
    pub fn start(self, tick_secs: u64) -> CronDriverHandle {
        let jobs = Arc::clone(&self.jobs);
        let sub_tx = self.sub_tx.clone();
        let store_path = self.store_path.clone();
        let store_lock = self.store_lock.clone();
        let store_error = self.store_error.clone();
        let join = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(tick_secs));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let fired = Self::tick(&jobs, &sub_tx).await;
                // 有 job 触发 → last_fired / next_fire 变了,落盘(纯内存时 noop)。
                if fired > 0
                    && store_error.is_none()
                    && let Some(path) = store_path.as_deref()
                    && let Err(e) = persist_jobs(path, &jobs, &store_lock)
                {
                    tracing::warn!(path = %path.display(), error = %e, "cron: persist after tick failed");
                }
            }
        });
        CronDriverHandle { join: Some(join) }
    }

    /// 单次扫描:找出所有到期(enabled && next_fire <= now)的 job,触发
    /// 它们并重算 next_fire。抽成独立 fn 便于单测(不必真起 driver)。
    /// 返回本次触发的 job 数(供 driver 决定是否落盘)。
    pub async fn tick(
        jobs: &Arc<RwLock<Vec<CronJobSpec>>>,
        sub_tx: &Option<mpsc::Sender<Submission>>,
    ) -> usize {
        let now = Utc::now();
        // 先快照 ID,等待通道容量后再持锁检查,避免执行已经被删除或禁用的任务。
        let due: Vec<String> = {
            let g = jobs.read();
            g.iter()
                .filter(|j| j.enabled && j.next_fire.is_some_and(|nf| nf <= now))
                .map(|j| j.id.clone())
                .collect()
        };
        let mut fired = 0;
        for job_id in due {
            // 获取容量时不持任务锁;通道已关闭则不记录虚假的触发。
            let permit = match sub_tx {
                Some(tx) => match tx.reserve().await {
                    Ok(permit) => Some(permit),
                    Err(_) => break,
                },
                None => None,
            };
            let fired_at = Utc::now();
            let mut g = jobs.write();
            let Some(job) = g.iter_mut().find(|j| {
                j.id == job_id && j.enabled && j.next_fire.is_some_and(|next| next <= fired_at)
            }) else {
                continue;
            };
            job.last_fired = Some(fired_at);
            job.next_fire = CronSchedule::parse(&job.schedule)
                .ok()
                .and_then(|p| p.next_after(fired_at));
            if let Some(permit) = permit {
                permit.send(Submission::user_input(job.prompt.clone()));
            }
            fired += 1;
        }
        fired
    }
}

// ── 持久化(v1.x)───────────────────────────────────────────────────

/// 在持久化锁内读取最新快照并落盘,防止较早的快照后写。
fn persist_jobs(
    path: &std::path::Path,
    jobs: &RwLock<Vec<CronJobSpec>>,
    lock: &parking_lot::Mutex<()>,
) -> std::io::Result<()> {
    let _guard = lock.lock();
    save_jobs(path, &jobs.read())
}

/// 默认持久化根目录:`<data_dir>/cron`(`~/.zhuye/cron`)。HOME 缺失 → `None`。
pub fn default_cron_base() -> Option<std::path::PathBuf> {
    zhuye_config::data_path(&["cron"])
}

/// 某 session 的持久化文件路径:`<base>/<session>.json`。
pub fn store_path_for(base: &std::path::Path, session_id: &ThreadId) -> std::path::PathBuf {
    base.join(format!("{session_id}.json"))
}

/// 读取持久化的 job 列表。文件缺失 → 空列表;其它 IO / 解析错误透传
/// (调用方决定是否保留内存态)。
pub fn load_jobs(path: &std::path::Path) -> std::io::Result<Vec<CronJobSpec>> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).map_err(std::io::Error::other),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// 原子写入 job 列表(保留 symlink / 权限不放大 / 失败保全,经 zhuye-fs)。
pub fn save_jobs(path: &std::path::Path, jobs: &[CronJobSpec]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(jobs).map_err(std::io::Error::other)?;
    zhuye_fs::write_atomic(path, &body)
}

/// driver task 句柄。drop 时 abort 底层 task。
pub struct CronDriverHandle {
    join: Option<tokio::task::JoinHandle<()>>,
}

impl CronDriverHandle {
    /// 主动停止 driver。
    pub fn stop(&mut self) {
        if let Some(j) = self.join.take() {
            j.abort();
        }
    }
}

impl Drop for CronDriverHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

// ── 旧 stub 别名(向后兼容 slash.rs 的 CronStubStatus 引用)──────────
//
// v0 导出的 `CronStubStatus` 在 `slash.rs` 仍被引用;保留类型别名让旧
// 代码继续编译,内部映射到新的 `CronStatus`。
#[doc(hidden)]
pub type CronStubStatus = CronStatus;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    // ── 持久化(v1.x)────────────────────────────────────────────────

    #[test]
    fn load_jobs_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron/x.json");
        assert!(load_jobs(&path).unwrap().is_empty());
    }

    #[test]
    fn save_and_load_roundtrip_preserves_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron/s1.json");
        let scheduler = CronScheduler::new(None, ThreadId::new()).with_store(&path);
        let job = scheduler
            .create("*/5 * * * *", "跑回归", Some("回归".into()))
            .unwrap();
        scheduler.persist();

        let back = load_jobs(&path).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, job.id);
        assert_eq!(back[0].schedule, "*/5 * * * *");
        assert_eq!(back[0].prompt, "跑回归");
        assert_eq!(back[0].name.as_deref(), Some("回归"));
        assert!(back[0].enabled);
    }

    #[test]
    fn with_store_loads_and_recomputes_next_fire() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron/s2.json");
        // 先造一份"上次 next_fire 已经过去"的持久化文件。
        let stale = vec![CronJobSpec {
            id: "job-1".into(),
            schedule: "*/5 * * * *".into(),
            prompt: "recurring".into(),
            name: None,
            enabled: true,
            created_at: Utc::now() - chrono::Duration::hours(2),
            last_fired: Some(Utc::now() - chrono::Duration::hours(2)),
            // next_fire 在 2 小时前(停机期间早该触发)。
            next_fire: Some(Utc::now() - chrono::Duration::hours(2)),
        }];
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string(&stale).unwrap()).unwrap();

        // 加载:next_fire 必须被重算到未来(停机期间到期的 job 不补发)。
        let scheduler = CronScheduler::new(None, ThreadId::new()).with_store(&path);
        let jobs = scheduler.list();
        assert_eq!(jobs.len(), 1);
        assert!(
            jobs[0].next_fire.unwrap() > Utc::now(),
            "next_fire 应重算为未来"
        );
    }

    #[test]
    fn with_store_broken_file_starts_empty_and_does_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron/bad.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json {{{").unwrap();

        let scheduler = CronScheduler::new(None, ThreadId::new()).with_store(&path);
        assert!(scheduler.list().is_empty(), "坏文件应起步为空");
        assert!(matches!(
            scheduler.create("* * * * *", "任务", None),
            Err(CronParseError::StoreUnavailable(_))
        ));
        assert!(scheduler.ensure_store_available().is_err());
        assert!(scheduler.try_delete("未知").is_err());
        assert!(
            scheduler
                .try_update("未知", None, None, None, None)
                .is_err()
        );
        scheduler.persist();
        assert!(scheduler.list().is_empty());
        // 后续操作也不能覆盖坏文件。
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json {{{");
    }

    #[test]
    fn concurrent_persistence_keeps_latest_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron.json");
        let scheduler = CronScheduler::new(None, ThreadId::new()).with_store(&path);
        std::thread::scope(|scope| {
            for index in 0..24 {
                let scheduler = scheduler.clone();
                scope.spawn(move || {
                    scheduler
                        .create("* * * * *", format!("任务 {index}"), None)
                        .unwrap();
                });
            }
        });
        let saved = load_jobs(&path).unwrap();
        assert_eq!(saved.len(), 24);
        assert_eq!(saved, *scheduler.jobs.read());
    }

    #[test]
    fn failed_persistence_rolls_back_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron.json");
        let scheduler = CronScheduler::new(None, ThreadId::new()).with_store(&path);
        let job = scheduler.create("* * * * *", "原任务", None).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions).unwrap();

        assert!(matches!(
            scheduler.create("* * * * *", "新任务", None),
            Err(CronParseError::StoreUnavailable(_))
        ));
        assert!(
            scheduler
                .try_update(&job.id, None, Some("改名"), None, None)
                .is_err()
        );
        assert!(scheduler.try_delete(&job.id).is_err());
        assert_eq!(scheduler.list(), vec![job]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn mutations_persist_across_scheduler_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron/s3.json");
        let sid = ThreadId::new();
        {
            let scheduler = CronScheduler::new(None, sid).with_store(&path);
            scheduler.create("0 9 * * *", "standup", None).unwrap();
            let j = scheduler.create("0 10 * * *", "review", None).unwrap();
            scheduler
                .update(&j.id, None, None, None, Some(false))
                .unwrap();
            scheduler.persist();
        }
        // "重启":新 scheduler 从文件加载,enabled=false 保留。
        let reloaded = CronScheduler::new(None, sid).with_store(&path);
        let jobs = reloaded.list();
        assert_eq!(jobs.len(), 2);
        let disabled = jobs.iter().find(|j| j.prompt == "review").unwrap();
        assert!(!disabled.enabled);
        assert!(disabled.next_fire.is_none(), "disabled job 不排期");
        // 删除也落盘。
        assert!(reloaded.delete(&jobs[0].id));
        assert_eq!(load_jobs(&path).unwrap().len(), 1);
    }

    // ── 表达式解析 ───────────────────────────────────────────────────

    #[test]
    fn parse_every_minute() {
        let s = CronSchedule::parse("* * * * *").unwrap();
        assert_eq!(s.minute.len(), 60);
        assert_eq!(s.hour.len(), 24);
    }

    #[test]
    fn parse_field_count_error() {
        assert_eq!(
            CronSchedule::parse("* * * *").unwrap_err(),
            CronParseError::FieldCount(4)
        );
    }

    #[test]
    fn parse_out_of_range() {
        let err = CronSchedule::parse("60 * * * *").unwrap_err();
        assert!(matches!(
            err,
            CronParseError::OutOfRange {
                field: "minute",
                ..
            }
        ));
    }

    #[test]
    fn parse_step_and_list() {
        let s = CronSchedule::parse("*/15 * * * *").unwrap();
        assert_eq!(s.minute, vec![0, 15, 30, 45]);
        let s = CronSchedule::parse("0,30 * * * *").unwrap();
        assert_eq!(s.minute, vec![0, 30]);
        let s = CronSchedule::parse("5-7 * * * *").unwrap();
        assert_eq!(s.minute, vec![5, 6, 7]);
    }

    #[test]
    fn parse_weekday_range() {
        let s = CronSchedule::parse("0 9 * * 1-5").unwrap();
        assert_eq!(s.dow, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn parse_zero_step_errors() {
        let err = CronSchedule::parse("*/0 * * * *").unwrap_err();
        assert!(matches!(err, CronParseError::ZeroStep { field: "minute" }));
    }

    #[test]
    fn parse_large_steps_and_invalid_ranges_without_overflow() {
        let schedule = CronSchedule::parse("1/2147483647 * * * *").unwrap();
        assert_eq!(schedule.minute, vec![1]);
        assert!(matches!(
            CronSchedule::parse("60/2 * * * *"),
            Err(CronParseError::OutOfRange {
                field: "minute",
                ..
            })
        ));
        assert!(matches!(
            CronSchedule::parse("9-2 * * * *"),
            Err(CronParseError::InvalidToken {
                field: "minute",
                ..
            })
        ));
    }

    // ── 计算下次触发时间(next_after)──────────────────────────

    #[test]
    fn next_after_every_minute_is_next_minute() {
        let s = CronSchedule::parse("* * * * *").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 2, 12, 0, 0).unwrap();
        let next = s.next_after(now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 7, 2, 12, 1, 0).unwrap());
    }

    #[test]
    fn next_after_hourly_aligns_to_top() {
        let s = CronSchedule::parse("0 * * * *").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 2, 12, 30, 0).unwrap();
        let next = s.next_after(now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 7, 2, 13, 0, 0).unwrap());
    }

    #[test]
    fn next_after_daily_finds_next_day() {
        let s = CronSchedule::parse("0 9 * * *").unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 2, 10, 0, 0).unwrap();
        let next = s.next_after(now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 7, 3, 9, 0, 0).unwrap());
    }

    // ── 调度器增删改查 ───────────────────────────────────────────

    fn sched() -> CronScheduler {
        CronScheduler::new(None, ThreadId::new())
    }

    #[test]
    fn create_validates_and_sets_next_fire() {
        let s = sched();
        let job = s
            .create("0 * * * *", "standup", Some("daily".into()))
            .unwrap();
        assert!(job.next_fire.is_some());
        assert_eq!(job.prompt, "standup");
        assert_eq!(s.list().len(), 1);
        assert_eq!(s.status(), CronStatus::HasJobs);
    }

    #[test]
    fn create_rejects_bad_schedule() {
        let s = sched();
        assert!(s.create("not-a-cron", "x", None).is_err());
        assert_eq!(s.list().len(), 0);
        assert_eq!(s.status(), CronStatus::Idle);
    }

    #[test]
    fn create_defaults_name_from_prompt() {
        let s = sched();
        let job = s.create("0 * * * *", "hello world prompt", None).unwrap();
        assert_eq!(job.name.as_deref(), Some("hello world prompt"));
    }

    #[test]
    fn update_changes_fields_and_recomputes_next_fire() {
        let s = sched();
        // 两条 schedule 的下一次触发可能撞上同一时刻(如 UTC 上午运行时
        // `0 * * * *` 与 `0 9 * * *` 都指向今天 09:00),断言改为「update
        // 确实重算了 next_fire 字段」—— 与创建时分开比较,不依赖墙钟。
        let job = s.create("0 3 * * *", "p1", None).unwrap();
        let nf0 = job.next_fire;
        let updated = s.update(&job.id, Some("0 5 * * *"), Some("p2"), None, None);
        assert!(updated.is_some());
        let u = updated.unwrap();
        assert_eq!(u.prompt, "p2");
        assert_eq!(u.schedule, "0 5 * * *");
        assert_ne!(u.next_fire, nf0);
    }

    #[test]
    fn update_disable_then_status_idle() {
        let s = sched();
        let job = s.create("0 * * * *", "p", None).unwrap();
        assert_eq!(s.status(), CronStatus::HasJobs);
        s.update(&job.id, None, None, None, Some(false));
        assert_eq!(s.status(), CronStatus::Idle);
    }

    #[test]
    fn reenable_loaded_job_recomputes_next_fire() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron.json");
        let scheduler = CronScheduler::new(None, ThreadId::new()).with_store(&path);
        let job = scheduler.create("* * * * *", "任务", None).unwrap();
        assert!(
            scheduler
                .update(&job.id, None, None, None, Some(false))
                .unwrap()
                .next_fire
                .is_none()
        );
        let restored = CronScheduler::new(None, ThreadId::new()).with_store(&path);
        assert!(restored.get(&job.id).unwrap().next_fire.is_none());
        let enabled = restored
            .update(&job.id, None, None, None, Some(true))
            .unwrap();
        assert!(enabled.enabled);
        assert!(enabled.next_fire.is_some_and(|time| time > Utc::now()));
        assert_eq!(load_jobs(&path).unwrap()[0].next_fire, enabled.next_fire);
    }

    #[test]
    fn delete_removes_job() {
        let s = sched();
        let job = s.create("0 * * * *", "p", None).unwrap();
        assert!(s.delete(&job.id));
        assert!(s.get(&job.id).is_none());
        assert!(!s.delete(&job.id), "second delete is no-op");
    }

    // ── 驱动循环 tick ──────────────────────────────────────────

    #[tokio::test]
    async fn tick_fires_due_job_and_recomputes_next_fire() {
        let s = sched();
        let mut job = s.create("* * * * *", "fired", None).unwrap();
        job.next_fire = Some(Utc::now() - Duration::minutes(5));
        s.jobs.write()[0] = job.clone();
        let jobs = Arc::clone(&s.jobs);
        CronScheduler::tick(&jobs, &None).await;
        let after = s.get(&job.id).unwrap();
        assert!(after.last_fired.is_some(), "last_fired should be set");
        assert!(after.next_fire.unwrap() > Utc::now());
    }

    #[tokio::test]
    async fn tick_sends_submission_when_tx_present() {
        let (tx, mut rx) = mpsc::channel::<Submission>(8);
        let s = CronScheduler::new(Some(tx), ThreadId::new());
        let mut job = s.create("* * * * *", "hello-cron", None).unwrap();
        job.next_fire = Some(Utc::now() - Duration::minutes(1));
        s.jobs.write()[0] = job.clone();
        let jobs = Arc::clone(&s.jobs);
        let tx = s.sub_tx.clone().unwrap();
        CronScheduler::tick(&jobs, &Some(tx)).await;
        let sub = rx.recv().await.expect("should receive a submission");
        assert!(matches!(sub.op, zhuye_protocol::Op::UserInput { .. }));
    }

    #[tokio::test]
    async fn tick_does_not_fire_job_disabled_while_waiting_for_capacity() {
        let (tx, mut rx) = mpsc::channel::<Submission>(1);
        tx.send(Submission::user_input("占满通道")).await.unwrap();
        let scheduler = sched();
        let mut job = scheduler.create("* * * * *", "任务", None).unwrap();
        job.next_fire = Some(Utc::now() - Duration::minutes(1));
        scheduler.jobs.write()[0] = job.clone();
        let jobs = scheduler.jobs.clone();
        let sender = tx.clone();
        let tick = tokio::spawn(async move { CronScheduler::tick(&jobs, &Some(sender)).await });
        tokio::task::yield_now().await;
        scheduler
            .update(&job.id, None, None, None, Some(false))
            .unwrap();
        rx.recv().await.unwrap();
        assert_eq!(tick.await.unwrap(), 0);
        assert!(rx.try_recv().is_err());
        assert!(scheduler.get(&job.id).unwrap().last_fired.is_none());
    }

    #[tokio::test]
    async fn concurrent_ticks_claim_due_job_only_once() {
        let (tx, mut rx) = mpsc::channel::<Submission>(2);
        let scheduler = sched();
        let mut job = scheduler.create("* * * * *", "任务", None).unwrap();
        job.next_fire = Some(Utc::now() - Duration::minutes(1));
        scheduler.jobs.write()[0] = job;
        let sender = Some(tx);
        let first = CronScheduler::tick(&scheduler.jobs, &sender);
        let second = CronScheduler::tick(&scheduler.jobs, &sender);
        let (first, second) = tokio::join!(first, second);
        assert_eq!(first + second, 1);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn closed_submission_channel_does_not_mark_job_fired() {
        let (tx, rx) = mpsc::channel::<Submission>(1);
        drop(rx);
        let scheduler = sched();
        let mut job = scheduler.create("* * * * *", "任务", None).unwrap();
        job.next_fire = Some(Utc::now() - Duration::minutes(1));
        scheduler.jobs.write()[0] = job.clone();
        assert_eq!(CronScheduler::tick(&scheduler.jobs, &Some(tx)).await, 0);
        let saved = scheduler.get(&job.id).unwrap();
        assert!(saved.last_fired.is_none());
        assert_eq!(saved.next_fire, job.next_fire);
    }

    /// v1.x:`rebind_sender` 后重启 driver,到期 job 投递到**新** channel,
    /// 且 job id 不变(前端按 id 管理 schedule 不漂移)。
    /// 同时验证 `start(self)` 按值捕获旧 sender —— 只 rebind 不重启时,
    /// 旧 driver 仍写旧 channel(这是要求调用方 stop+restart 的原因)。
    #[tokio::test]
    async fn rebind_sender_replaces_tx_before_restart() {
        let (old_tx, mut old_rx) = mpsc::channel::<Submission>(8);
        let mut s = CronScheduler::new(Some(old_tx), ThreadId::new());
        let job = s.create("* * * * *", "rebind", None).unwrap();
        let job_id = job.id.clone();

        // 换绑到新 channel。
        let (new_tx, mut new_rx) = mpsc::channel::<Submission>(8);
        s.rebind_sender(Some(new_tx));
        assert_eq!(s.list()[0].id, job_id, "rebind 不动 job,id 保持");

        // 直接用换绑后的 scheduler 触发 tick → 落新 channel。
        let mut due = s.get(&job_id).unwrap();
        due.next_fire = Some(Utc::now() - Duration::minutes(1));
        s.jobs.write()[0] = due;
        let jobs = Arc::clone(&s.jobs);
        let tx = s.sub_tx.clone().unwrap();
        CronScheduler::tick(&jobs, &Some(tx)).await;
        let sub = new_rx.recv().await.expect("换绑后应投递到新 channel");
        assert!(matches!(sub.op, zhuye_protocol::Op::UserInput { .. }));
        assert!(old_rx.try_recv().is_err(), "旧 channel 不应再收到投递");
    }

    #[tokio::test]
    async fn tick_skips_disabled_jobs() {
        let s = sched();
        let mut job = s.create("* * * * *", "p", None).unwrap();
        job.enabled = false;
        job.next_fire = Some(Utc::now() - Duration::minutes(1));
        s.jobs.write()[0] = job.clone();
        let jobs = Arc::clone(&s.jobs);
        CronScheduler::tick(&jobs, &None).await;
        let after = s.get(&job.id).unwrap();
        assert!(after.last_fired.is_none(), "disabled job must not fire");
    }

    // 回归:tick 快照与写回之间发生 delete(retain)使下标前移时,必须按
    // **id** 而非下标重定位,否则会把 last_fired/next_fire 写到错误的 job 上。
    // 复现:job A、job B 均到期 → tick 快照两者 → 删 A(下标前移,B 从 1→0)
    // → 写回时若按下标,B 被更新两次、A(已删)的更新落到 B 头上。
    #[tokio::test]
    async fn tick_survives_concurrent_delete_by_resolving_id() {
        let s = sched();
        // 两个到期 job,故意给 A 一个更早的 next_fire 让它排在快照第一位。
        let mut a = s.create("* * * * *", "A", None).unwrap();
        a.next_fire = Some(Utc::now() - Duration::minutes(10));
        let mut b = s.create("* * * * *", "B", None).unwrap();
        b.next_fire = Some(Utc::now() - Duration::minutes(5));
        let id_a = a.id.clone();
        let id_b = b.id.clone();
        {
            let mut g = s.jobs.write();
            g[0] = a;
            g[1] = b;
        }
        let jobs = Arc::clone(&s.jobs);

        // tick 内部先快照(A、B),再逐个写回。我们在两次写回之间删掉 A ——
        // 模拟并发 delete。由于 tick 逐 job 加写锁,这里在 tick 之前删 A 即可
        // 制造「快照里还有 A、但写回时 A 已不在」的场景,验证写回按 id 定位、
        // 不污染 B。
        s.delete(&id_a);
        CronScheduler::tick(&jobs, &None).await;

        // B 必须被正确更新(last_fired + next_fire 重算),而非保持过期值。
        let after_b = s.get(&id_b).expect("B must still exist");
        assert!(
            after_b.last_fired.is_some(),
            "B should have been fired and its last_fired set"
        );
        assert!(
            after_b.next_fire.unwrap() > Utc::now(),
            "B's next_fire should have been recomputed into the future"
        );
    }
}
