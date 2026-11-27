//! Prompt cache 保温(cache warming)调度器。
//!
//! 职责切分:数字(该不该保温、延时多久)在 `zhuye_llm::cache_warming`
//! 纯函数侧;本模块管调度 —— 何时问、何时发、何时取消,以及与引擎
//! 状态的联动:
//!
//! 1. **候选槽**:[`crate::graph::nodes::model_call`] 成功后写入「上次
//!    真实请求」的逐字节克隆 + 同一凭据的 client 句柄 + 缓存观测回执。
//!    保温重放 = 克隆请求仅改 `max_tokens=1` 并加 `warm_replay` 标记,
//!    仍用同一 client(同凭据同缓存)发送 —— 前缀字节一致,必命中。
//! 2. **单飞**:同时至多一个保温任务;新调度顶掉旧任务(独立 child
//!    token,随 `AgentConfig::cancel` 在 Shutdown 级联取消)。
//! 3. **代际失效**:任何改变下一请求 wire 前缀的变更(rewind / compact /
//!    model / effort / tools / prompt 切换 / 配置重载)都 bump
//!    `AgentConfig::cache_generation`;fire 时比对候选代际,不一致即弃
//!    (skip `stale`)。注:空闲期没有 model_call 喂
//!    `CacheBreakDetector`,其 change_count 在空闲期天然不变,
//!    「前缀是否漂移」由代际计数在全部变更点显式兜住。
//! 4. **零污染不变式**:不写 rollout、不触发 hook、不进
//!    `session_usage`、不发 `TokenCount`、不喂 `CacheBreakDetector`
//!    (旁路 model_call 天然成立);花费只进 quota_tracker、本模块
//!    计数与 `CacheWarming` 事件 —— 既有「从 TokenCount 流算命中率 /
//!    成本」的口径零扰动。
//!
//! 事件经可选 `event_tx` 出口送 submission_loop 的 forwarder(stamp
//! seq 后 `fan_out_session`);无出口时静默(库路径 / 测试)。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use parking_lot::{Mutex, RwLock};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zhuye_llm::client::BoxedModelClient;
use zhuye_llm::event::UsageSnapshot;
use zhuye_llm::{
    CacheObservation, CacheWarmingSettings, ChatEvent, SkipReason, WarmingAction, WarmingPhase,
    effective_ttl_secs, evaluate, refresh_delay_secs,
};
use zhuye_protocol::{
    CacheWarmingAction, CacheWarmingEvent, CacheWarmingPhase, EVENT_ID_NONE, Event, EventMsg,
    TokenUsage, TurnId,
};

/// 候选请求:上次真实模型调用的保温蓝本。由 `model_call` 成功后写入。
#[derive(Clone)]
pub struct WarmCandidate {
    /// 上次真实调用的 wire 请求(已 `fold_system_messages` + 注入缓存
    /// 断点后的形态)。重放逐字节复刻本字段。
    pub request: zhuye_llm::ChatRequest,
    /// 完整 model spec(如 `"anthropic/claude-sonnet-4-latest"`)。
    pub model_spec: String,
    /// 上次请求的 `input_tokens`(含缓存段的总量)。
    pub prompt_tokens: u32,
    /// 上次请求的缓存段回执(read + write),决策的观测输入。
    pub observation: CacheObservation,
    /// 上次调用实际使用的 client(**同凭据同缓存**,重放必须走它,
    /// 不经 `ModelRegistry` 轮询)。
    pub client: BoxedModelClient,
    /// 该凭据在池内的 label(quota 记账用)。
    pub credential_label: String,
    /// 写入时的缓存代际快照;fire 时与当前代际比对。
    pub generation: u64,
    /// 候选发起时刻(= 缓存写入时刻,TTL 与窗口的计时原点)。
    pub created_at: Instant,
    /// 该候选是否可被 1-token 重放(provider 按 wire 形态判定;
    /// Anthropic 显式 thinking 不可重放)。
    pub replayable: bool,
    /// 创建候选的根任务账本;保温任务是新任务,必须显式继承。
    pub task_budget: Option<Arc<zhuye_llm::task_budget::TaskBudget>>,
    /// 触发保温候选的回合,用于空闲结算的账本事件。
    pub turn_id: TurnId,
}

/// 保温运行计数(可观测 / 测试断言)。
#[derive(Debug, Default)]
pub struct WarmingStats {
    /// 成功执行的重放次数。
    pub executed: AtomicU64,
    /// 决策不保温(含 stale / 窗口耗尽等)次数。
    pub skipped: AtomicU64,
    /// 在途重放被取消次数。
    pub cancelled: AtomicU64,
    /// 累计保温成本,微 USD(原子整数避免浮点原子;成本未知不计)。
    pub warm_cost_micro_usd: AtomicU64,
}

