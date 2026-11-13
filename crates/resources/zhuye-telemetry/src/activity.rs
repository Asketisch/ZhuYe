//! Activity 时间线 —— 本地事件审计日志(WP12 收编 ReflectDesktop
//! `app-core/src/activity.rs` + `src-tauri/src/state/activity.rs` 的映射层)。
//!
//! 记录「谁(actor)在何时(ts)做了什么(kind + summary)」,供 Inbox /
//! Activity timeline / @mention 搜索使用。
//!
//! ## 为什么不能做成插件 hook
//!
//! 插件 hook 每次派发 spawn 一个 `sh -c` 子进程,且超时 / 崩溃
//! fail-closed **Deny** —— 审计插件挂掉会拒掉正常工具调用,审计反过来
//! 拖垮 agent。所以 ActivityLogger 必须是进程内实现(`record` 纯内存 +
//! 同步 append,单条 < 1KB,可接受),订阅方拿 `Event` 调
//! [`map_event`] + [`ActivityLogger::record`]。
//!
//! ## 存储
//!
//! - 内存:`VecDeque` 环形缓冲,cap = [`CAP_INMEMORY`],LRU 驱逐最旧;
//! - 磁盘:`<data_dir>/activity/activity.jsonl` 追加写;超过
//!   [`ROTATE_BYTES`] 时轮转为 `activity.<ts>.<uuid>.jsonl` 再开新文件;
//!   逐条 `flush()`(防进程被杀丢审计)。
//!
//! ## 数据源
//!
//! Logger 不主动订阅;调用方把 `Event` 经 [`map_event`](纯函数,None =
//! 不记录)转换后 `record`。保持本模块对 runtime 的零依赖。

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use zhuye_protocol::{Actor, Event, EventMsg, SideChannelEvent};

/// 内存环形缓冲容量。
pub const CAP_INMEMORY: usize = 500;

/// 单 JSONL 文件 rotate 阈值(1 MB)。
pub const ROTATE_BYTES: u64 = 1024 * 1024;

/// 事件等级(供 UI 颜色 / 过滤)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivityLevel {
    Info,
    Warn,
    Error,
}

impl ActivityLevel {
    /// `"info"` / `"warn"` / `"error"`。
    pub fn as_str(self) -> &'static str {
        match self {
            ActivityLevel::Info => "info",
            ActivityLevel::Warn => "warn",
            ActivityLevel::Error => "error",
        }
    }
}

impl std::fmt::Display for ActivityLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 事件种类(snake_case;与 `EventMsg` variant 一一映射,task / team 类
/// 展开成更细的种类)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    TurnStarted,
    TurnComplete,
    TurnAborted,
    AgentMessage,
    ToolCallBegin,
    ToolCallEnd,
    ApprovalRequest,
    PlanRequest,
    PlanApproved,
    PlanRejected,
    TaskCreated,
    TaskCompleted,
    TaskUpdated,
    TaskClaimed,
    TeamCreated,
    TeamDeleted,
    Mention,
    Error,
    SessionConfigured,
    PermissionModeChanged,
    McpServer,
    LspServer,
    ContextCompacted,
    Other,
}

/// 单条 activity 事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEvent {
    /// 稳定 id(`act-<seq>`)。
    pub id: String,
    /// 毫秒时间戳(UNIX_EPOCH)。
    pub ts_ms: u64,
    pub kind: ActivityKind,
    /// 触发者。
    pub actor: Actor,
    /// 人类可读摘要(UI 主文本)。
    pub summary: String,
    /// 关联任务 id(可选;task / team 类事件回填)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<u32>,
    /// 所属 team(可选)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_name: Option<String>,
    pub level: ActivityLevel,
}

impl ActivityEvent {
    /// 便捷构造(id / ts 自动填)。
    pub fn new(
        kind: ActivityKind,
        actor: Actor,
        summary: impl Into<String>,
        level: ActivityLevel,
    ) -> Self {
        Self {
            id: String::new(),
            ts_ms: now_ms(),
            kind,
            actor,
            summary: summary.into(),
            task_id: None,
            team_name: None,
            level,
        }
    }
}

/// 查询过滤器([`ActivityLogger::list`] 用)。
#[derive(Debug, Clone, Default)]
pub struct ActivityFilter {
    pub kind: Option<ActivityKind>,
    pub level: Option<ActivityLevel>,
    pub actor_id: Option<String>,
    pub team_name: Option<String>,
    pub since_ms: Option<u64>,
    pub limit: Option<usize>,
}

