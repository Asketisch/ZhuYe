//! Side-channel agent —— 用户驱动、独立于主 agent 轮次生命周期的并发
//! agent 运行(WP9 收编 ReflectDesktop `app-core/src/side_channel.rs`)。
//!
//! ## 与 `Task` 工具(模型驱动子代理)的区别
//!
//! | | Side-channel | Subagent (`Task`) |
//! |---|---|---|
//! | 触发方式 | 用户输入(Op / SDK) | 模型调用 `Task` 工具 |
//! | 并发性 | 与主 agent 并发运行 | 阻塞主轮次 |
//! | 主历史 | 不受影响(独立 thread,不写主 recorder) | 工具结果写入主历史 |
//! | 取消 | 独立 `CancellationToken` —— 主窗口中断不误杀 | 继承父级取消信号 |
//! | 事件 | 专用 `EventMsg::SideChannel*` | `SubagentProgress` |
//!
//! ## 结构
//!
//! - [`SideChannelRegistry`]:进程级 registry,稳定 id(`side-<8hex>`) →
//!   handle,带 `broadcast` 事件流(Started / Done / Cancelled / Error);
//! - [`SideChannelDriver`]:在 registry 之上补**执行驱动** —— 桌面端
//!   只有 registry 没有 driver(其命令层注释自述「实际执行未做」),这里
//!   经 thread 工厂创建独立 `AgentThread` 并注入 prompt。
//!
//! ## 与 subagent 在途预算的关系
//!
//! driver 用**独立的** `AtomicUsize` 计数器与上限(`MAX_CONCURRENT`),
//! 不占 `zhuye-subagent` 的共享槽位 —— 两类并发互不挤占。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// side-channel 并发上限(独立预算,不占 subagent 的 16 槽位)。
pub const MAX_CONCURRENT: usize = 8;

/// 稳定的 handle 标识符(`side-<8 hex>`)。
pub type SideChannelId = String;

/// side-channel 的生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SideChannelStatus {
    /// 正在执行。
    Running,
    /// 自然完成(成功或结果为空)。
    Done,
    /// 已被用户取消。
    Cancelled,
    /// 因错误退出。
    Error,
}

impl SideChannelStatus {
    /// `"running"` / `"done"` / `"cancelled"` / `"error"`。
    pub fn as_str(self) -> &'static str {
        match self {
            SideChannelStatus::Running => "running",
            SideChannelStatus::Done => "done",
            SideChannelStatus::Cancelled => "cancelled",
            SideChannelStatus::Error => "error",
        }
    }
}

/// Registry 事件流中的一项(状态变化时发送给所有订阅者)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SideChannelEvent {
    /// 已启动。
    Started {
        id: SideChannelId,
        agent_name: String,
        prompt: String,
        started_at_ms: i64,
    },
    /// 已正常完成;`summary` 供 UI 在频道标题显示(最后一条 assistant 文本)。
    Done {
        id: SideChannelId,
        finished_at_ms: i64,
        summary: Option<String>,
    },
    /// 已被取消。
    Cancelled {
        id: SideChannelId,
        finished_at_ms: i64,
    },
    /// 因错误退出。
    Error {
        id: SideChannelId,
        finished_at_ms: i64,
        message: String,
    },
}

/// side-channel 的公开视图快照(发送到前端 / IPC)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SideChannelInfo {
    pub id: SideChannelId,
    pub agent_name: String,
    pub prompt: String,
    pub started_at_ms: i64,
    /// `"running"` / `"done"` / `"cancelled"` / `"error"`。
    pub status: String,
    /// 持续时间(毫秒);运行中为 `None`。
    pub duration_ms: Option<i64>,
}

/// Registry 中保存的单个 side-channel 状态。
pub(crate) struct SideChannelHandle {
    pub agent_name: String,
    pub prompt: String,
    pub started_at: Instant,
    pub started_at_epoch_ms: i64,
    pub status: SideChannelStatus,
    pub duration_ms: Option<i64>,
    /// 独立取消 token(不是主 token 的子级)。
    pub cancel: CancellationToken,
}

impl SideChannelHandle {
    fn snapshot(&self, id: &str) -> SideChannelInfo {
        SideChannelInfo {
            id: id.to_string(),
            agent_name: self.agent_name.clone(),
            prompt: self.prompt.clone(),
            started_at_ms: self.started_at_epoch_ms,
            status: self.status.as_str().to_string(),
            duration_ms: self.duration_ms,
        }
    }
}

/// 所有 side-channel 的进程级 Registry。
#[derive(Clone)]
pub struct SideChannelRegistry {
    inner: Arc<Mutex<HashMap<SideChannelId, SideChannelHandle>>>,
    events: broadcast::Sender<SideChannelEvent>,
    /// 进程级计数器;回绕时跳过仍存在的 id。
    next_seq: Arc<AtomicUsize>,
}