/// 保温调度器。常驻 `AgentConfig`(默认 disabled,零行为);开启后
/// 由 `model_call` 喂候选、turn 边界排程。
pub struct CacheWarmingScheduler {
    settings: Arc<RwLock<CacheWarmingSettings>>,
    /// 会话父令牌:Shutdown 级联取消保温任务。
    parent_cancel: CancellationToken,
    /// 缓存代际(与 `AgentConfig::cache_generation` 共享同一 Arc)。
    generation: Arc<AtomicU64>,
    candidate: Mutex<Option<WarmCandidate>>,
    /// 在飞保温任务的取消令牌(单飞;新调度顶掉旧的)。
    active_cancel: Mutex<Option<CancellationToken>>,
    /// 事件出口(submission_loop 挂 forwarder;None = 静默)。
    event_tx: Mutex<Option<mpsc::Sender<Event>>>,
    /// 保温花费入配额(与真实请求同一 tracker;不进 session_usage)。
    quota: Mutex<Option<zhuye_llm::SharedQuotaTracker>>,
    /// 本地遥测(model-io 落一条 synthetic 标记的记录)。
    telemetry: Mutex<Option<Arc<zhuye_telemetry::TelemetrySink>>>,
    stats: Arc<WarmingStats>,
}

impl std::fmt::Debug for CacheWarmingScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheWarmingScheduler")
            .field("enabled", &self.settings.read().enabled)
            .finish_non_exhaustive()
    }
}

impl CacheWarmingScheduler {
    /// 构造调度器。`settings.enabled = false` 时全部行为为 no-op。
    pub fn new(
        settings: CacheWarmingSettings,
        parent_cancel: CancellationToken,
        generation: Arc<AtomicU64>,
    ) -> Self {
        Self {
            settings: Arc::new(RwLock::new(settings)),
            parent_cancel,
            generation,
            candidate: Mutex::new(None),
            active_cancel: Mutex::new(None),
            event_tx: Mutex::new(None),
            quota: Mutex::new(None),
            telemetry: Mutex::new(None),
            stats: Arc::new(WarmingStats::default()),
        }
    }

    /// 当前设置快照。
    pub fn settings(&self) -> CacheWarmingSettings {
        self.settings.read().clone()
    }

    /// 热更新设置(配置重载 / Builder)。关闭时取消在飞保温。
    pub fn update_settings(&self, settings: CacheWarmingSettings) {
        let was_enabled = self.settings.read().enabled;
        *self.settings.write() = settings.clone();
        if was_enabled && !settings.enabled {
            self.cancel_active();
        }
    }

    /// 挂事件出口(submission_loop 启动时调用;满则丢,不阻塞引擎)。
    pub fn set_event_tx(&self, tx: mpsc::Sender<Event>) {
        *self.event_tx.lock() = Some(tx);
    }

    /// 挂 quota tracker(保温花费计入与真实请求同一窗口)。
    pub fn set_quota(&self, quota: zhuye_llm::SharedQuotaTracker) {
        *self.quota.lock() = Some(quota);
    }

    /// 挂 telemetry sink(model-io 落 synthetic 标记记录)。
    pub fn set_telemetry(&self, sink: Arc<zhuye_telemetry::TelemetrySink>) {
        *self.telemetry.lock() = Some(sink);
    }

    /// 运行计数快照句柄(测试 / 诊断)。
    pub fn stats(&self) -> Arc<WarmingStats> {
        Arc::clone(&self.stats)
    }

    /// 记录候选(上次真实调用成功后)。settings 未开启时跳过存储
    /// (零开销;热开启后等下一次真实调用自然补上候选)。
    pub fn record_candidate(&self, cand: WarmCandidate) {
        if !self.settings.read().enabled {
            return;
        }
        *self.candidate.lock() = Some(cand);
    }

    /// 候选槽是否非空(诊断 / 测试)。
    pub fn has_candidate(&self) -> bool {
        self.candidate.lock().is_some()
    }

    /// 排程一轮保温(单飞:顶掉在飞任务)。Turn 阶段静默(每步工具
    /// 后都调用,发事件会是噪声);Idle 阶段由 run 循环发 `scheduled`。
    /// settings 未开启 / 候选缺失 / 候选不可重放时 no-op。
    pub fn schedule(self: &Arc<Self>, phase: WarmingPhase) {
        let settings = self.settings();
        if !settings.enabled {
            return;
        }
        {
            let cand = self.candidate.lock();
            match cand.as_ref() {
                Some(c) if c.replayable => {}
                _ => return,
            }
        }
        // 单飞:旧任务 cancel 后在新任务循环首轮被感知,不再竞争。
        let cancel = self.parent_cancel.child_token();
        if let Some(old) = self.active_cancel.lock().replace(cancel.clone()) {
            old.cancel();
        }
        tokio::spawn(Arc::clone(self).run(phase, cancel));
    }