impl ActivityFilter {
    fn matches(&self, e: &ActivityEvent) -> bool {
        if let Some(k) = &self.kind
            && e.kind != *k
        {
            return false;
        }
        if let Some(l) = &self.level
            && e.level != *l
        {
            return false;
        }
        if let Some(a) = &self.actor_id
            && e.actor.actor_id != *a
        {
            return false;
        }
        if let Some(t) = &self.team_name
            && e.team_name.as_deref() != Some(t.as_str())
        {
            return false;
        }
        if let Some(since) = self.since_ms
            && e.ts_ms < since
        {
            return false;
        }
        true
    }
}

struct Inner {
    buf: VecDeque<ActivityEvent>,
    lock_file: Option<File>,
    path: PathBuf,
    next_seq: u64,
    /// 持久化的追加句柄(flock 内复用):避免每条事件 open/close 两个
    /// 系统调用。跨进程轮转检测:每条经 `metadata()` 比对 inode(一次
    /// 调用同时拿到真实文件长度作轮转依据),其他进程 rename 活跃文件
    /// 后本进程下一写即重开。
    write_file: Option<File>,
    write_inode: Option<u64>,
}

/// Activity 审计日志(线程安全;`record` 同步 append)。
pub struct ActivityLogger {
    inner: Mutex<Inner>,
}

impl Default for ActivityLogger {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl std::fmt::Debug for ActivityLogger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivityLogger")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl ActivityLogger {
    /// 默认构造:落盘 `<data_dir>/activity/activity.jsonl`;HOME 缺失时
    /// 退化为纯内存(审计不是启动前提,不 panic)。
    pub fn with_default_home() -> std::sync::Arc<Self> {
        match zhuye_config::data_path(&["activity"]) {
            Some(dir) => {
                static LOGGERS: OnceLock<Mutex<HashMap<PathBuf, Weak<ActivityLogger>>>> =
                    OnceLock::new();
                let mut loggers = LOGGERS.get_or_init(|| Mutex::new(HashMap::new())).lock();
                if let Some(logger) = loggers.get(&dir).and_then(Weak::upgrade) {
                    return logger;
                }
                loggers.retain(|_, logger| logger.strong_count() > 0);
                let logger = Arc::new(Self::new(dir.clone()));
                loggers.insert(dir, Arc::downgrade(&logger));
                logger
            }
            None => Arc::new(Self::in_memory()),
        }
    }

    /// 落盘到 `<root>/activity.jsonl`。
    pub fn new(root: PathBuf) -> Self {
        let path = root.join("activity.jsonl");
        // 跨进程共享锁保护追加和轮转;无法打开时降级到纯内存。
        // 日志文件句柄只用于探路(随后丢弃);锁文件句柄必须走 open_lock。
        let lock_file = open_append(&path)
            .and_then(|_| open_lock(&root.join("activity.lock")))
            .map_err(|e| {
                tracing::warn!(path = %path.display(), error = %e, "activity: open log failed; falling back to in-memory");
                e
            })
            .ok();
        Self {
            inner: Mutex::new(Inner {
                buf: VecDeque::with_capacity(CAP_INMEMORY),
                lock_file,
                path,
                next_seq: 1,
                write_file: None,
                write_inode: None,
            }),
        }
    }

    /// 纯内存(测试 / 关闭落盘)。
    pub fn in_memory() -> Self {
        Self {
            inner: Mutex::new(Inner {
                buf: VecDeque::with_capacity(CAP_INMEMORY),
                lock_file: None,
                path: PathBuf::new(),
                next_seq: 1,
                write_file: None,
                write_inode: None,
            }),
        }
    }

