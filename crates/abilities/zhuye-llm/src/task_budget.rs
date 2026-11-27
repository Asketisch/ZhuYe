//! 任务树共享预算的原子账本。
//!
//! 账本只管理预留、结算与派发配额,调用方负责将同一实例传给整棵任务树,
//! 并在每条实际模型调用路径记录用量。预留是估算值,不代表精确计费上限。

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{Stream, StreamExt};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{ChatEvent, ChatRequest, LlmError, ModelClient};

tokio::task_local! {
    /// 当前任务树的预算账本。新任务不会自动继承,派生任务必须显式传入。
    static CURRENT_TASK_BUDGET: Arc<TaskBudget>;
}

/// 在异步任务范围内设置共享账本。
pub async fn scope_task_budget<F: std::future::Future>(
    budget: Arc<TaskBudget>,
    future: F,
) -> F::Output {
    CURRENT_TASK_BUDGET.scope(budget, future).await
}

/// 读取当前异步任务中的账本;没有配置时返回空值。
pub fn current_task_budget() -> Option<Arc<TaskBudget>> {
    CURRENT_TASK_BUDGET.try_with(Arc::clone).ok()
}

/// 任务树预算参数。
#[derive(Debug, Clone, Default)]
pub struct TaskBudgetLimits {
    /// 整棵任务树的 token 估算上限。
    pub total_tokens: Option<u64>,
    /// 仅供最终答复使用的预留额度。
    pub final_answer_tokens: u64,
    /// 从账本创建开始计算的截止时间。
    pub duration: Option<Duration>,
    /// 整棵任务树可派发的子代理总数。
    pub subagent_dispatches: Option<u64>,
}

/// 当前账本快照。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TaskBudgetSnapshot {
    /// 已成功预留的各次调用估算值之和,包含后来取消的预留。
    pub estimated_tokens: u64,
    /// 已结算的真实用量。
    pub actual_tokens: u64,
    /// 尚未结算的估算预留。
    pub reserved_tokens: u64,
    /// 已结算的请求数。
    pub settled_calls: u64,
    /// 未返回用量的已派发调用,按预留值保守估算的累计用量。
    pub unknown_estimated_tokens: u64,
    /// 已派发但没有用量回执的调用数。
    pub unknown_calls: u64,
    /// 实际用量超过该次预留的累计差额。
    pub estimate_overrun_tokens: u64,
    /// 已派发的子代理数。
    pub dispatched_subagents: u64,
    /// 尚未提交的子代理派发预留。
    pub reserved_subagents: u64,
}

/// 预留被拒绝的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TaskBudgetError {
    /// 截止时间已到。
    #[error("任务树预算已到截止时间")]
    Deadline,
    /// token 预留超出允许范围。
    #[error("任务树 token 预算不足")]
    Tokens,
    /// 子代理派发数已达上限。
    #[error("任务树子代理派发预算不足")]
    Subagents,
}

/// 被根任务和子任务共同持有的预算账本。
#[derive(Debug)]
pub struct TaskBudget {
    limits: TaskBudgetLimits,
    id: uuid::Uuid,
    started: Instant,
    state: Mutex<TaskBudgetSnapshot>,
}

impl TaskBudget {
    /// 创建任务树账本,截止时间从此时开始计算。
    pub fn new(limits: TaskBudgetLimits) -> Arc<Self> {
        Arc::new(Self {
            limits,
            id: uuid::Uuid::new_v4(),
            started: Instant::now(),
            state: Mutex::new(TaskBudgetSnapshot::default()),
        })
    }

    /// 读取一致的账本快照。
    pub fn snapshot(&self) -> TaskBudgetSnapshot {
        *self.state.lock()
    }

    /// 整棵任务树共用的账本标识。
    pub fn id(&self) -> uuid::Uuid {
        self.id
    }

    /// 按当前账本估算可用于下一次预留的 token 数;并发预留可能立即改变该值。
    pub fn available_tokens(&self, final_answer: bool) -> Option<u64> {
        let limit = self.limits.total_tokens?;
        let allowed = if final_answer {
            limit
        } else {
            limit.saturating_sub(self.limits.final_answer_tokens)
        };
        let state = self.state.lock();
        Some(
            allowed
                .saturating_sub(state.actual_tokens)
                .saturating_sub(state.unknown_estimated_tokens)
                .saturating_sub(state.reserved_tokens),
        )
    }

    /// 是否超过截止时间。
    pub fn expired(&self) -> bool {
        self.limits
            .duration
            .is_some_and(|duration| self.started.elapsed() >= duration)
    }

