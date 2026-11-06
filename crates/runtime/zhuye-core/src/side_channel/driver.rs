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

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use super::{MAX_CONCURRENT, SideChannelId, SideChannelRegistry, SideChannelStatus};

/// thread 工厂:按 agent 名 + 取消令牌构造一条独立 `AgentThread`。
///
/// 由调用方(exec / facade)实现 —— 工厂决定 agent 定义来源、工具集、
/// memory store 与 **recorder 是否为 `None`**(不写主历史的承诺在工厂
/// 落地,driver 不感知 rollout)。失败原因统一用
/// [`SideChannelStartError::Factory`],不引入无类型错误。
pub type SideThreadFactory = Arc<
    dyn Fn(&str, CancellationToken) -> Result<crate::AgentThread, SideChannelStartError>
        + Send
        + Sync
        + 'static,
>;

/// 侧通道启动时的工作区与模型快照。
#[derive(Debug, Clone)]
pub struct SideChannelContext {
    pub workspace: std::path::PathBuf,
    pub model: String,
}

/// 接收启动时上下文快照的侧通道线程工厂。
pub type SideContextThreadFactory = Arc<
    dyn Fn(
            &str,
            CancellationToken,
            SideChannelContext,
        ) -> Result<crate::AgentThread, SideChannelStartError>
        + Send
        + Sync
        + 'static,
>;

#[derive(Clone)]
enum Factory {
    Plain(SideThreadFactory),
    Context {
        workspace: Arc<parking_lot::RwLock<std::path::PathBuf>>,
        model: Arc<parking_lot::RwLock<String>>,
        build: SideContextThreadFactory,
    },
}

/// driver 启动失败的原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum SideChannelStartError {
    /// 并发已满(`MAX_CONCURRENT`)。
    #[error("side-channel concurrency limit reached ({MAX_CONCURRENT})")]
    Busy,
    /// thread 工厂失败(agent 定义缺失等)。
    #[error("side-channel thread factory failed: {0}")]
    Factory(String),
}

/// side-channel 执行驱动。
#[derive(Clone)]
pub struct SideChannelDriver {
    registry: SideChannelRegistry,
    factory: Factory,
    in_flight: Arc<AtomicUsize>,
    approval_bridge: ApprovalBridge,
}

type ApprovalRoutes = Arc<
    Mutex<
        HashMap<
            (InteractionKind, String),
            (SideChannelId, mpsc::Sender<zhuye_protocol::Submission>),
        >,
    >,
>;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum InteractionKind {
    Approval,
    Question,
    Input,
    Plan,
}