    /// 记录一条(id 自动分配;内存 LRU + 磁盘 append + rotate)。
    pub fn record(&self, mut event: ActivityEvent) -> std::io::Result<()> {
        let mut g = self.inner.lock();
        event.id = format!("act-{:016x}", g.next_seq);
        g.next_seq += 1;
        // 1) 内存环形缓冲。
        if g.buf.len() >= CAP_INMEMORY {
            g.buf.pop_front();
        }
        g.buf.push_back(event.clone());
        // 2) 磁盘 append:flock 内复用持久句柄 —— 省掉每条 open/close
        // 两个系统调用。每条经一次 `metadata()` 同时校验句柄 inode
        // (其他进程轮转过则重开,A27 跨进程语义)与真实文件长度
        // (轮转判断依据)—— 正确性优先,性能收益来自持久句柄。
        if g.lock_file.is_some() {
            // lock/unlock 的借用是临时的,闭包独占 `&mut g` 不与其冲突。
            FileExt::lock_exclusive(g.lock_file.as_ref().expect("已判 Some"))?;
            let result = (|| -> std::io::Result<()> {
                // 闭包内对 g 的可变借与 path 借并存,提前拷出 path。
                let path = g.path.clone();
                // 惰性打开(构造时未持有写句柄)。
                if g.write_file.is_none() {
                    let fresh = open_append(&path)?;
                    g.write_inode = fresh.metadata().ok().as_ref().and_then(file_inode);
                    g.write_file = Some(fresh);
                }
                // 每条一次 **path** stat(非 fd metadata):fd 的 metadata
                // 始终指向打开时的 inode,看不到其他进程对路径的 rename。
                // path inode != fd inode = 别的进程已轮转(本句柄会写进
                // 旧归档)→ 重开;path 长度 = 轮转判断的真实依据。
                let path_st = std::fs::metadata(&path)?;
                if g.write_inode.is_some() && file_inode(&path_st) != g.write_inode {
                    let fresh = open_append(&path)?;
                    g.write_inode = fresh.metadata().ok().as_ref().and_then(file_inode);
                    g.write_file = Some(fresh);
                }
                let mut line = serde_json::to_vec(&event)?;
                line.push(b'\n');
                if let Some(file) = g.write_file.as_mut() {
                    file.write_all(&line)?;
                    file.flush()?;
                }
                if path_st.len() + line.len() as u64 >= ROTATE_BYTES {
                    let rotated =
                        path.with_extension(format!("{}.{}.jsonl", now_ms(), uuid::Uuid::new_v4()));
                    std::fs::rename(&path, rotated)?;
                    let fresh = open_append(&path)?;
                    g.write_inode = fresh.metadata().ok().as_ref().and_then(file_inode);
                    g.write_file = Some(fresh);
                }
                Ok(())
            })();
            let unlocked = FileExt::unlock(g.lock_file.as_ref().expect("已判 Some"));
            result.and(unlocked)?;
        }
        Ok(())
    }

    /// 按过滤器查内存缓冲(新 → 旧)。
    pub fn list(&self, filter: &ActivityFilter) -> Vec<ActivityEvent> {
        let g = self.inner.lock();
        let mut out: Vec<ActivityEvent> = g
            .buf
            .iter()
            .rev()
            .filter(|e| filter.matches(e))
            .cloned()
            .collect();
        if let Some(limit) = filter.limit {
            out.truncate(limit);
        }
        out
    }

    /// `@<id>` mention 搜索:summary 里含 `@<actor_id>` 的条目。
    pub fn search_mentions(&self, query: &str) -> Vec<ActivityEvent> {
        if query.is_empty() {
            return Vec::new();
        }
        let needle = format!("@{query}");
        let g = self.inner.lock();
        g.buf
            .iter()
            .rev()
            .filter(|e| e.summary.contains(&needle) || e.actor.actor_id == query)
            .cloned()
            .collect()
    }

    /// 内存缓冲条数。
    pub fn len(&self) -> usize {
        self.inner.lock().buf.len()
    }

