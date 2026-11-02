//! Side-channel 执行驱动 —— 桌面端缺失的那一半。
//!
//! 桌面端 `app-core/src/side_channel.rs` 只有 registry(id / 取消 /
//! 事件),其命令层注释自述「把实际执行接到 AgentThread 未做」。本模块
//! 补上:经 **thread 工厂**创建独立 `AgentThread`,把 prompt 作为
//! `Submission::user_input` 注入,消费 `TurnHandle` 事件流,完成后推进
//! registry 终态并广播。
//!
//! ## 关键不变量
//!
//! - **独立取消**:side thread 用自己的 `CancellationToken`(构造时由
//!   registry 生成),不是主 token 的子级 —— 主 agent 的 `Op::Interrupt`
//!   不会误杀 side-channel;
//! - **不写主历史**:thread 工厂负责构造 recorder 为 `None` 的配置
//!   (副作用留在调用方),driver 只负责驱动;
//! - **独立预算**:`in_flight` 计数器与 `MAX_CONCURRENT` 独立于
//!   `zhuye-subagent` 的共享槽位;
//! - **失败不污染**:构造 / 提交失败只推进该 id 的 Error 终态并广播。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::{MAX_CONCURRENT, SideChannelId, SideChannelRegistry, SideChannelStatus};

/// thread 工厂:按 agent 名 + 取消令牌构造一条独立 `AgentThread`。
///
/// 由调用方(exec / facade)实现 —— 工厂决定 agent 定义来源、工具集、
/// memory store 与 **recorder 是否为 `None`**(不写主历史的承诺在工厂
/// 落地,driver 不感知 rollout)。
pub type SideThreadFactory = Arc<
    dyn Fn(&str, CancellationToken) -> anyhow::Result<crate::AgentThread> + Send + Sync + 'static,
>;

/// driver 启动失败的原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideChannelStartError {
    /// 并发已满(`MAX_CONCURRENT`)。
    Busy,
    /// thread 工厂失败(agent 定义缺失等)。
    Factory(String),
}

impl std::fmt::Display for SideChannelStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => write!(
                f,
                "side-channel concurrency limit reached ({MAX_CONCURRENT})"
            ),
            Self::Factory(e) => write!(f, "side-channel thread factory failed: {e}"),
        }
    }
}

impl std::error::Error for SideChannelStartError {}

/// side-channel 执行驱动。
#[derive(Clone)]
pub struct SideChannelDriver {
    registry: SideChannelRegistry,
    factory: SideThreadFactory,
    in_flight: Arc<AtomicUsize>,
}

impl std::fmt::Debug for SideChannelDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SideChannelDriver")
            .field("running", &self.registry.running_count())
            .finish_non_exhaustive()
    }
}

impl SideChannelDriver {
    /// 用 registry + thread 工厂构造。
    pub fn new(registry: SideChannelRegistry, factory: SideThreadFactory) -> Self {
        Self {
            registry,
            factory,
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 只含 registry 的空 driver(工厂缺失时 `start` 一律 `Factory` 失败)。
    /// 供 `AgentConfig` 默认值与测试使用。
    pub fn without_factory(registry: SideChannelRegistry) -> Self {
        Self::new(
            registry,
            Arc::new(|_agent, _cancel| Err(anyhow::anyhow!("side-channel factory not configured"))),
        )
    }

    /// registry 句柄(查询 / 取消 / 订阅)。
    pub fn registry(&self) -> &SideChannelRegistry {
        &self.registry
    }

    /// 启动一个 side-channel:注册 → 构造独立 thread → 注入 prompt →
    /// 后台 task 消费事件流并推进终态。
    ///
    /// 立即返回 `(id, cancel)`;执行异步进行。并发满 → `Busy`。
    pub fn start(
        &self,
        agent_name: &str,
        prompt: &str,
    ) -> Result<(SideChannelId, CancellationToken), SideChannelStartError> {
        if self.in_flight.load(Ordering::Relaxed) >= MAX_CONCURRENT {
            return Err(SideChannelStartError::Busy);
        }
        let (id, cancel) = self
            .registry
            .start(agent_name.to_string(), prompt.to_string());
        self.in_flight.fetch_add(1, Ordering::Relaxed);

        let registry = self.registry.clone();
        let factory = self.factory.clone();
        let in_flight = self.in_flight.clone();
        let id_for_task = id.clone();
        let agent_name = agent_name.to_string();
        let prompt = prompt.to_string();
        let cancel_for_task = cancel.clone();

        tokio::spawn(async move {
            let _guard = InFlightGuard(in_flight);
            let outcome = run_side_channel(
                factory,
                &agent_name,
                &prompt,
                cancel_for_task,
                &registry,
                &id_for_task,
            )
            .await;
            match outcome {
                Ok(summary) => {
                    registry.finish(&id_for_task, SideChannelStatus::Done, summary);
                }
                Err(message) => {
                    // 已取消 → 状态已是 Cancelled,迟到 Error 是 noop。
                    registry.finish(&id_for_task, SideChannelStatus::Error, Some(message));
                }
            }
        });

        Ok((id, cancel))
    }

    /// 按 id 取消。
    pub fn cancel(&self, id: &str) -> bool {
        self.registry.cancel(id)
    }

    /// 快照列表。
    pub fn list(&self) -> Vec<super::SideChannelInfo> {
        self.registry.list()
    }
}

/// 执行一个 side-channel,返回 Done 的 summary(最后一条 assistant 文本)。
async fn run_side_channel(
    factory: SideThreadFactory,
    agent_name: &str,
    prompt: &str,
    cancel: CancellationToken,
    registry: &SideChannelRegistry,
    id: &str,
) -> Result<Option<String>, String> {
    // 1. 构造独立 thread(工厂决定 recorder=None 等)。
    let thread = factory(agent_name, cancel.clone()).map_err(|e| e.to_string())?;
    // 2. 注入 prompt(与 cron driver 同一入口)。
    let sub = zhuye_protocol::Submission::user_input(prompt);
    let mut handle = thread.submit(sub).await;
    // 3. 消费事件流:收集最后一条 assistant 文本作 summary;取消 / 断开
    //    时提前结束。
    let mut summary: Option<String> = None;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                registry.finish(id, SideChannelStatus::Cancelled, None);
                return Err("cancelled".into());
            }
            event = handle.next() => {
                let Some(event) = event else {
                    // 流结束 = turn 完成(或 loop 已退出)。
                    break;
                };
                if let Some(text) = assistant_text(&event) {
                    summary = Some(text);
                }
            }
        }
    }
    Ok(summary)
}

/// 从事件里提取 assistant 增量文本(最后一条非空即 summary)。
fn assistant_text(event: &zhuye_protocol::Event) -> Option<String> {
    use zhuye_protocol::EventMsg;
    let EventMsg::AgentMessageDelta(delta) = &event.msg else {
        return None;
    };
    (!delta.delta.trim().is_empty()).then(|| delta.delta.clone())
}

/// 退出作用域时递减在途计数(panic 安全)。
struct InFlightGuard(Arc<AtomicUsize>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests;
