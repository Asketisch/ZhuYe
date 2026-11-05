//! Autopilot —— 定时驱动 agent 跑任务的闭环(WP11 收编 ReflectDesktop
//! `app-core/src/autopilot.rs`)。
//!
//! 桌面端只有 config / history 两个 JSON 文件的 CRUD,模块 doc 自述
//! 「尚未接入自动调度或任务执行」(`should_run()` 只是
//! `enabled && !schedule.is_empty()` 的布尔摆设)。归位到框架后补齐
//! **真闭环**:
//!
//! - `schedule` 字段统一走本 crate 的 [`crate::cron::CronSchedule`]
//!   (同一套 5 字段解析,不再各写一套 cron 口径);
//! - [`AutopilotManager::should_run`] 按 cron 到期判定:距上次触发
//!   (`history[0].executed_at_ms`,无历史取配置文件修改时间为起点)的
//!   `next_after` 已过 → 到期;
//! - [`fire_if_due`] 把 `task_template` 作为
//!   `Submission::user_input` 注入 agent loop 并落一条 `InProgress`
//!   运行记录;调用方(cron driver / bootstrap)负责在任务终态时
//!   [`AutopilotManager::finalize_run`]。
//!
//! 存储沿用桌面端语义:`<root>/config.json` + `<root>/history.json`
//! (最多保留最近 100 条),写入走 `zhuye_fs::write_atomic`。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use zhuye_protocol::Submission;

use crate::cron::CronSchedule;

/// 历史上限(最旧的记录被截断丢弃)。
pub const HISTORY_CAP: usize = 100;

/// Autopilot 配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutopilotConfig {
    /// 是否启用。
    pub enabled: bool,
    /// 触发调度的 5 字段 cron 表达式(与本 crate cron 同一口径)。
    pub schedule: String,
    /// 触发时注入 agent loop 的模板 prompt。
    pub task_template: String,
    /// 执行任务所用的 agent(可选;None 用默认)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// 最大并发数(超出时 [`AutopilotRunStatus::Skipped`])。
    pub max_concurrent: usize,
}

impl Default for AutopilotConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            schedule: String::new(),
            task_template: String::new(),
            agent: None,
            max_concurrent: 1,
        }
    }
}

/// 一次运行的历史记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutopilotRun {
    /// 唯一运行 id(uuid)。
    pub id: String,
    /// 触发时间(UNIX epoch 毫秒)。
    pub executed_at_ms: u64,
    /// 创建的任务 id(可选)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// 状态。
    pub status: AutopilotRunStatus,
    /// 可选错误消息。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 运行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AutopilotRunStatus {
    /// 成功完成。
    Success,
    /// 失败。
    Failed,
    /// 进行中(触发即记录,终态由调用方回填)。
    InProgress,
    /// 已跳过(达最大并发 / 未到期等)。
    Skipped,
}

/// 配置与历史的管理器。
pub struct AutopilotManager {
    config_path: PathBuf,
    history_path: PathBuf,
    history_lock: parking_lot::Mutex<()>,
}

impl Default for AutopilotManager {
    fn default() -> Self {
        Self::new()
    }
}

impl AutopilotManager {
    /// 用默认数据目录构造(`<data_dir>/autopilot`,`~/.zhuye/autopilot`)。
    pub fn new() -> Self {
        let root = zhuye_config::data_path_or_relative(&["autopilot"]);
        Self::new_with_root(root)
    }

    /// 用指定根目录构造(测试友好;`root` 不需预先存在)。
    pub fn new_with_root(root: PathBuf) -> Self {
        Self {
            config_path: root.join("config.json"),
            history_path: root.join("history.json"),
            history_lock: parking_lot::Mutex::new(()),
        }
    }

    /// 加载配置;文件不存在用默认;坏文件报 `InvalidData`(不静默重置,
    /// 用户数据不丢)。
    pub fn load_config(&self) -> io::Result<AutopilotConfig> {
        read_json_or_default(&self.config_path)
    }

    /// 保存配置(原子写;父目录缺失时创建 —— 桌面端语义「saves create
    /// missing directory」)。
    pub fn save_config(&self, config: &AutopilotConfig) -> io::Result<()> {
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(config)?;
        zhuye_fs::write_atomic(&self.config_path, content.as_bytes())
    }