impl Default for SideChannelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SideChannelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SideChannelRegistry")
            .field("count", &self.inner.lock().len())
            .finish_non_exhaustive()
    }
}

impl SideChannelRegistry {
    /// 使用默认 broadcast 容量(256)构造。
    pub fn new() -> Self {
        Self::with_capacity(256)
    }

    /// 使用指定 broadcast 容量构造。
    pub fn with_capacity(cap: usize) -> Self {
        let (events, _) = broadcast::channel(cap);
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            events,
            next_seq: Arc::new(AtomicUsize::new(1)),
        }
    }

    /// 订阅事件流。
    pub fn subscribe_events(&self) -> broadcast::Receiver<SideChannelEvent> {
        self.events.subscribe()
    }

    /// 插入新的 side-channel,返回分配的 id 与调用方必须持有的取消
    /// handle,并发送 `Started` 事件。
    pub fn start(&self, agent_name: String, prompt: String) -> (SideChannelId, CancellationToken) {
        let epoch_ms = now_ms();
        let handle = SideChannelHandle {
            agent_name: agent_name.clone(),
            prompt: prompt.clone(),
            started_at: Instant::now(),
            started_at_epoch_ms: epoch_ms,
            status: SideChannelStatus::Running,
            duration_ms: None,
            cancel: CancellationToken::new(),
        };
        let cancel = handle.cancel.clone();
        let mut g = self.inner.lock();
        let id = loop {
            let candidate = format!("side-{:08x}", self.next_seq.fetch_add(1, Ordering::Relaxed));
            if !g.contains_key(&candidate) {
                break candidate;
            }
        };
        g.insert(id.clone(), handle);
        let _ = self.events.send(SideChannelEvent::Started {
            id: id.clone(),
            agent_name,
            prompt,
            started_at_ms: epoch_ms,
        });
        drop(g);
        (id, cancel)
    }

    /// 按 id 取消(触发 token + 置状态)。找到并取消时返回 `true`。
    pub fn cancel(&self, id: &str) -> bool {
        let mut g = self.inner.lock();
        if let Some(h) = g
            .get_mut(id)
            .filter(|h| h.status == SideChannelStatus::Running)
        {
            h.cancel.cancel();
            h.status = SideChannelStatus::Cancelled;
            h.duration_ms = Some(h.started_at.elapsed().as_millis() as i64);
            let finished_at = now_ms();
            let _ = self.events.send(SideChannelEvent::Cancelled {
                id: id.to_string(),
                finished_at_ms: finished_at,
            });
            return true;
        }
        false
    }

    /// 标记自然完成。终态不可逆 —— 迟到的 `finish` 是 noop。
    pub fn finish(&self, id: &str, status: SideChannelStatus, summary: Option<String>) -> bool {
        let mut g = self.inner.lock();
        let Some(h) = g
            .get_mut(id)
            .filter(|h| h.status == SideChannelStatus::Running)
        else {
            return false;
        };
        h.status = status;
        h.duration_ms = Some(h.started_at.elapsed().as_millis() as i64);
        let finished_at = now_ms();
        let event = match status {
            SideChannelStatus::Done => SideChannelEvent::Done {
                id: id.to_string(),
                finished_at_ms: finished_at,
                summary,
            },
            SideChannelStatus::Error => SideChannelEvent::Error {
                id: id.to_string(),
                finished_at_ms: finished_at,
                message: summary.unwrap_or_else(|| "side channel failed".into()),
            },
            // Running / Cancelled 不走 finish 路径。
            _ => return true,
        };
        let _ = self.events.send(event);
        true
    }

    /// 全部 side-channel 的快照(按 started_at 升序,同毫秒按 id 字典序
    /// —— HashMap 迭代序随机,加 tiebreak 保证输出确定性)。
    pub fn list(&self) -> Vec<SideChannelInfo> {
        let g = self.inner.lock();
        let mut out: Vec<SideChannelInfo> = g.iter().map(|(id, h)| h.snapshot(id)).collect();
        out.sort_by(|a, b| {
            a.started_at_ms
                .cmp(&b.started_at_ms)
                .then_with(|| a.id.cmp(&b.id))
        });
        out
    }

    /// 单个 side-channel 的快照。
    pub fn get(&self, id: &str) -> Option<SideChannelInfo> {
        let g = self.inner.lock();
        g.get(id).map(|h| h.snapshot(id))
    }

    /// 运行中的数量。
    pub fn running_count(&self) -> usize {
        self.inner
            .lock()
            .values()
            .filter(|h| h.status == SideChannelStatus::Running)
            .count()
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests;