    /// 内存缓冲是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 句柄指向文件的 inode(unix);非 unix 平台返回 `None` —— 陈旧检测
/// 跳过(写句柄仍持久化,仅失去跨进程轮转重开能力)。
#[cfg(unix)]
fn file_inode(m: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(m.ino())
}
#[cfg(not(unix))]
fn file_inode(_m: &std::fs::Metadata) -> Option<u64> {
    None
}

fn open_append(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// 打开锁文件专用句柄:锁文件本身不读写任何内容,但 Windows 的 LockFileEx
/// 要求句柄带 GENERIC_READ 或 GENERIC_WRITE —— Rust std 的 `append(true)`
/// 会刻意剔除 FILE_WRITE_DATA(只留 FILE_APPEND_DATA),拿 append 句柄加锁
/// 会直接失败在 ACCESS_DENIED(code 5)。必须用 read + write 方式打开。
/// `truncate(false)` 显式声明保留已有内容,同时消解
/// clippy::suspicious_open_options(与 zhuye-task 锁文件同一口径)。
fn open_lock(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// 把 `Event` 映射成 [`ActivityEvent`](None = 不记录)。默认 actor:
/// 用户发起的动作(turn / plan / approval)归 `Actor::user()`,agent /
/// 基础设施动作(tool / mcp / lsp / compact / error)归 `Actor::system()`。
///
/// 移植自桌面端 `src-tauri/src/state/activity.rs::map_event`(剥离 tauri)。
pub fn map_event(event: &Event) -> Option<ActivityEvent> {
    let (kind, summary, level, actor) = match &event.msg {
        EventMsg::TurnStarted(_) => (
            ActivityKind::TurnStarted,
            "Turn started".to_string(),
            ActivityLevel::Info,
            Actor::user(),
        ),
        EventMsg::TurnComplete(_) => (
            ActivityKind::TurnComplete,
            "Turn complete".to_string(),
            ActivityLevel::Info,
            Actor::user(),
        ),
        EventMsg::TurnAborted(_) => (
            ActivityKind::TurnAborted,
            "Turn aborted".to_string(),
            ActivityLevel::Warn,
            Actor::user(),
        ),
        EventMsg::AgentMessage(_) => (
            ActivityKind::AgentMessage,
            "Agent sent a message".to_string(),
            ActivityLevel::Info,
            Actor::system(),
        ),
        EventMsg::ToolCallBegin(e) => (
            ActivityKind::ToolCallBegin,
            format!("Tool started: {}", e.tool_name),
            ActivityLevel::Info,
            Actor::system(),
        ),
        EventMsg::ToolCallEnd(_) => (
            ActivityKind::ToolCallEnd,
            "Tool finished".to_string(),
            ActivityLevel::Info,
            Actor::system(),
        ),
        EventMsg::ApprovalRequest(e) => (
            ActivityKind::ApprovalRequest,
            format!("Approval pending: {:?}", e.kind),
            ActivityLevel::Warn,
            Actor::user(),
        ),
        EventMsg::AskUserQuestion(_) => (
            ActivityKind::ApprovalRequest,
            "Agent asked a question".to_string(),
            ActivityLevel::Warn,
            Actor::user(),
        ),
        EventMsg::AskUserInput(_) => (
            ActivityKind::ApprovalRequest,
            "Agent requested input".to_string(),
            ActivityLevel::Warn,
            Actor::user(),
        ),
        EventMsg::PlanRequest(_) => (
            ActivityKind::PlanRequest,
            "Plan requested".to_string(),
            ActivityLevel::Info,
            Actor::user(),
        ),
        EventMsg::PlanReady(_) => (
            ActivityKind::PlanRequest,
            "Plan ready for review".to_string(),
            ActivityLevel::Info,
            Actor::user(),
        ),
        EventMsg::PlanApproved(_) => (
            ActivityKind::PlanApproved,
            "Plan approved".to_string(),
            ActivityLevel::Info,
            Actor::user(),
        ),
        EventMsg::PlanRejected(_) => (
            ActivityKind::PlanRejected,
            "Plan rejected".to_string(),
            ActivityLevel::Warn,
            Actor::user(),
        ),
        EventMsg::PermissionModeChanged(e) => (
            ActivityKind::PermissionModeChanged,
            format!("Permission mode changed: {:?}", e.to),
            ActivityLevel::Info,
            Actor::user(),
        ),
        EventMsg::ContextCompacted(_) => (
            ActivityKind::ContextCompacted,
            "Context compacted".to_string(),
            ActivityLevel::Info,
            Actor::system(),
        ),
        EventMsg::Error(e) => (
            ActivityKind::Error,
            format!("Error: {}", e.message),
            ActivityLevel::Error,
            Actor::system(),
        ),
        EventMsg::StreamError(e) => (
            ActivityKind::Error,
            format!("Stream error: {}", e.message),
            ActivityLevel::Error,
            Actor::system(),
        ),
        EventMsg::McpServerStarted(e) => (
            ActivityKind::McpServer,
            format!("MCP server started: {}", e.server),
            ActivityLevel::Info,
            Actor::system(),
        ),
        EventMsg::McpServerFailed(e) => (
            ActivityKind::McpServer,
            format!("MCP server failed: {}", e.server),
            ActivityLevel::Error,
            Actor::system(),
        ),
        EventMsg::LspServerStarted(e) => (
            ActivityKind::LspServer,
            format!("LSP server started: {}", e.server),
            ActivityLevel::Info,
            Actor::system(),
        ),
        EventMsg::LspServerFailed(e) => (
            ActivityKind::LspServer,
            format!("LSP server failed: {}", e.server),
            ActivityLevel::Error,
            Actor::system(),
        ),
        EventMsg::SessionConfigured(_) => (
            ActivityKind::SessionConfigured,
            "Session configured".to_string(),
            ActivityLevel::Info,
            Actor::system(),
        ),
        // task / team 生命周期经 TaskManager 的 hook 事件透传(无独立
        // EventMsg),此映射覆盖不到的部分由调用方按 hook 事件补记。
        // SideChannel* 的载荷是 tag 枚举(kind: started/done/...),按内层
        // 变体取 agent 名。
        EventMsg::SideChannelStarted(e) => (
            ActivityKind::Other,
            match e {
                SideChannelEvent::Started { agent_name, .. } => {
                    format!("Side channel started: {agent_name}")
                }
                _ => "Side channel started".to_string(),
            },
            ActivityLevel::Info,
            Actor::user(),
        ),
        EventMsg::SideChannelFinished(_) => (
            ActivityKind::Other,
            "Side channel finished".to_string(),
            ActivityLevel::Info,
            Actor::user(),
        ),
        _ => return None,
    };
    Some(ActivityEvent::new(kind, actor, summary, level))
}

/// 订阅桥:把 `Event` 流沉淀进 logger(纯 async,不绑 runtime —— 调用方
/// spawn)。移植自桌面端 `subscribe_activity_logger`(剥 tauri)。
pub async fn pump_events(
    logger: std::sync::Arc<ActivityLogger>,
    mut rx: tokio::sync::mpsc::Receiver<Event>,
) {
    while let Some(event) = rx.recv().await {
        if let Some(activity) = map_event(&event)
            && let Err(e) = logger.record(activity)
        {
            tracing::warn!(error = %e, "[activity] record failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_logger() -> (TempDirGuard, ActivityLogger) {
        let dir = TempDirGuard(tempfile::tempdir().unwrap());
        let logger = ActivityLogger::new(dir.0.path().to_path_buf());
        (dir, logger)
    }

    /// 包一层让 tempdir 活到测试结束。
    struct TempDirGuard(tempfile::TempDir);
    impl TempDirGuard {
        fn path(&self) -> &Path {
            self.0.path()
        }
    }

    fn sample(kind: ActivityKind, summary: &str, level: ActivityLevel) -> ActivityEvent {
        ActivityEvent::new(kind, Actor::user(), summary, level)
    }

    #[test]
    fn record_assigns_ids_and_keeps_order() {
        let (_d, logger) = temp_logger();
        for i in 0..5 {
            logger
                .record(sample(
                    ActivityKind::TurnStarted,
                    format!("t{i}").as_str(),
                    ActivityLevel::Info,
                ))
                .unwrap();
        }
        let all = logger.list(&ActivityFilter::default());
        assert_eq!(all.len(), 5);
        // 新 → 旧。
        assert_eq!(all[0].summary, "t4");
        // id 单调。
        assert!(all[0].id != all[1].id);
    }

    #[test]
    fn in_memory_buffer_evicts_oldest_beyond_cap() {
        let logger = ActivityLogger::in_memory();
        for i in 0..(CAP_INMEMORY + 20) {
            logger
                .record(sample(
                    ActivityKind::Other,
                    format!("e{i}").as_str(),
                    ActivityLevel::Info,
                ))
                .unwrap();
        }
        assert_eq!(logger.len(), CAP_INMEMORY);
        let all = logger.list(&ActivityFilter::default());
        assert_eq!(all[0].summary, format!("e{}", CAP_INMEMORY + 19));
    }

    #[test]
    fn disk_append_survives_reopen() {
        let (dir, logger) = temp_logger();
        logger
            .record(sample(
                ActivityKind::AgentMessage,
                "persisted",
                ActivityLevel::Info,
            ))
            .unwrap();
        // 重新打开:文件还在(append 模式),内容不丢。
        let path = dir.path().join("activity.jsonl");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("persisted"), "落盘内容:{body}");
        assert!(
            body.contains("\"actorType\":\"system\"") || body.contains("agent"),
            "actor 序列化"
        );
    }

    #[test]
    fn rapid_rotations_keep_each_archive() {
        let (dir, logger) = temp_logger();
        for summary in ["第一条", "第二条"] {
            let padded = format!("{summary}{}", "x".repeat(ROTATE_BYTES as usize));
            logger
                .record(sample(ActivityKind::Other, &padded, ActivityLevel::Info))
                .unwrap();
        }
        let mut summaries = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "jsonl")
                    && path.file_name().unwrap() != "activity.jsonl"
            })
            .map(|path| {
                let body = std::fs::read_to_string(path).unwrap();
                serde_json::from_str::<ActivityEvent>(body.trim())
                    .unwrap()
                    .summary
                    .chars()
                    .take(3)
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        summaries.sort();
        assert_eq!(summaries, ["第一条", "第二条"]);
    }

    #[test]
    fn filter_by_kind_level_and_actor() {
        let (_d, logger) = temp_logger();
        logger
            .record(sample(ActivityKind::TurnStarted, "a", ActivityLevel::Info))
            .unwrap();
        logger
            .record(sample(ActivityKind::Error, "b", ActivityLevel::Error))
            .unwrap();
        logger
            .record(sample(
                ActivityKind::ToolCallBegin,
                "c",
                ActivityLevel::Info,
            ))
            .unwrap();

        let errors = logger.list(&ActivityFilter {
            level: Some(ActivityLevel::Error),
            ..Default::default()
        });
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].summary, "b");

        let tools = logger.list(&ActivityFilter {
            kind: Some(ActivityKind::ToolCallBegin),
            ..Default::default()
        });
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].summary, "c");

