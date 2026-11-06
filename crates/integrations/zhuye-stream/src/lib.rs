//! `zhuye-stream` — 流式会话与调度后端。
//!
//! - `postgres_session`: PostgreSQL `RolloutRecorder` trait 扩展
//! - `sse_redis`: SSE + Redis 回放 stub
//! - `workspace_sync`: Git clone/sync
//! - `cron`: v1.2 真实定时调度(driver + 5 字段表达式解析)
//! - `queue`: v1.10 队列模式(session 级 FIFO 任务队列,事件驱动串行消费)

pub mod autopilot;
pub mod cron;
pub mod postgres_session;
pub mod queue;
pub mod sse_redis;
pub mod workspace_sync;

pub use autopilot::{
    AutopilotConfig, AutopilotManager, AutopilotRun, AutopilotRunStatus, HISTORY_CAP,
};
pub use cron::{
    CronDriverHandle, CronJobSpec, CronParseError, CronSchedule, CronScheduler, CronStatus,
    CronStubStatus, default_cron_base, load_jobs, save_jobs, store_path_for,
};
pub use postgres_session::{PostgresSessionConfig, PostgresSessionStore, StubPostgresSessionStore};
pub use queue::{
    QueueCounts, QueueDriverHandle, QueueScheduler, QueueState, QueueStatusSummary, QueueTask,
    QueueTaskStatus, default_queue_base, load_state, queue_store_path_for, save_state,
};
pub use sse_redis::{SseRedisBackend, SseRedisConfig, SseRedisStubStatus};
pub use workspace_sync::{WorkspaceSyncOptions, clone_repo, sync_status_line};