    /// 加载运行历史(最新在前)。
    pub fn load_history(&self) -> io::Result<Vec<AutopilotRun>> {
        let _guard = self.history_lock.lock();
        read_json_or_default(&self.history_path)
    }

    /// 保存运行历史(原子写)。
    pub fn save_history(&self, history: &[AutopilotRun]) -> io::Result<()> {
        let _guard = self.history_lock.lock();
        self.write_history(history)
    }

    /// 追加一次新运行(插到头部,截断到 [`HISTORY_CAP`])。
    pub fn add_run(&self, run: AutopilotRun) -> io::Result<()> {
        let _guard = self.history_lock.lock();
        let mut history: Vec<AutopilotRun> = read_json_or_default(&self.history_path)?;
        history.insert(0, run);
        if history.len() > HISTORY_CAP {
            history.truncate(HISTORY_CAP);
        }
        self.write_history(&history)
    }

    /// 终态回填:把 `run_id` 的状态从 `InProgress` 改为 `status`。
    /// 找不到(已被截断 / 未知 id)→ `Ok(false)`。
    pub fn finalize_run(
        &self,
        run_id: &str,
        status: AutopilotRunStatus,
        error: Option<String>,
    ) -> io::Result<bool> {
        let _guard = self.history_lock.lock();
        let mut history: Vec<AutopilotRun> = read_json_or_default(&self.history_path)?;
        let Some(run) = history
            .iter_mut()
            .find(|r| r.id == run_id && r.status == AutopilotRunStatus::InProgress)
        else {
            return Ok(false);
        };
        run.status = status;
        run.error = error;
        self.write_history(&history)?;
        Ok(true)
    }

    /// 按 cron 到期判定是否应执行(真闭环,取代桌面端的布尔摆设):
    /// enabled + schedule 可解析 + 距上次触发的 next_after 已过。
    ///
    /// 无历史记录时以**配置文件修改时间**为起点(首次保存配置后不会立即连发,
    /// 从下一个 schedule 点开始)。配置 / 历史文件坏 → 透传 `InvalidData`。
    pub fn should_run(&self) -> io::Result<bool> {
        let config = self.load_config()?;
        if !config.enabled || config.schedule.is_empty() {
            return Ok(false);
        }
        let Ok(parsed) = CronSchedule::parse(&config.schedule) else {
            return Ok(false);
        };
        let history = self.load_history()?;
        self.is_due(&parsed, &history)
    }

    fn is_due(&self, schedule: &CronSchedule, history: &[AutopilotRun]) -> io::Result<bool> {
        let last = match history.first() {
            Some(run) => ms_to_datetime(run.executed_at_ms),
            None => self.config_path.metadata()?.modified()?.into(),
        };
        Ok(schedule
            .next_after(last)
            .is_some_and(|next| next <= Utc::now()))
    }

    fn reserve_due_run(&self) -> io::Result<Option<(AutopilotConfig, AutopilotRun)>> {
        let _guard = self.history_lock.lock();
        let config = self.load_config()?;
        if !config.enabled || config.schedule.is_empty() {
            return Ok(None);
        }
        let Ok(schedule) = CronSchedule::parse(&config.schedule) else {
            return Ok(None);
        };
        let mut history: Vec<AutopilotRun> = read_json_or_default(&self.history_path)?;
        if !self.is_due(&schedule, &history)? {
            return Ok(None);
        }
        let active = history
            .iter()
            .filter(|r| r.status == AutopilotRunStatus::InProgress)
            .count();
        let run = AutopilotRun {
            id: uuid::Uuid::new_v4().to_string(),
            executed_at_ms: now_ms(),
            task_id: None,
            status: if active >= config.max_concurrent {
                AutopilotRunStatus::Skipped
            } else {
                AutopilotRunStatus::InProgress
            },
            error: None,
        };
        history.insert(0, run.clone());
        history.truncate(HISTORY_CAP);
        self.write_history(&history)?;
        Ok(Some((config, run)))
    }

    fn write_history(&self, history: &[AutopilotRun]) -> io::Result<()> {
        if let Some(parent) = self.history_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_vec_pretty(history)?;
        zhuye_fs::write_atomic(&self.history_path, &content)
    }
}