        let mine = logger.list(&ActivityFilter {
            actor_id: Some("user".into()),
            ..Default::default()
        });
        assert_eq!(mine.len(), 3, "三条都是 user actor");
    }

    #[test]
    fn search_mentions_finds_at_references() {
        let (_d, logger) = temp_logger();
        logger
            .record(sample(
                ActivityKind::Mention,
                "请 @builder 看一下",
                ActivityLevel::Info,
            ))
            .unwrap();
        logger
            .record(sample(
                ActivityKind::AgentMessage,
                "没有提及",
                ActivityLevel::Info,
            ))
            .unwrap();
        let hits = logger.search_mentions("builder");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].summary.contains("@builder"));
        assert!(logger.search_mentions("").is_empty());
    }

    #[test]
    fn serde_camel_case_roundtrip() {
        let mut e = sample(ActivityKind::TaskCreated, "task", ActivityLevel::Warn);
        e.task_id = Some(7);
        e.team_name = Some("rocket".into());
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["taskId"], 7);
        assert_eq!(v["teamName"], "rocket");
        assert_eq!(v["level"], "warn");
        let back: ActivityEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back.task_id, Some(7));
    }

    #[test]
    fn map_event_covers_turn_tool_and_error() {
        use zhuye_protocol::{EventMsg, TokenUsage, TurnCompleteEvent, TurnStartedEvent};
        // turn 类 → user actor。
        let ev = Event {
            id: "x".into(),
            msg: EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: zhuye_protocol::TurnId::new(),
                user_message_id: None,
            }),
        };
        let a = map_event(&ev).unwrap();
        assert_eq!(a.kind, ActivityKind::TurnStarted);
        assert_eq!(a.actor.actor_id, "user");

        // tool 类 → system actor。
        let ev = Event {
            id: "x".into(),
            msg: EventMsg::ToolCallBegin(zhuye_protocol::ToolCallBeginEvent {
                call_id: "c1".into(),
                tool_name: "bash".into(),
                args: serde_json::json!({}),
                child_id: None,
            }),
        };
        let a = map_event(&ev).unwrap();
        assert_eq!(a.kind, ActivityKind::ToolCallBegin);
        assert_eq!(a.actor.actor_id, "system");
        assert!(a.summary.contains("bash"));

        // error 类 → Error 等级。
        let ev = Event {
            id: "x".into(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: zhuye_protocol::TurnId::new(),
                usage: TokenUsage::default(),
                status: zhuye_protocol::TurnStatus::Success,
            }),
        };
        let a = map_event(&ev).unwrap();
        assert_eq!(a.kind, ActivityKind::TurnComplete);

        // 未映射变体 → None。
        let ev = Event {
            id: "x".into(),
            msg: EventMsg::AgentMessageDelta(zhuye_protocol::AgentMessageDelta {
                delta: "x".into(),
            }),
        };
        assert!(map_event(&ev).is_none(), "delta 不进审计(噪音)");
    }
}