    /// 取消在飞保温任务(新真实请求 / 回合异常 / 配置关闭)。
    /// 候选槽保留:前缀本身未失效,下次排程仍可复用(代际兜底)。
    pub fn cancel_active(&self) {
        if let Some(token) = self.active_cancel.lock().take() {
            token.cancel();
        }
    }

    /// 保温主循环:睡到刷新点 → 复查(设置 / 代际 / 窗口 / TTL)→
    /// 决策 → 重放或跳过 → 续排,直到窗口耗尽 / 取消 / 决策 Stop。
    async fn run(self: Arc<Self>, phase: WarmingPhase, cancel: CancellationToken) {
        // Idle 排程建立事件:每个 turn 完成后至多一次,客户端可显示
        // 「保温已布防」。Turn 阶段不发(频率太高)。
        if phase == WarmingPhase::Idle {
            self.emit(
                CacheWarmingAction::Scheduled,
                phase,
                None,
                None,
                None,
                None,
                None,
                None,
            );
        }
        // TTL 计时原点:候选写入时刻(缓存最后写入),每次成功重放后
        // 前移到重放完成时刻(重放命中刷新了整条前缀的 TTL)。
        let mut last_refresh = self
            .candidate
            .lock()
            .as_ref()
            .map_or_else(Instant::now, |c| c.created_at);
        loop {
            if cancel.is_cancelled() {
                return;
            }
            let settings = self.settings();
            if !settings.enabled {
                return;
            }
            let Some(cand) = self.candidate.lock().clone() else {
                return;
            };
            if cand.generation != self.generation.load(Ordering::Relaxed) {
                self.stats.skipped.fetch_add(1, Ordering::Relaxed);
                self.emit(
                    CacheWarmingAction::Skipped,
                    phase,
                    Some("stale"),
                    Some(&cand.model_spec),
                    Some(cand.prompt_tokens),
                    None,
                    None,
                    None,
                );
                return;
            }
            // 窗口:距候选发起超过本阶段上限后,延续估计不再可靠。
            if cand.created_at.elapsed().as_secs() >= phase_window(phase, &settings) {
                self.stats.skipped.fetch_add(1, Ordering::Relaxed);
                self.emit(
                    CacheWarmingAction::Skipped,
                    phase,
                    Some("window_exhausted"),
                    Some(&cand.model_spec),
                    Some(cand.prompt_tokens),
                    None,
                    None,
                    None,
                );
                return;
            }
            let ttl = effective_ttl_secs(&cand.request, &settings);
            let Some(delay) = refresh_delay_secs(ttl, &settings) else {
                self.stats.skipped.fetch_add(1, Ordering::Relaxed);
                self.emit(
                    CacheWarmingAction::Skipped,
                    phase,
                    Some("ttl_too_short"),
                    Some(&cand.model_spec),
                    Some(cand.prompt_tokens),
                    None,
                    None,
                    None,
                );
                return;
            };
            // 等到刷新点(TTL 的 90% − 边际)。等待中随时可被取消。
            let since_refresh = last_refresh.elapsed();
            let wait = Duration::from_secs(delay).saturating_sub(since_refresh);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(wait) => {}
            }
            // 醒来后全部状态重读(设置可能热更、候选可能被替换)。
            let settings = self.settings();
            if !settings.enabled || cancel.is_cancelled() {
                return;
            }
            let Some(cand) = self.candidate.lock().clone() else {
                return;
            };
            if cand.generation != self.generation.load(Ordering::Relaxed) {
                self.stats.skipped.fetch_add(1, Ordering::Relaxed);
                self.emit(
                    CacheWarmingAction::Skipped,
                    phase,
                    Some("stale"),
                    Some(&cand.model_spec),
                    Some(cand.prompt_tokens),
                    None,
                    None,
                    None,
                );
                return;
            }
            let age_secs = cand.created_at.elapsed().as_secs();
            // 缓存条目已死(age ≥ TTL):保温等于全价 prefill 现在付一次、
            // 下次再付一次,纯浪费 —— 停到下一次真实请求。
            if age_secs >= ttl {
                self.stats.skipped.fetch_add(1, Ordering::Relaxed);
                self.emit(
                    CacheWarmingAction::Skipped,
                    phase,
                    Some("cache_expired"),
                    Some(&cand.model_spec),
                    Some(cand.prompt_tokens),
                    None,
                    None,
                    None,
                );
                return;
            }
            if age_secs >= phase_window(phase, &settings) {
                self.stats.skipped.fetch_add(1, Ordering::Relaxed);
                self.emit(
                    CacheWarmingAction::Skipped,
                    phase,
                    Some("window_exhausted"),
                    Some(&cand.model_spec),
                    Some(cand.prompt_tokens),
                    None,
                    None,
                    None,
                );
                return;
            }
            let decision = evaluate(
                &cand.model_spec,
                cand.prompt_tokens,
                cand.observation,
                phase,
                age_secs,
                &settings,
            );
            if decision.action != WarmingAction::Warm {
                self.stats.skipped.fetch_add(1, Ordering::Relaxed);
                self.emit(
                    CacheWarmingAction::Skipped,
                    phase,
                    decision.skip_reason.as_ref().map(SkipReason::as_str),
                    Some(&cand.model_spec),
                    Some(cand.prompt_tokens),
                    None,
                    decision.warm_cost_usd,
                    decision.expected_savings_usd,
                );
                return;
            }
            // 重放请求:逐字节复刻候选,仅改 max_tokens=1 + warm_replay
            // 标记(真实 provider 忽略未知 metadata 键,wire 前缀不变)。
            let mut replay = cand.request.clone();
            replay.max_tokens = Some(1);
            replay.metadata.insert(
                zhuye_llm::METADATA_WARM_REPLAY.into(),
                zhuye_llm::METADATA_WARM_REPLAY_VALUE.into(),
            );
            let replay_started = Instant::now();
            // 修复原因:空闲保温是独立任务,根回合结束后不再有截止时间
            // 监督。已发出的重放也必须在同一截止时间停止并释放预留。
            let remaining = cand
                .task_budget
                .as_ref()
                .and_then(|budget| budget.remaining_time());
            let deadline = async {
                if let Some(remaining) = remaining {
                    tokio::time::sleep_until(
                        tokio::time::Instant::from_std(replay_started) + remaining,
                    )
                    .await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::pin!(deadline);
            let report_budget = || {
                if phase == WarmingPhase::Idle
                    && let Some(budget) = cand.task_budget.as_ref()
                    && let Some(tx) = self.event_tx.lock().as_ref()
                {
                    let _ = tx.try_send(Event::new(
                        EVENT_ID_NONE,
                        crate::submission_loop::task_budget_snapshot(budget, cand.turn_id),
                    ));
                }
            };
            let report_deadline = || {
                cancel.cancel();
                self.stats.cancelled.fetch_add(1, Ordering::Relaxed);
                self.emit(
                    CacheWarmingAction::Cancelled,
                    phase,
                    Some("task_deadline"),
                    Some(&cand.model_spec),
                    Some(cand.prompt_tokens),
                    None,
                    None,
                    None,
                );
            };
            let stream_result = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    // 在途被取消(新请求到达):发一次 cancelled 事件。
                    self.stats.cancelled.fetch_add(1, Ordering::Relaxed);
                    self.emit(
                        CacheWarmingAction::Cancelled,
                        phase,
                        Some("cancelled_by_request"),
                        Some(&cand.model_spec),
                        Some(cand.prompt_tokens),
                        None,
                        None,
                        None,
                    );
                    report_budget();
                    return;
                }
                _ = &mut deadline => {
                    report_deadline();
                    report_budget();
                    return;
                }
                result = async {
                    if let Some(budget) = cand.task_budget.as_ref() {
                        zhuye_llm::task_budget::stream_with_task_budget(
                            cand.client.as_ref(), replay, cancel.clone(), budget, 1, false,
                        ).await
                    } else {
                        cand.client.stream(replay, cancel.clone()).await
                            .map_err(zhuye_llm::task_budget::BudgetedCallError::Model)
                    }
                } => result,
            };
            let Ok(mut s) = stream_result else {
                // 建流失败(凭据冷却 / 网络抖动):单次尝试,不重试
                // (失败本身不打扰主流程;下一次真实请求会重建缓存)。
                self.stats.skipped.fetch_add(1, Ordering::Relaxed);
                self.emit(
                    CacheWarmingAction::Skipped,
                    phase,
                    Some("replay_failed"),
                    Some(&cand.model_spec),
                    Some(cand.prompt_tokens),
                    None,
                    None,
                    None,
                );
                report_budget();
                return;
            };
            use futures::StreamExt;
            // 排空重放流,捕获最后的 usage 帧(取消随时可中断)。
            let mut usage: Option<UsageSnapshot> = None;
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        drop(s);
                        self.stats.cancelled.fetch_add(1, Ordering::Relaxed);
                        self.emit(
                            CacheWarmingAction::Cancelled,
                            phase,
                            Some("cancelled_by_request"),
                            Some(&cand.model_spec),
                            Some(cand.prompt_tokens),
                            None,
                            None,
                            None,
                        );
                        report_budget();
                        return;
                    }
                    _ = &mut deadline => {
                        drop(s);
                        report_deadline();
                        report_budget();
                        return;
                    }
                    item = s.next() => match item {
                        Some(Ok(ChatEvent::Usage { input_tokens, output_tokens, cached_tokens, cache_write_tokens })) => {
                            usage = Some(UsageSnapshot { input_tokens, output_tokens, cached_tokens, cache_write_tokens });
                        }
                        Some(Ok(_)) => {}
                        // 流内错误 / 流结束:以已捕获的 usage 收场(可能为 None)。
                        Some(Err(_)) | None => break,
                    }
                }
            }
            // 流内报错会提前退出,主动析构保证快照读到已结算账本。
            drop(s);
            // 记账:quota(与真实请求同窗口)+ 本模块计数 + 事件。
            // **绝不**进 session_usage / state / rollout / TokenCount。
            let (warm_cached, warm_cost) = match usage {
                Some(u) => {
                    let bare = cand
                        .model_spec
                        .split_once('/')
                        .map_or(cand.model_spec.as_str(), |(_, m)| m);
                    let cost = zhuye_llm::providers::price(
                        bare,
                        &TokenUsage {
                            input_tokens: u.input_tokens,
                            output_tokens: u.output_tokens,
                            cached_tokens: u.cached_tokens,
                            cache_write_tokens: u.cache_write_tokens,
                            total_tokens: u.input_tokens.saturating_add(u.output_tokens),
                        },
                    );
                    let pool = cand
                        .model_spec
                        .split_once('/')
                        .map_or(cand.model_spec.as_str(), |(p, _)| p);
                    if let Some(q) = self.quota.lock().clone() {
                        q.record_usage(
                            pool,
                            &cand.credential_label,
                            u.input_tokens.saturating_add(u.output_tokens) as u64,
                        );
                    }
                    (Some(u.cached_tokens), cost)
                }
                None => (None, None),
            };
            self.stats.executed.fetch_add(1, Ordering::Relaxed);
            if let Some(c) = warm_cost {
                self.stats
                    .warm_cost_micro_usd
                    .fetch_add((c * 1_000_000.0) as u64, Ordering::Relaxed);
            }
            self.record_telemetry(&cand, usage, warm_cost, replay_started);
            self.emit(
                CacheWarmingAction::Executed,
                phase,
                None,
                Some(&cand.model_spec),
                Some(cand.prompt_tokens),
                warm_cached,
                warm_cost,
                decision.expected_savings_usd,
            );
            report_budget();
            // 重放命中刷新了整条前缀的 TTL → 下一个刷新点从本次重放起算。
            last_refresh = Instant::now();
        }
    }

    /// model-io 遥测:synthetic 标记的保温记录(source = "cache_warming"),
    /// 与真实请求可区分。失败仅 debug,不影响保温。
    fn record_telemetry(
        &self,
        cand: &WarmCandidate,
        usage: Option<UsageSnapshot>,
        cost: Option<f64>,
        started: Instant,
    ) {
        let Some(sink) = self.telemetry.lock().clone() else {
            return;
        };
        if !sink.enabled() {
            return;
        }
        let bare = cand
            .model_spec
            .split_once('/')
            .map_or(cand.model_spec.as_str(), |(_, m)| m);
        let pool = cand
            .model_spec
            .split_once('/')
            .map_or(cand.model_spec.as_str(), |(p, _)| p);
        let u = usage.unwrap_or_default();
        let model_ref = zhuye_telemetry::ModelRef {
            model_id: bare.to_string(),
            provider_id: Some(pool.to_string()),
            role: Some("main".into()),
            source: Some("cache_warming".into()),
        };
        let req_record = serde_json::json!({
            "model": cand.model_spec,
            "warm_replay": true,
            "prompt_tokens": cand.prompt_tokens,
            "messages": serde_json::to_value(&cand.request.messages).unwrap_or(serde_json::Value::Null),
            "system": serde_json::to_value(&cand.request.system).unwrap_or(serde_json::Value::Null),
        });
        let resp_record = serde_json::json!({
            "warm_replay": true,
            "finish_reason": "stop",
        });
        sink.record_model_call(
            None,
            None,
            model_ref,
            req_record,
            resp_record,
            zhuye_telemetry::UsageSnapshot {
                input_tokens: u.input_tokens as u64,
                output_tokens: u.output_tokens as u64,
                cached_tokens: u.cached_tokens as u64,
                cache_write_tokens: u.cache_write_tokens as u64,
                total_tokens: u.input_tokens.saturating_add(u.output_tokens) as u64,
                cost_usd: cost,
            },
            started.elapsed().as_millis() as u64,
            1,
            zhuye_telemetry::SpanStatus::Completed,
            "cache_warming",
        );
    }

    /// 发一条 `CacheWarming` 事件(try_send,满则丢;无出口则静默)。
    #[allow(clippy::too_many_arguments)]
    fn emit(
        &self,
        action: CacheWarmingAction,
        phase: WarmingPhase,
        reason: Option<&str>,
        model: Option<&str>,
        prompt_tokens: Option<u32>,
        warm_cached_tokens: Option<u32>,
        warm_cost_usd: Option<f64>,
        expected_savings_usd: Option<f64>,
    ) {
        let Some(tx) = self.event_tx.lock().clone() else {
            return;
        };
        let phase = match phase {
            WarmingPhase::Turn => CacheWarmingPhase::Turn,
            WarmingPhase::Idle => CacheWarmingPhase::Idle,
        };
        let ev = Event::new(
            EVENT_ID_NONE,
            EventMsg::CacheWarming(CacheWarmingEvent {
                action,
                phase,
                reason: reason.map(str::to_string),
                prompt_tokens,
                warm_cached_tokens,
                warm_cost_usd,
                expected_savings_usd,
                model: model.map(str::to_string),
                at: SystemTime::now(),
            }),
        );
        let _ = tx.try_send(ev);
    }
}