/// 真闭环执行:到期则把 `task_template` 注入 `sub_tx` 并记录
/// `InProgress` 运行。返回触发的运行记录;未到期 / 未启用 → `None`。
///
/// 并发保护:同一 manager 串行(内部锁),多调用方共享 `Arc` 时由
/// 检查到期与占位在同一把锁内完成,避免双发。
pub async fn fire_if_due(
    manager: &AutopilotManager,
    sub_tx: &Option<tokio::sync::mpsc::Sender<Submission>>,
) -> io::Result<Option<AutopilotRun>> {
    let Some((config, mut run)) = manager.reserve_due_run()? else {
        return Ok(None);
    };
    if run.status == AutopilotRunStatus::InProgress {
        let sent = match sub_tx {
            Some(tx) => tx
                .send(Submission::user_input(config.task_template))
                .await
                .is_ok(),
            None => false,
        };
        if !sent {
            let error = "autopilot submission channel unavailable".to_string();
            manager.finalize_run(&run.id, AutopilotRunStatus::Failed, Some(error.clone()))?;
            run.status = AutopilotRunStatus::Failed;
            run.error = Some(error);
        }
    }
    Ok(Some(run))
}

/// 便捷构造:绑定 session 的共享 manager + 提交通道,返回可直接交给
/// 周期 driver(cron driver / tokio interval)的闭包形态。
pub fn shared_manager(root: PathBuf) -> Arc<AutopilotManager> {
    Arc::new(AutopilotManager::new_with_root(root))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn ms_to_datetime(ms: u64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms as i64)
        .single()
        .unwrap_or_else(Utc::now)
}