    /// 距截止时间的剩余时长;未配置截止时间时为空。
    pub fn remaining_time(&self) -> Option<Duration> {
        self.limits
            .duration
            .map(|duration| duration.saturating_sub(self.started.elapsed()))
    }

    /// 原子预留一次模型调用的估算用量。
    ///
    /// 普通调用不能占用最终答复保留额度。拒绝时不改变账本;
    /// 返回的句柄若未结算就被丢弃,会释放预留。
    pub fn reserve(
        self: &Arc<Self>,
        estimate: u64,
        final_answer: bool,
    ) -> Result<TokenReservation, TaskBudgetError> {
        let mut state = self.state.lock();
        if self.expired() {
            return Err(TaskBudgetError::Deadline);
        }
        // 修复原因:无限额模式也允许多个并发预留,饱和加法会丢失超出
        // u64 的部分,使句柄释放时发生下溢;无法表示时直接拒绝预留。
        let reserved = state
            .reserved_tokens
            .checked_add(estimate)
            .ok_or(TaskBudgetError::Tokens)?;
        if let Some(limit) = self.limits.total_tokens {
            let allowed = if final_answer {
                limit
            } else {
                limit.saturating_sub(self.limits.final_answer_tokens)
            };
            let total = state
                .actual_tokens
                .checked_add(state.unknown_estimated_tokens)
                .and_then(|used| used.checked_add(reserved))
                .ok_or(TaskBudgetError::Tokens)?;
            if total > allowed {
                return Err(TaskBudgetError::Tokens);
            }
        }
        state.reserved_tokens = reserved;
        state.estimated_tokens = state.estimated_tokens.saturating_add(estimate);
        Ok(TokenReservation {
            budget: self.clone(),
            estimate,
            settled: false,
            dispatched: false,
            observed: None,
        })
    }

    /// 原子预留一次子代理派发;仅真正派发后调用 `commit`。
    pub fn reserve_subagent(self: &Arc<Self>) -> Result<DispatchReservation, TaskBudgetError> {
        let mut state = self.state.lock();
        if self.expired() {
            return Err(TaskBudgetError::Deadline);
        }
        if self.limits.subagent_dispatches.is_some_and(|limit| {
            state
                .dispatched_subagents
                .saturating_add(state.reserved_subagents)
                >= limit
        }) {
            return Err(TaskBudgetError::Subagents);
        }
        state.reserved_subagents = state
            .reserved_subagents
            .checked_add(1)
            .ok_or(TaskBudgetError::Subagents)?;
        Ok(DispatchReservation {
            budget: self.clone(),
            committed: false,
        })
    }
}

/// 一次模型调用的预留;丢弃时释放未结算额度。
#[derive(Debug)]
pub struct TokenReservation {
    budget: Arc<TaskBudget>,
    estimate: u64,
    settled: bool,
    dispatched: bool,
    observed: Option<u64>,
}

impl TokenReservation {
    /// 返回本次预留的估算 token 数。
    pub fn estimate(&self) -> u64 {
        self.estimate
    }

    /// 即将发起实际请求时调用;未收到用量的请求会单独按估算值登记。
    pub fn mark_dispatched(&mut self) {
        self.dispatched = true;
    }

    /// 缓存最新的用量快照;取消或流中断时仍按已收到的回执结算。
    pub fn observe_usage(&mut self, actual: u64) {
        self.observed = Some(actual);
    }

    /// 用 provider 报告的实际 token 数结算,缓存子集不得重复相加。
    pub fn settle(mut self, actual: u64) {
        let mut state = self.budget.state.lock();
        state.reserved_tokens -= self.estimate;
        state.actual_tokens = state.actual_tokens.saturating_add(actual);
        state.estimate_overrun_tokens = state
            .estimate_overrun_tokens
            .saturating_add(actual.saturating_sub(self.estimate));
        state.settled_calls = state.settled_calls.saturating_add(1);
        self.settled = true;
    }
}

impl Drop for TokenReservation {
    fn drop(&mut self) {
        if !self.settled {
            let mut state = self.budget.state.lock();
            state.reserved_tokens -= self.estimate;
            if self.dispatched {
                if let Some(actual) = self.observed {
                    state.actual_tokens = state.actual_tokens.saturating_add(actual);
                    state.estimate_overrun_tokens = state
                        .estimate_overrun_tokens
                        .saturating_add(actual.saturating_sub(self.estimate));
                    state.settled_calls = state.settled_calls.saturating_add(1);
                } else {
                    // 修复原因:流取消或网络中断不等于上游未计费。没有回执时
                    // 释放在途预留并单独保守登记估算,避免反复中断绕过预算。
                    state.unknown_estimated_tokens =
                        state.unknown_estimated_tokens.saturating_add(self.estimate);
                    state.unknown_calls = state.unknown_calls.saturating_add(1);
                }
            }
        }
    }
}