/// 本阶段的最长保温窗口(秒)。
fn phase_window(phase: WarmingPhase, settings: &CacheWarmingSettings) -> u64 {
    match phase {
        WarmingPhase::Turn => settings.max_turn_window_secs,
        WarmingPhase::Idle => settings.max_idle_window_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use zhuye_llm::{ChatRequest, LlmError, ModelClient};

    /// 测试替身:记录收到的请求,回一条带 usage 的最小流。
    #[derive(Default)]
    struct RecordingStub {
        requests: Mutex<Vec<zhuye_llm::ChatRequest>>,
        stall: bool,
    }

    impl RecordingStub {
        fn recorded(&self) -> Vec<zhuye_llm::ChatRequest> {
            self.requests.lock().clone()
        }
    }

    #[async_trait::async_trait]
    impl ModelClient for RecordingStub {
        fn name(&self) -> &str {
            "stub"
        }
        async fn stream(
            &self,
            request: zhuye_llm::ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<
            Pin<Box<dyn futures::Stream<Item = Result<ChatEvent, LlmError>> + Send>>,
            LlmError,
        > {
            self.requests.lock().push(request);
            if self.stall {
                return Ok(Box::pin(futures::stream::pending()));
            }
            let chunks: Vec<Result<ChatEvent, LlmError>> = vec![
                Ok(ChatEvent::MessageStart {
                    id: "stub-1".into(),
                    model: "m".into(),
                }),
                Ok(ChatEvent::Usage {
                    input_tokens: 10,
                    output_tokens: 1,
                    cached_tokens: 8,
                    cache_write_tokens: 0,
                }),
                Ok(ChatEvent::MessageStop),
            ];
            Ok(Box::pin(futures::stream::iter(chunks)))
        }
    }

    fn warm_settings() -> CacheWarmingSettings {
        CacheWarmingSettings {
            enabled: true,
            force: true,
            // ttl 3s + margin 0 → 刷新延时 2s(真实短延时,仓库无 time::pause 先例)。
            ttl_secs: Some(3),
            refresh_margin_secs: 0,
            ..Default::default()
        }
    }

    fn candidate(client: Arc<RecordingStub>, generation: u64) -> WarmCandidate {
        // 收敛点:具体 Arc 在此统一向上转型为 trait 对象。
        let client: BoxedModelClient = client;
        WarmCandidate {
            request: ChatRequest {
                model: "m".into(),
                messages: vec![zhuye_llm::ChatMessage::User(zhuye_llm::UserContent {
                    blocks: vec![zhuye_llm::ContentBlock::Text {
                        text: "hello".into(),
                    }],
                })],
                cache_control: vec![zhuye_llm::CacheBreak {
                    after_message_index: 0,
                    ttl: zhuye_llm::CacheTtl::FiveMinutes,
                }],
                ..Default::default()
            },
            model_spec: "anthropic/claude-sonnet-4-latest".into(),
            prompt_tokens: 12_000,
            observation: CacheObservation {
                cached_tokens: 11_500,
                cache_write_tokens: 0,
            },
            client,
            credential_label: "c1".into(),
            generation,
            created_at: Instant::now(),
            replayable: true,
            task_budget: None,
            turn_id: TurnId::new(),
        }
    }

    fn scheduler(settings: CacheWarmingSettings) -> (Arc<CacheWarmingScheduler>, Arc<AtomicU64>) {
        let generation = Arc::new(AtomicU64::new(0));
        let sched = Arc::new(CacheWarmingScheduler::new(
            settings,
            CancellationToken::new(),
            Arc::clone(&generation),
        ));
        (sched, generation)
    }

    /// 等待事件通道出现指定 action(超时 panic)。
    async fn wait_action(
        rx: &mut tokio::sync::mpsc::Receiver<Event>,
        want: &str,
        timeout_secs: u64,
    ) -> CacheWarmingEvent {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or_else(|| Duration::from_secs(0));
            let ev = tokio::time::timeout(remaining, rx.recv())
                .await
                .expect("等待保温事件超时")
                .expect("事件通道关闭");
            let EventMsg::CacheWarming(w) = ev.msg else {
                continue;
            };
            let got = match w.action {
                CacheWarmingAction::Scheduled => "scheduled",
                CacheWarmingAction::Executed => "executed",
                CacheWarmingAction::Skipped => "skipped",
                CacheWarmingAction::Cancelled => "cancelled",
            };
            if got == want {
                return w;
            }
        }
    }

    #[tokio::test]
    async fn 排程后到点重放_请求仅改_max_tokens与标记() {
        let (sched, generation) = scheduler(warm_settings());
        let stub = Arc::new(RecordingStub::default());
        sched.record_candidate(candidate(Arc::clone(&stub), 0));
        assert!(sched.has_candidate());
        sched.schedule(WarmingPhase::Idle);
        // 等待 executed(延时 2s,留裕量)。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while sched.stats().executed.load(Ordering::Relaxed) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "保温重放未在预期时间内执行"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let recorded = stub.recorded();
        assert_eq!(recorded.len(), 1, "应恰好一次重放");
        let replay = &recorded[0];
        assert_eq!(replay.max_tokens, Some(1), "重放必须 max_tokens=1");
        assert_eq!(
            replay
                .metadata
                .get(zhuye_llm::METADATA_WARM_REPLAY)
                .map(String::as_str),
            Some(zhuye_llm::METADATA_WARM_REPLAY_VALUE)
        );
        // 除去 max_tokens 与 metadata 标记,重放与候选逐字节一致(缓存键)。
        let mut orig = serde_json::to_value(candidate_for_compare().request).unwrap();
        let mut got = serde_json::to_value(replay.clone()).unwrap();
        if let (Some(o), Some(g)) = (orig.as_object_mut(), got.as_object_mut()) {
            o.remove("max_tokens");
            o.remove("metadata");
            g.remove("max_tokens");
            g.remove("metadata");
            assert_eq!(o, g, "重放前缀必须与候选逐字节一致");
        }
        let _ = generation;
    }

    fn candidate_for_compare() -> WarmCandidate {
        candidate(Arc::new(RecordingStub::default()), 0)
    }

    #[tokio::test]
    async fn 保温请求受任务树预算约束并结算() {
        use zhuye_llm::task_budget::{TaskBudget, TaskBudgetLimits};
        let allowed = TaskBudget::new(TaskBudgetLimits {
            total_tokens: Some(100),
            ..Default::default()
        });
        let denied = TaskBudget::new(TaskBudgetLimits {
            total_tokens: Some(1),
            ..Default::default()
        });
        let (yes, _) = scheduler(warm_settings());
        let (no, _) = scheduler(warm_settings());
        let yes_client = Arc::new(RecordingStub::default());
        let no_client = Arc::new(RecordingStub::default());
        let mut yes_candidate = candidate(yes_client.clone(), 0);
        yes_candidate.task_budget = Some(allowed.clone());
        let mut no_candidate = candidate(no_client.clone(), 0);
        no_candidate.task_budget = Some(denied.clone());
        let (yes_tx, mut yes_rx) = mpsc::channel(16);
        let (no_tx, mut no_rx) = mpsc::channel(16);
        yes.set_event_tx(yes_tx);
        no.set_event_tx(no_tx);
        yes.record_candidate(yes_candidate);
        no.record_candidate(no_candidate);
        yes.schedule(WarmingPhase::Idle);
        no.schedule(WarmingPhase::Idle);
        wait_action(&mut yes_rx, "executed", 8).await;
        wait_action(&mut no_rx, "skipped", 8).await;
        assert_eq!(yes_client.recorded().len(), 1);
        assert!(no_client.recorded().is_empty());
        assert_eq!(allowed.snapshot().actual_tokens, 11);
        assert_eq!(allowed.snapshot().reserved_tokens, 0);
        assert_eq!(denied.snapshot().settled_calls, 0);
    }

    #[tokio::test]
    async fn 在途保温请求在任务截止时结束并释放预留() {
        use zhuye_llm::task_budget::{TaskBudget, TaskBudgetLimits};
        let budget = TaskBudget::new(TaskBudgetLimits {
            total_tokens: Some(100),
            duration: Some(Duration::from_secs(1)),
            ..Default::default()
        });
        let (sched, _) = scheduler(CacheWarmingSettings {
            ttl_secs: Some(10),
            refresh_margin_secs: 8,
            ..warm_settings()
        });
        let client = Arc::new(RecordingStub {
            stall: true,
            ..Default::default()
        });
        let mut cand = candidate(client.clone(), 0);
        cand.created_at -= Duration::from_secs(2);
        cand.task_budget = Some(budget.clone());
        let (tx, mut rx) = mpsc::channel(16);
        sched.set_event_tx(tx);
        sched.record_candidate(cand);
        sched.schedule(WarmingPhase::Idle);

        let cancelled = wait_action(&mut rx, "cancelled", 5).await;
        assert_eq!(cancelled.reason.as_deref(), Some("task_deadline"));
        assert_eq!(client.recorded().len(), 1);
        assert_eq!(budget.snapshot().reserved_tokens, 0);
        assert_eq!(budget.snapshot().unknown_calls, 1);
    }

    #[tokio::test]
    async fn 取消后排程不触发重放() {
        let (sched, _gen) = scheduler(warm_settings());
        let stub = Arc::new(RecordingStub::default());
        sched.record_candidate(candidate(Arc::clone(&stub), 0));
        sched.schedule(WarmingPhase::Idle);
        // 立刻取消:到点(2s)后不应有任何重放。
        sched.cancel_active();
        tokio::time::sleep(Duration::from_millis(2600)).await;
        assert_eq!(stub.recorded().len(), 0, "取消后不得重放");
        assert_eq!(sched.stats().executed.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn 代际失配跳过_不发重放() {
        let (sched, generation) = scheduler(warm_settings());
        let stub = Arc::new(RecordingStub::default());
        sched.record_candidate(candidate(Arc::clone(&stub), 0));
        // 候选写入后前缀变更(rewind / compact / model 切换等)→ 代际 +1。
        generation.fetch_add(1, Ordering::Relaxed);
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        sched.set_event_tx(tx);
        sched.schedule(WarmingPhase::Idle);
        let ev = wait_action(&mut rx, "skipped", 8).await;
        assert_eq!(ev.reason.as_deref(), Some("stale"));
        assert_eq!(stub.recorded().len(), 0, "stale 候选不得重放");
        assert_eq!(sched.stats().skipped.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn 关闭时全部_no_op() {
        let (sched, _gen) = scheduler(CacheWarmingSettings::default());
        let stub = Arc::new(RecordingStub::default());
        sched.record_candidate(candidate(Arc::clone(&stub), 0));
        assert!(!sched.has_candidate(), "关闭时不得存储候选");
        sched.schedule(WarmingPhase::Idle);
        sched.schedule(WarmingPhase::Turn);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(stub.recorded().len(), 0);
        assert_eq!(sched.stats().executed.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn 空闲排程发_scheduled_事件() {
        let (sched, _gen) = scheduler(warm_settings());
        let stub = Arc::new(RecordingStub::default());
        sched.record_candidate(candidate(Arc::clone(&stub), 0));
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        sched.set_event_tx(tx);
        sched.schedule(WarmingPhase::Idle);
        let ev = wait_action(&mut rx, "scheduled", 2).await;
        assert_eq!(ev.phase, CacheWarmingPhase::Idle);
        // Turn 阶段静默:不产生 scheduled 事件。
        sched.schedule(WarmingPhase::Turn);
        tokio::time::sleep(Duration::from_millis(150)).await;
        while let Ok(ev) = rx.try_recv() {
            let EventMsg::CacheWarming(w) = ev.msg else {
                continue;
            };
            assert_ne!(
                w.action,
                CacheWarmingAction::Scheduled,
                "Turn 阶段不得发 scheduled"
            );
        }
    }

    #[tokio::test]
    async fn 不可重放候选排程_no_op() {
        let (sched, _gen) = scheduler(warm_settings());
        let stub = Arc::new(RecordingStub::default());
        let mut cand = candidate(Arc::clone(&stub), 0);
        cand.replayable = false;
        sched.record_candidate(cand);
        sched.schedule(WarmingPhase::Turn);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(stub.recorded().len(), 0, "不可重放候选不得触发重放");
    }

    #[tokio::test]
    async fn executed_事件携带重放观测字段() {
        let (sched, _gen) = scheduler(warm_settings());
        let stub = Arc::new(RecordingStub::default());
        sched.record_candidate(candidate(Arc::clone(&stub), 0));
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        sched.set_event_tx(tx);
        sched.schedule(WarmingPhase::Idle);
        let ev = wait_action(&mut rx, "executed", 8).await;
        assert_eq!(ev.warm_cached_tokens, Some(8), "stub 回执 cached=8");
        assert_eq!(
            ev.model.as_deref(),
            Some("anthropic/claude-sonnet-4-latest")
        );
        assert_eq!(ev.prompt_tokens, Some(12_000));
    }

    #[tokio::test]
    async fn 会话父令牌取消级联到保温任务() {
        let parent = CancellationToken::new();
        let generation = Arc::new(AtomicU64::new(0));
        let sched = Arc::new(CacheWarmingScheduler::new(
            warm_settings(),
            parent.clone(),
            generation,
        ));
        let stub = Arc::new(RecordingStub::default());
        sched.record_candidate(candidate(Arc::clone(&stub), 0));
        sched.schedule(WarmingPhase::Idle);
        // 会话级取消(Shutdown 路径)→ 保温任务随之取消。
        parent.cancel();
        tokio::time::sleep(Duration::from_millis(2600)).await;
        assert_eq!(stub.recorded().len(), 0, "会话取消后不得重放");
    }
}