fn read_json_or_default<T: serde::de::DeserializeOwned + Default>(path: &Path) -> io::Result<T> {
    match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {error}", path.display()),
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// 每个测试用独立 tempdir,避免污染真实 `~/.zhuye/autopilot/`。
    fn fresh_manager() -> AutopilotManager {
        let dir = TempDir::new().expect("tempdir");
        // 泄漏 tempdir 使文件在测试期间存活(测试进程退出时回收)。
        AutopilotManager::new_with_root(dir.keep())
    }

    #[test]
    fn default_config_is_disabled() {
        let config = AutopilotConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.max_concurrent, 1);
    }

    #[test]
    fn load_config_creates_default() {
        let config = fresh_manager().load_config().unwrap();
        assert!(!config.enabled);
    }

    #[test]
    fn save_and_load_config_roundtrip() {
        let manager = fresh_manager();
        let config = AutopilotConfig {
            enabled: true,
            schedule: "0 * * * *".to_string(),
            task_template: "跑回归".into(),
            ..AutopilotConfig::default()
        };
        manager.save_config(&config).unwrap();
        let loaded = manager.load_config().unwrap();
        assert!(loaded.enabled);
        assert_eq!(loaded.schedule, "0 * * * *");
        assert_eq!(loaded.task_template, "跑回归");
    }

    #[test]
    fn history_starts_empty_and_saves_create_missing_directory() {
        let dir = TempDir::new().unwrap();
        let manager = AutopilotManager::new_with_root(dir.path().join("missing"));
        assert!(manager.load_history().unwrap().is_empty());
        manager.save_config(&AutopilotConfig::default()).unwrap();
        manager.save_history(&[]).unwrap();
        assert!(!manager.load_config().unwrap().enabled);
        assert!(manager.load_history().unwrap().is_empty());
    }

    #[test]
    fn history_truncates_to_cap() {
        let manager = fresh_manager();
        for i in 0..(HISTORY_CAP + 10) {
            manager
                .add_run(AutopilotRun {
                    id: i.to_string(),
                    executed_at_ms: i as u64,
                    task_id: None,
                    status: AutopilotRunStatus::Success,
                    error: None,
                })
                .unwrap();
        }
        let history = manager.load_history().unwrap();
        assert_eq!(history.len(), HISTORY_CAP);
        // 最新在前:最后一次插入的 id 在头部。
        assert_eq!(history[0].id, (HISTORY_CAP + 9).to_string());
    }

    #[test]
    fn concurrent_runs_are_all_recorded() {
        let dir = TempDir::new().unwrap();
        let manager = Arc::new(AutopilotManager::new_with_root(dir.path().to_path_buf()));
        let workers: Vec<_> = (0..40)
            .map(|index| {
                let manager = manager.clone();
                std::thread::spawn(move || {
                    manager
                        .add_run(AutopilotRun {
                            id: index.to_string(),
                            executed_at_ms: index,
                            task_id: None,
                            status: AutopilotRunStatus::Success,
                            error: None,
                        })
                        .unwrap()
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let history = manager.load_history().unwrap();
        let ids: std::collections::HashSet<_> = history.iter().map(|r| &r.id).collect();
        assert_eq!(history.len(), 40);
        assert_eq!(ids.len(), 40);
    }

    #[cfg(unix)]
    #[test]
    fn save_preserves_symlink_and_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = TempDir::new().unwrap();
        let manager = AutopilotManager::new_with_root(dir.path().to_path_buf());
        let target = dir.path().join("actual.json");
        let link = dir.path().join("config.json");
        std::fs::write(&target, "{}").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        symlink("actual.json", &link).unwrap();
        manager.save_config(&AutopilotConfig::default()).unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(!manager.load_config().unwrap().enabled);
    }

    #[test]
    fn malformed_files_reported_and_history_not_overwritten() {
        let dir = TempDir::new().unwrap();
        let manager = AutopilotManager::new_with_root(dir.path().to_path_buf());
        std::fs::write(dir.path().join("config.json"), "invalid config").unwrap();
        let history = dir.path().join("history.json");
        std::fs::write(&history, "invalid history").unwrap();
        assert_eq!(
            manager.load_config().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        // should_run 读 config,坏文件透传(不静默当作禁用)。
        assert!(matches!(
            manager.should_run(),
            Err(e) if e.kind() == io::ErrorKind::InvalidData
        ));
        assert_eq!(
            manager.load_history().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(
            manager
                .add_run(AutopilotRun {
                    id: "run".into(),
                    executed_at_ms: 0,
                    task_id: None,
                    status: AutopilotRunStatus::Success,
                    error: None,
                })
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(&history).unwrap(),
            "invalid history"
        );
    }

    // ── 真闭环(v1.x)────────────────────────────────────────────────

    #[test]
    fn should_run_respects_enabled_and_cron_due() {
        let manager = fresh_manager();
        let config = AutopilotConfig {
            enabled: false,
            schedule: "* * * * *".into(),
            task_template: "t".into(),
            ..AutopilotConfig::default()
        };
        manager.save_config(&config).unwrap();
        assert!(!manager.should_run().unwrap(), "禁用 → false");

        let mut enabled = config.clone();
        enabled.enabled = true;
        manager.save_config(&enabled).unwrap();
        // 首次无历史 → 以配置修改时间为起点,每分钟 schedule 的下一次还没到。
        assert!(!manager.should_run().unwrap(), "未到期 → false");

        // 伪造 2 小时前的一次历史 → next_after(last) 已过 → 到期。
        manager
            .add_run(AutopilotRun {
                id: "seed".into(),
                executed_at_ms: (Utc::now() - chrono::Duration::hours(2)).timestamp_millis() as u64,
                task_id: None,
                status: AutopilotRunStatus::Success,
                error: None,
            })
            .unwrap();
        assert!(manager.should_run().unwrap(), "距上次触发超一个周期 → true");
    }

    #[tokio::test]
    async fn first_run_and_concurrent_reservations() {
        let dir = TempDir::new().unwrap();
        let manager = Arc::new(AutopilotManager::new_with_root(dir.path().to_path_buf()));
        manager
            .save_config(&AutopilotConfig {
                enabled: true,
                schedule: "* * * * *".into(),
                task_template: "首次任务".into(),
                ..AutopilotConfig::default()
            })
            .unwrap();
        let config_file = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.path().join("config.json"))
            .unwrap();
        config_file
            .set_times(
                std::fs::FileTimes::new().set_modified(
                    std::time::SystemTime::now() - std::time::Duration::from_secs(120),
                ),
            )
            .unwrap();
        assert!(manager.should_run().unwrap());
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let manager = Arc::clone(&manager);
            let tx = tx.clone();
            tasks.push(tokio::spawn(async move {
                fire_if_due(&manager, &Some(tx)).await.unwrap()
            }));
        }
        let mut fired = 0;
        for task in tasks {
            fired += usize::from(task.await.unwrap().is_some());
        }
        assert_eq!(fired, 1);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
        assert_eq!(manager.load_history().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn concurrent_limit_records_skipped_run() {
        let manager = fresh_manager();
        manager
            .save_config(&AutopilotConfig {
                enabled: true,
                schedule: "* * * * *".into(),
                task_template: "任务".into(),
                max_concurrent: 1,
                ..AutopilotConfig::default()
            })
            .unwrap();
        manager
            .add_run(AutopilotRun {
                id: "active".into(),
                executed_at_ms: 0,
                task_id: None,
                status: AutopilotRunStatus::InProgress,
                error: None,
            })
            .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let run = fire_if_due(&manager, &Some(tx)).await.unwrap().unwrap();
        assert_eq!(run.status, AutopilotRunStatus::Skipped);
        assert!(rx.try_recv().is_err());
        assert_eq!(
            manager.load_history().unwrap()[0].status,
            AutopilotRunStatus::Skipped
        );
    }

    #[tokio::test]
    async fn unavailable_channel_marks_run_failed() {
        let manager = fresh_manager();
        manager
            .save_config(&AutopilotConfig {
                enabled: true,
                schedule: "* * * * *".into(),
                task_template: "任务".into(),
                ..AutopilotConfig::default()
            })
            .unwrap();
        manager
            .add_run(AutopilotRun {
                id: "seed".into(),
                executed_at_ms: 0,
                task_id: None,
                status: AutopilotRunStatus::Success,
                error: None,
            })
            .unwrap();
        let run = fire_if_due(&manager, &None).await.unwrap().unwrap();
        assert_eq!(run.status, AutopilotRunStatus::Failed);
        assert_eq!(
            manager.load_history().unwrap()[0].status,
            AutopilotRunStatus::Failed
        );
    }

    #[test]
    fn should_run_false_for_unparseable_schedule() {
        let manager = fresh_manager();
        manager
            .save_config(&AutopilotConfig {
                enabled: true,
                schedule: "not a cron".into(),
                task_template: "t".into(),
                ..AutopilotConfig::default()
            })
            .unwrap();
        assert!(!manager.should_run().unwrap(), "解析失败不触发(也不报错)");
    }

    #[tokio::test]
    async fn fire_if_due_injects_submission_and_records_run() {
        let dir = TempDir::new().unwrap();
        let manager = Arc::new(AutopilotManager::new_with_root(dir.path().to_path_buf()));
        manager
            .save_config(&AutopilotConfig {
                enabled: true,
                schedule: "* * * * *".into(),
                task_template: "自动回归".into(),
                ..AutopilotConfig::default()
            })
            .unwrap();
        // 伪造 2 小时前的历史 → 到期。
        manager
            .add_run(AutopilotRun {
                id: "seed".into(),
                executed_at_ms: (Utc::now() - chrono::Duration::hours(2)).timestamp_millis() as u64,
                task_id: None,
                status: AutopilotRunStatus::Success,
                error: None,
            })
            .unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let run = fire_if_due(&manager, &Some(tx))
            .await
            .unwrap()
            .expect("应触发");
        assert_eq!(run.status, AutopilotRunStatus::InProgress);
        let injected = rx.recv().await.expect("应注入 submission");
        assert_eq!(injected.op.discriminant(), "user_input");
        // 历史头部是本次 InProgress 运行。
        let head = manager.load_history().unwrap().remove(0);
        assert_eq!(head.id, run.id);

        // 终态回填。
        assert!(
            manager
                .finalize_run(&run.id, AutopilotRunStatus::Success, None)
                .unwrap()
        );
        assert_eq!(
            manager.load_history().unwrap()[0].status,
            AutopilotRunStatus::Success
        );
        // 再触发会被防住(next_after 刚重置)。
        assert!(fire_if_due(&manager, &None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn finalize_run_unknown_id_returns_false() {
        let manager = fresh_manager();
        assert!(
            !manager
                .finalize_run("ghost", AutopilotRunStatus::Failed, None)
                .unwrap()
        );
    }
}