/// 一次尚未提交的子代理派发预留。
#[derive(Debug)]
pub struct DispatchReservation {
    budget: Arc<TaskBudget>,
    committed: bool,
}

impl DispatchReservation {
    /// 记录成功派发。
    pub fn commit(mut self) {
        let mut state = self.budget.state.lock();
        state.reserved_subagents -= 1;
        state.dispatched_subagents = state.dispatched_subagents.saturating_add(1);
        self.committed = true;
    }
}

impl Drop for DispatchReservation {
    fn drop(&mut self) {
        if !self.committed {
            self.budget.state.lock().reserved_subagents -= 1;
        }
    }
}

/// 带预算的模型请求错误。
#[derive(Debug, thiserror::Error)]
pub enum BudgetedCallError {
    /// 请求未能取得预留,不会调用模型。
    #[error(transparent)]
    Budget(#[from] TaskBudgetError),
    /// 模型调用失败。
    #[error(transparent)]
    Model(#[from] LlmError),
}

/// 预留估算用量后发起模型流,在流结束或丢弃时自动结算。
///
/// `fallback_output_tokens` 用于请求未设置输出上限的情况;实际用量可能
/// 超过估算,调用方应将账本快照和超额情况提供给宿主。
pub async fn stream_with_task_budget(
    client: &dyn ModelClient,
    request: ChatRequest,
    cancel: CancellationToken,
    budget: &Arc<TaskBudget>,
    fallback_output_tokens: u32,
    final_answer: bool,
) -> Result<
    std::pin::Pin<Box<dyn Stream<Item = Result<ChatEvent, LlmError>> + Send>>,
    BudgetedCallError,
> {
    let input = u64::from(crate::output_budget::estimate_input_tokens(&request));
    let output = u64::from(request.max_tokens.unwrap_or(fallback_output_tokens));
    let mut reservation = budget.reserve(input.saturating_add(output), final_answer)?;
    if cancel.is_cancelled() {
        return Err(BudgetedCallError::Model(LlmError::Cancelled));
    }
    reservation.mark_dispatched();
    let stream = client.stream(request, cancel).await?;
    let tracked = futures::stream::unfold(
        (stream, reservation),
        |(mut stream, mut reservation)| async move {
            let item = stream.next().await?;
            if let Ok(ChatEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            }) = &item
            {
                reservation.observe_usage(u64::from(*input_tokens) + u64::from(*output_tokens));
            }
            Some((item, (stream, reservation)))
        },
    );
    Ok(Box::pin(tracked))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn 并发预留不可超过普通调用额度() {
        let budget = TaskBudget::new(TaskBudgetLimits {
            total_tokens: Some(100),
            final_answer_tokens: 20,
            ..Default::default()
        });
        let barrier = Arc::new(Barrier::new(9));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let budget = budget.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                let reservation = budget.reserve(30, false);
                barrier.wait();
                reservation
            }));
        }
        barrier.wait();
        let reservations: Vec<_> = handles
            .into_iter()
            .filter_map(|h| h.join().unwrap().ok())
            .collect();
        assert_eq!(reservations.len(), 2);
        assert_eq!(budget.snapshot().reserved_tokens, 60);
        drop(reservations);
        assert_eq!(budget.snapshot().reserved_tokens, 0);
    }

    #[test]
    fn 结算超额与最终答复保留可观察() {
        let budget = TaskBudget::new(TaskBudgetLimits {
            total_tokens: Some(100),
            final_answer_tokens: 20,
            ..Default::default()
        });
        budget.reserve(70, false).unwrap().settle(75);
        assert_eq!(
            budget.reserve(6, false).unwrap_err(),
            TaskBudgetError::Tokens
        );
        budget.reserve(20, true).unwrap().settle(23);
        assert_eq!(
            budget.snapshot(),
            TaskBudgetSnapshot {
                estimated_tokens: 90,
                actual_tokens: 98,
                settled_calls: 2,
                estimate_overrun_tokens: 8,
                ..Default::default()
            }
        );
        assert_eq!(
            budget.reserve(3, true).unwrap_err(),
            TaskBudgetError::Tokens
        );
    }

    #[test]
    fn 派发失败释放额度而成功计入总数() {
        let budget = TaskBudget::new(TaskBudgetLimits {
            subagent_dispatches: Some(1),
            ..Default::default()
        });
        let pending = budget.reserve_subagent().unwrap();
        assert_eq!(
            budget.reserve_subagent().unwrap_err(),
            TaskBudgetError::Subagents
        );
        drop(pending);
        budget.reserve_subagent().unwrap().commit();
        assert_eq!(budget.snapshot().dispatched_subagents, 1);
        assert_eq!(
            budget.reserve_subagent().unwrap_err(),
            TaskBudgetError::Subagents
        );
    }

    #[test]
    fn 截止时间拒绝所有新预留() {
        let budget = TaskBudget::new(TaskBudgetLimits {
            duration: Some(Duration::ZERO),
            ..Default::default()
        });
        assert_eq!(
            budget.reserve(1, false).unwrap_err(),
            TaskBudgetError::Deadline
        );
        assert_eq!(
            budget.reserve_subagent().unwrap_err(),
            TaskBudgetError::Deadline
        );
    }

    #[test]
    fn 中断时保留已知用量并单独统计未知用量() {
        let budget = TaskBudget::new(TaskBudgetLimits {
            total_tokens: Some(100),
            ..Default::default()
        });
        let mut partial = budget.reserve(40, false).unwrap();
        partial.mark_dispatched();
        partial.observe_usage(12);
        drop(partial);
        let mut unknown = budget.reserve(30, false).unwrap();
        unknown.mark_dispatched();
        drop(unknown);
        let snapshot = budget.snapshot();
        assert_eq!(snapshot.actual_tokens, 12);
        assert_eq!(snapshot.unknown_estimated_tokens, 30);
        assert_eq!(snapshot.unknown_calls, 1);
        assert_eq!(snapshot.reserved_tokens, 0);
        assert_eq!(
            budget.reserve(59, false).unwrap_err(),
            TaskBudgetError::Tokens
        );
        assert!(budget.reserve(58, false).is_ok());
    }

    #[test]
    fn 预留计数溢出时拒绝而不破坏已存在的句柄() {
        let budget = TaskBudget::new(TaskBudgetLimits::default());
        let reservation = budget.reserve(u64::MAX, false).unwrap();
        assert_eq!(
            budget.reserve(1, false).unwrap_err(),
            TaskBudgetError::Tokens
        );
        drop(reservation);
        assert_eq!(budget.snapshot().reserved_tokens, 0);
    }

    #[tokio::test]
    async fn 模型流先预留并按用量回执结算() {
        let client = crate::MockClient::new();
        let request = ChatRequest {
            max_tokens: Some(8),
            ..Default::default()
        };
        let denied = TaskBudget::new(TaskBudgetLimits {
            total_tokens: Some(1),
            ..Default::default()
        });
        assert!(matches!(
            stream_with_task_budget(
                &client,
                request.clone(),
                CancellationToken::new(),
                &denied,
                8,
                false
            )
            .await,
            Err(BudgetedCallError::Budget(TaskBudgetError::Tokens))
        ));
        assert!(client.recorded_requests().is_empty());

        let budget = TaskBudget::new(TaskBudgetLimits {
            total_tokens: Some(1000),
            ..Default::default()
        });
        let mut stream = stream_with_task_budget(
            &client,
            request,
            CancellationToken::new(),
            &budget,
            8,
            false,
        )
        .await
        .unwrap();
        assert!(budget.snapshot().reserved_tokens > 0);
        while stream.next().await.is_some() {}
        let snapshot = budget.snapshot();
        assert_eq!(snapshot.reserved_tokens, 0);
        assert_eq!(snapshot.settled_calls, 1);
        assert_eq!(snapshot.unknown_calls, 0);
        assert_eq!(client.recorded_requests().len(), 1);
    }

    #[tokio::test]
    async fn 失败后重试分别记账且不会重复累计用量() {
        let budget = TaskBudget::new(TaskBudgetLimits {
            total_tokens: Some(1000),
            ..Default::default()
        });
        let request = ChatRequest {
            max_tokens: Some(8),
            ..Default::default()
        };
        let failed = crate::MockClient::with_config_error("配置错误".into());
        assert!(
            stream_with_task_budget(
                &failed,
                request.clone(),
                CancellationToken::new(),
                &budget,
                8,
                false
            )
            .await
            .is_err()
        );
        let client = crate::MockClient::new();
        let mut stream = stream_with_task_budget(
            &client,
            request,
            CancellationToken::new(),
            &budget,
            8,
            false,
        )
        .await
        .unwrap();
        while stream.next().await.is_some() {}
        let snapshot = budget.snapshot();
        assert_eq!(snapshot.actual_tokens, 4);
        assert_eq!(snapshot.settled_calls, 1);
        assert_eq!(snapshot.unknown_calls, 1);
        assert_eq!(snapshot.unknown_estimated_tokens, 8);
        assert_eq!(snapshot.reserved_tokens, 0);
    }
}