#[derive(Clone)]
struct ApprovalBridge {
    events: broadcast::Sender<(SideChannelId, zhuye_protocol::ApprovalRequestEvent)>,
    interactions: broadcast::Sender<(SideChannelId, zhuye_protocol::EventMsg)>,
    routes: ApprovalRoutes,
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
        let (approvals, _) = broadcast::channel(128);
        let (interactions, _) = broadcast::channel(128);
        Self {
            registry,
            factory: Factory::Plain(factory),
            in_flight: Arc::new(AtomicUsize::new(0)),
            approval_bridge: ApprovalBridge {
                events: approvals,
                interactions,
                routes: Arc::new(Mutex::new(HashMap::new())),
            },
        }
    }

    /// 启动时快照父会话工作区和模型,后台线程始终使用该快照。
    pub fn new_with_context(
        registry: SideChannelRegistry,
        workspace: Arc<parking_lot::RwLock<std::path::PathBuf>>,
        model: Arc<parking_lot::RwLock<String>>,
        factory: SideContextThreadFactory,
    ) -> Self {
        let mut driver = Self::without_factory(registry);
        driver.factory = Factory::Context {
            workspace,
            model,
            build: factory,
        };
        driver
    }

    /// 只含 registry 的空 driver(工厂缺失时 `start` 一律 `Factory` 失败)。
    /// 供 `AgentConfig` 默认值与测试使用。
    pub fn without_factory(registry: SideChannelRegistry) -> Self {
        Self::new(
            registry,
            Arc::new(|_agent, _cancel| {
                Err(SideChannelStartError::Factory(
                    "side-channel factory not configured".into(),
                ))
            }),
        )
    }

    /// registry 句柄(查询 / 取消 / 订阅)。
    pub fn registry(&self) -> &SideChannelRegistry {
        &self.registry
    }

    /// 订阅侧通道审批请求;调用方应在启动任务前订阅以免遗漏。
    pub fn subscribe_approvals(
        &self,
    ) -> broadcast::Receiver<(SideChannelId, zhuye_protocol::ApprovalRequestEvent)> {
        self.approval_bridge.events.subscribe()
    }

    /// 把审批结果送回发起该请求的侧通道;未知或已结束的请求返回 false。
    pub async fn resolve_approval(
        &self,
        id: &str,
        decision: zhuye_protocol::ReviewDecision,
    ) -> bool {
        self.resolve_interaction(
            InteractionKind::Approval,
            id,
            zhuye_protocol::Op::ToolApproval {
                id: id.to_string(),
                decision,
            },
        )
        .await
    }

    /// 订阅侧通道提问和计划审批事件;需在启动前订阅。
    pub fn subscribe_interactions(
        &self,
    ) -> broadcast::Receiver<(SideChannelId, zhuye_protocol::EventMsg)> {
        self.approval_bridge.interactions.subscribe()
    }

    /// 把选择题答案送回对应侧通道。
    pub async fn resolve_question(&self, id: &str, answers: zhuye_protocol::AskUserAnswer) -> bool {
        self.resolve_interaction(
            InteractionKind::Question,
            id,
            zhuye_protocol::Op::AskUserQuestionResponse {
                id: id.to_string(),
                answers,
            },
        )
        .await
    }

    /// 把自由文本答案送回对应侧通道。
    pub async fn resolve_input(&self, id: &str, text: String) -> bool {
        self.resolve_interaction(
            InteractionKind::Input,
            id,
            zhuye_protocol::Op::AskUserInputResponse {
                id: id.to_string(),
                text,
            },
        )
        .await
    }

    /// 把计划审批结果送回对应侧通道。
    pub async fn resolve_plan(&self, id: &str, choice: zhuye_protocol::PlanApprovalChoice) -> bool {
        self.resolve_interaction(
            InteractionKind::Plan,
            id,
            zhuye_protocol::Op::PlanApproval {
                id: id.to_string(),
                choice,
            },
        )
        .await
    }

    async fn resolve_interaction(
        &self,
        kind: InteractionKind,
        id: &str,
        op: zhuye_protocol::Op,
    ) -> bool {
        let route = self
            .approval_bridge
            .routes
            .lock()
            .remove(&(kind, id.to_string()));
        let Some((_, tx)) = route else { return false };
        let submission = zhuye_protocol::Submission::with_id(uuid::Uuid::new_v4().to_string(), op);
        tx.send(submission).await.is_ok()
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
        let mut count = self.in_flight.load(Ordering::Relaxed);
        loop {
            if count >= MAX_CONCURRENT {
                return Err(SideChannelStartError::Busy);
            }
            match self.in_flight.compare_exchange_weak(
                count,
                count + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => count = actual,
            }
        }
        let (id, cancel) = self
            .registry
            .start(agent_name.to_string(), prompt.to_string());

        let registry = self.registry.clone();
        let factory: SideThreadFactory = match &self.factory {
            Factory::Plain(factory) => factory.clone(),
            Factory::Context {
                workspace,
                model,
                build,
            } => {
                let context = SideChannelContext {
                    workspace: workspace.read().clone(),
                    model: model.read().clone(),
                };
                let build = build.clone();
                Arc::new(move |agent, cancel| build(agent, cancel, context.clone()))
            }
        };
        let in_flight = self.in_flight.clone();
        let id_for_task = id.clone();
        let agent_name = agent_name.to_string();
        let prompt = prompt.to_string();
        let cancel_for_task = cancel.clone();
        let approval_bridge = self.approval_bridge.clone();

        tokio::spawn(async move {
            let _guard = InFlightGuard {
                in_flight,
                registry: registry.clone(),
                id: id_for_task.clone(),
                approval_routes: approval_bridge.routes.clone(),
            };
            let outcome = run_side_channel(
                factory,
                &agent_name,
                &prompt,
                cancel_for_task,
                &registry,
                &id_for_task,
                &approval_bridge,
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
    approvals: &ApprovalBridge,
) -> Result<Option<String>, String> {
    // 1. 构造独立 thread(工厂决定 recorder=None 等)。
    let thread = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        factory(agent_name, cancel.clone())
    }))
    .map_err(|_| "side-channel thread factory panicked".to_string())?
    .map_err(|e| e.to_string())?;
    // 2. 注入 prompt(与 cron driver 同一入口)。
    let tx = thread.submission_sender();
    let mut session_events = thread.subscribe_session();
    let sub = zhuye_protocol::Submission::user_input(prompt);
    let mut handle = thread.submit(sub).await;
    // 3. 消费事件流:收集最后一条 assistant 文本作 summary;取消 / 断开
    //    时提前结束。
    let mut summary = String::new();
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                registry.cancel(id);
                return Err("cancelled".into());
            }
            Some(event) = session_events.recv() => {
                if matches!(event.msg, zhuye_protocol::EventMsg::PlanApproved(_) | zhuye_protocol::EventMsg::PlanRejected(_)) {
                    let _ = approvals.interactions.send((id.to_string(), event.msg));
                }
            }
            event = handle.next() => {
                let Some(event) = event else {
                    return Err("side-channel event stream closed before turn completed".into());
                };
                match event.msg {
                    zhuye_protocol::EventMsg::ApprovalRequest(request) => {
                        approvals.routes.lock().insert((InteractionKind::Approval, request.request_id.clone()), (id.to_string(), tx.clone()));
                        let _ = approvals.events.send((id.to_string(), request));
                    }
                    msg @ (zhuye_protocol::EventMsg::AskUserQuestion(_) | zhuye_protocol::EventMsg::AskUserInput(_) | zhuye_protocol::EventMsg::PlanRequest(_) | zhuye_protocol::EventMsg::PlanReady(_)) => {
                        let (kind, request_id) = match &msg {
                            zhuye_protocol::EventMsg::AskUserQuestion(e) => (InteractionKind::Question, e.request_id.clone()),
                            zhuye_protocol::EventMsg::AskUserInput(e) => (InteractionKind::Input, e.request_id.clone()),
                            zhuye_protocol::EventMsg::PlanRequest(e) => (InteractionKind::Plan, e.plan_id.to_string()),
                            zhuye_protocol::EventMsg::PlanReady(e) => (InteractionKind::Plan, e.plan_id.to_string()),
                            _ => unreachable!(),
                        };
                        approvals.routes.lock().insert((kind, request_id), (id.to_string(), tx.clone()));
                        let _ = approvals.interactions.send((id.to_string(), msg));
                    }
                    zhuye_protocol::EventMsg::AgentMessageDelta(delta) => summary.push_str(&delta.delta),
                    zhuye_protocol::EventMsg::AgentMessage(message) => summary = message.text,
                    zhuye_protocol::EventMsg::ToolCallBegin(_) => summary.clear(),
                    zhuye_protocol::EventMsg::Error(error) => return Err(error.message),
                    zhuye_protocol::EventMsg::TurnAborted(aborted) => return Err(format!("turn aborted: {:?}", aborted.reason)),
                    zhuye_protocol::EventMsg::TurnComplete(complete) => {
                        if complete.status != zhuye_protocol::TurnStatus::Success {
                            return Err(format!("turn ended with status {:?}", complete.status));
                        }
                        return Ok((!summary.trim().is_empty()).then_some(summary));
                    }
                    _ => {}
                }
            }
        }
    }
}

/// 任务异常退出时回收额度并将仍运行的记录置为错误终态。
struct InFlightGuard {
    in_flight: Arc<AtomicUsize>,
    registry: SideChannelRegistry,
    id: SideChannelId,
    approval_routes: ApprovalRoutes,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
        self.approval_routes
            .lock()
            .retain(|_, (id, _)| id != &self.id);
        self.registry.finish(
            &self.id,
            SideChannelStatus::Error,
            Some("side-channel task terminated unexpectedly".into()),
        );
    }
}

#[cfg(test)]
mod tests;
