//! [`ApprovalGate`] 结构体定义、`Debug` 实现与全部方法实现。
//!
//! 类型别名、free function(`complete_*`)与常量留在 [`super`] (`mod.rs`),
//! 此文件只承载 gate 自身的逻辑,通过 `use super::*` 拿到共享状态类型。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use zhuye_protocol::{
    ApprovalKind, ApprovalRequestEvent, AskUserAnswer, AskUserInputEvent, Event, EventMsg,
    InteractionKind, InteractionOutcome, InteractionResolvedEvent, PendingInteraction,
    PermissionBubbleEvent, PermissionMode, Question, ReviewDecision, RiskLevel, RolloutRecord,
    RolloutRecorder, ToolError, question::AskUserQuestionEvent,
};

use super::{
    ApprovalWaiters, AskUserInputWaiters, AskUserQuestionWaiters, MAX_ASK_USER_PROMPT_BYTES,
    MAX_PENDING_APPROVALS, PendingLease, PendingRegistry, complete_approval,
    complete_ask_user_question,
};

/// 单 turn 的审批门。由 `submission_loop` 持有,交给
/// `ToolExecutionQueue::execute_all_with_gate` 使用。
pub struct ApprovalGate {
    /// v1.5 R1:`ThreadSettingsOverrides.approval_policy = deny` 时置位 ——
    /// 本回合所有需要审批的工具一律直接拒绝(不弹 modal、不问 resolver)。
    /// gate 为回合级实例,天然限定作用域。
    pub(super) deny_all: std::sync::atomic::AtomicBool,
    turn_policy: Option<zhuye_protocol::ApprovalPolicy>,
    pub(super) event_tx: mpsc::Sender<Event>,
    pub(super) sub_id: String,
    pub(super) waiters: ApprovalWaiters,
    /// 用户在整个会话生命周期内已批准的工具。
    pub(super) session_allow: Arc<Mutex<HashSet<String>>>,
    /// F06:用户「拒绝且不再问」的工具 → 拒绝原因。会话级,与
    /// `session_allow` 对称;命中即直接拒绝,不再弹审批。
    pub(super) session_deny: Arc<Mutex<HashMap<String, String>>>,
    /// S5a:可选的 permission resolver,`ask_tool` 入口先查规则短路
    /// Allow / Deny。`None` 时退回原 modal-only 行为(向后兼容)。
    pub(super) permission_resolver: Option<Arc<dyn zhuye_permissions::PermissionResolver>>,
    /// v1.1.0 P1 #14:per-gate waiter map for `ask_user_question` 工具的
    /// 结构化问答。每个 `ApprovalGate` 自带一份,`submission_loop` 通过
    /// `Arc<ApprovalGate>` 调 `complete_question` 路由 `Op::AskUserQuestionResponse`
    /// 回执。
    pub(super) question_waiters: AskUserQuestionWaiters,
    /// v1.1.0 P1 #15:`ask_user` 自由文本询问 waiter map。
    pub(super) user_input_waiters: AskUserInputWaiters,
    /// 会话级 `PermissionMode`(AcceptEdits / Bubble 短路用)。
    pub(super) session_permission_mode: Option<Arc<parking_lot::RwLock<PermissionMode>>>,
    /// P2 `yolo-classifier`:Auto 模式下的启发式审批分类器。
    /// `None`(默认)= Auto 模式下落到 modal(向后兼容);`Some` 时 Auto 模式
    /// 且 resolver 无规则、mode 未自动批准时,查分类器:Allow(高置信)→ 自动批准,
    /// Deny → 拒绝,Ask / 低置信 → modal。
    pub(super) yolo_classifier: Mutex<Option<Arc<dyn zhuye_permissions::YoloClassifier>>>,
    /// YOLO 自动批准的置信度阈值(>= 此值才信任 Allow 建议)。默认 0.8。
    pub(super) yolo_threshold: Mutex<f32>,
    /// 本回合的**回合级**取消令牌(`Op::Interrupt` 取消它)。
    ///
    /// ⚠️ 为什么需要它:gate 的等待(`ask_tool` / `ask_hook` /
    /// `ask_question` / `ask_user`)此前只观察**会话级**令牌
    /// (调用方 `ToolContext.cancel` 传的是它)。而 `Op::Interrupt` 取消的
    /// 是**回合级**令牌 —— 于是「agent 正在等审批时按中断」**不会**解除
    /// 等待,回合**永远不终止**(实测:回合停在 approval_request 上,
    /// 中断后既没有 turn_aborted 也没有 turn_complete)。
    ///
    /// gate 是**每回合**构造的(submission_loop 里),所以把回合令牌挂在
    /// gate 上是安全的;会话令牌仍由调用方的 `cancel` 参数负责
    /// (`Op::Shutdown` 的语义)。
    pub(super) turn_cancel: Option<CancellationToken>,
    /// v1.9 S6:审批 TTL。`Some(d)` 时 gate 构造起算 `created_at + d`
    /// 为**共享绝对死线**,本 turn 全部挂起审批到点统一过期(Deny);
    /// `None` = 不超时(headless 全自动流水线显式关闭)。
    pub(super) approval_ttl: Option<std::time::Duration>,
    /// v1.x:`ask_question` 的绝对死线(秒)。此前结构化问答无任何 TTL,
    /// headless 强开审批且无人应答时永久挂起。
    pub(super) question_timeout_secs: u64,
    /// v1.9 S6:gate 构造时刻(死线锚点)。
    pub(super) created_at: std::time::Instant,
    /// v1.12:挂起人机交互注册表(会话级共享;与 waiter 表并排的快照行)。
    /// 注册 / 了结与 waiter 严格同宽;`pending_snapshot()` 与
    /// `AgentThread::pending_interactions()` 据此回答「当前挂起哪些交互」。
    pub(super) pending: PendingRegistry,
    /// v1.12:可选 rollout recorder —— 交互注册 / 了结时落
    /// `InteractionRequest` / `InteractionResolution` 审计记录。
    /// `None`(测试 / TUI 简单装配)时只维护内存注册表,不落盘。
    pub(super) recorder: Option<Arc<dyn RolloutRecorder>>,
}

// 手写 Debug —— `Arc<dyn PermissionResolver>` 没 derive Debug。
impl std::fmt::Debug for ApprovalGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalGate")
            .field("sub_id", &self.sub_id)
            .field("waiters", &self.waiters)
            .field("session_allow", &self.session_allow)
            .field(
                "permission_resolver",
                &self
                    .permission_resolver
                    .as_ref()
                    .map(|_| "<dyn PermissionResolver>"),
            )
            .field("question_waiters", &self.question_waiters)
            .field("user_input_waiters", &self.user_input_waiters)
            .field(
                "session_permission_mode",
                &self
                    .session_permission_mode
                    .as_ref()
                    .map(|_| "<RwLock<PermissionMode>>"),
            )
            .field("yolo_classifier", &self.yolo_classifier.lock().is_some())
            .field("yolo_threshold", &*self.yolo_threshold.lock())
            .field("approval_ttl", &self.approval_ttl)
            .field("pending_count", &self.pending.lock().len())
            .field(
                "recorder",
                &self.recorder.as_ref().map(|_| "<dyn RolloutRecorder>"),
            )
            .finish()
    }
}

/// v1.9 S7:abort-safe 审批租约。
///
/// `ask_tool` / `ask_hook` 注册 waiter 后立即持有一个 lease;等待
/// future 无论从哪条路径退出(cancel / interrupt / TTL / decision /
/// drop-abort),lease 的 Drop 都保证 waiter 条目从 map 清除:
/// - 正常路径:decision 经 `complete_approval` 先移除条目 → Drop no-op;
/// - 异常路径(future 被 abort / panic):没有别的清理者 → Drop 兜底,
///   不留占挂起名额的死条目。
///
/// 有了它,"注册后、select 前 future 被 drop" 的窗口也不再泄漏
/// (旧实现在该窗口里条目永久悬挂)。
struct WaiterLease {
    map: ApprovalWaiters,
    request_id: String,
}

impl WaiterLease {
    fn register(
        map: &ApprovalWaiters,
        request_id: String,
        tx: oneshot::Sender<ReviewDecision>,
    ) -> Option<WaiterLease> {
        let mut waiters = map.lock();
        if waiters.len() >= MAX_PENDING_APPROVALS {
            return None;
        }
        waiters.insert(request_id.clone(), tx);
        Some(WaiterLease {
            map: std::sync::Arc::clone(map),
            request_id,
        })
    }
}

impl Drop for WaiterLease {
    fn drop(&mut self) {
        self.map.lock().remove(&self.request_id);
    }
}

impl ApprovalGate {
    /// 挂上本回合的**回合级**取消令牌 —— `Op::Interrupt` 取消它。
    ///
    /// 挂上之后,「agent 正在等审批时按中断」能真正解除等待(此前只观察
    /// 会话级令牌,回合会永远停在审批门上)。
    pub fn with_turn_cancel(mut self, turn_cancel: CancellationToken) -> Self {
        self.turn_cancel = Some(turn_cancel);
        self
    }

    /// 等回合级取消(未挂载时永远 pending)。
    async fn wait_turn_cancel(&self) {
        match &self.turn_cancel {
            Some(t) => t.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    }

    pub fn new(event_tx: mpsc::Sender<Event>, sub_id: impl Into<String>) -> Self {
        Self::with_state(
            event_tx,
            sub_id,
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashSet::new())),
            None,
            None,
            None,
            None,
        )
    }

    /// 构造一个共享"session 内已批准"集合的 gate,使兄弟 turn 共用(从而
    /// turn N 的 `ApproveForSession` 决策可以延续到 turn N+1)。Waiters
    /// 是该 gate 私有的。
    pub fn with_session_allow(
        event_tx: mpsc::Sender<Event>,
        sub_id: impl Into<String>,
        session_allow: Arc<Mutex<HashSet<String>>>,
    ) -> Self {
        Self::with_state(
            event_tx,
            sub_id,
            Arc::new(Mutex::new(HashMap::new())),
            session_allow,
            None,
            None,
            None,
            None,
        )
    }

    /// 构造一个共享 `waiters` 与 `session_allow` 的 gate。这让 `submission_loop`
    /// 能把传入的 `Op::ToolApproval` 路由到任一 gate 写入的同一个 waiter
    /// map(request_id 是 uuid,跨 turn 不会撞 id)。
    ///
    /// S5a:第 5 参数 `permission_resolver` 可选。`Some(r)` 时 `ask_tool`
    /// 入口先查 `r.resolve(tool)` 短路 Allow / Deny;`None` 时回退到
    /// 原 modal 行为。**不**修改 `ask_tool` 以外的现有路径语义。
    ///
    /// v1.1.0 P1 #14:第 6 参数 `question_waiters` 可选。`Some(qw)` 时 gate
    /// 把 `ask_user_question` 工具的 oneshot waiter 注册到共享 map(由
    /// `submission_loop` 持全局,`Op::AskUserQuestionResponse` 路由用);
    /// `None` 时 per-gate 新建一个独立 map(向后兼容 + 简单场景)。
    ///
    /// v1.1.0 P1 #15:第 7 参数 `user_input_waiters` —— `ask_user` 工具用。
    /// 第 8 参数 `session_permission_mode` —— AcceptEdits / Bubble 短路。
    #[allow(clippy::too_many_arguments)]
    pub fn with_state(
        event_tx: mpsc::Sender<Event>,
        sub_id: impl Into<String>,
        waiters: ApprovalWaiters,
        session_allow: Arc<Mutex<HashSet<String>>>,
        permission_resolver: Option<Arc<dyn zhuye_permissions::PermissionResolver>>,
        question_waiters: Option<AskUserQuestionWaiters>,
        user_input_waiters: Option<AskUserInputWaiters>,
        session_permission_mode: Option<Arc<parking_lot::RwLock<PermissionMode>>>,
    ) -> Self {
        Self {
            deny_all: std::sync::atomic::AtomicBool::new(false),
            turn_policy: None,
            turn_cancel: None,
            event_tx,
            sub_id: sub_id.into(),
            waiters,
            session_allow,
            // F06:会话级拒绝表。`with_state` 是唯一构造点;跨 turn 共享语义
            // 由调用方改用 `with_session_deny` 注入同一 Arc(见下)。
            session_deny: Arc::new(Mutex::new(HashMap::new())),
            permission_resolver,
            question_waiters: question_waiters
                .unwrap_or_else(|| Arc::new(Mutex::new(HashMap::new()))),
            user_input_waiters: user_input_waiters
                .unwrap_or_else(|| Arc::new(Mutex::new(HashMap::new()))),
            session_permission_mode,
            yolo_classifier: Mutex::new(None),
            yolo_threshold: Mutex::new(0.8),
            approval_ttl: Some(std::time::Duration::from_secs(
                super::DEFAULT_APPROVAL_TTL_SECS,
            )),
            question_timeout_secs: super::ASK_QUESTION_TIMEOUT_SECS,
            created_at: std::time::Instant::now(),
            pending: super::pending::new_pending_registry(),
            recorder: None,
        }
    }

    /// v1.12:注入会话级共享的挂起交互注册表。`submission_loop` 装配时
    /// 传 `cfg.approval_context.pending.clone()`,父子代理与 plan gate
    /// 共用同一张表;不注入时 gate 自持一张独立表(测试 / 简单装配)。
    pub fn with_pending_registry(mut self, pending: PendingRegistry) -> Self {
        self.pending = pending;
        self
    }

    /// v1.12:注入 rollout recorder,交互注册 / 了结时落审计记录。
    pub fn with_recorder(mut self, recorder: Option<Arc<dyn RolloutRecorder>>) -> Self {
        self.recorder = recorder;
        self
    }

    /// v1.12:当前挂起交互快照(按注册时刻升序)。
    ///
    /// 客户端 reload / 换绑后据此重新水合审批 UI;活进程内这是权威面,
    /// 客户端内存投影只是它的缓存。
    pub fn pending_snapshot(&self) -> Vec<PendingInteraction> {
        super::pending::pending_snapshot(&self.pending)
    }

    /// v1.12:了结注册表条目并广播 `InteractionResolved` + 落盘 resolution。
    ///
    /// 返回 `false` 表示条目已被他人了结(如 TTL 清扫先行移除)—— 此时
    /// 广播与落盘由清扫方负责,调用方应跳过,避免双份通知。
    async fn resolve_pending(&self, request_id: &str, outcome: InteractionOutcome) -> bool {
        let Some(entry) = super::pending::pending_resolve(&self.pending, request_id) else {
            return false;
        };
        let ev = Event::new(
            self.sub_id.clone(),
            EventMsg::InteractionResolved(InteractionResolvedEvent {
                request_id: request_id.to_string(),
                kind: entry.kind.clone(),
                outcome,
            }),
        );
        let _ = self.event_tx.send(ev).await;
        if let Some(rec) = &self.recorder {
            let rec = Arc::clone(rec);
            let request_id = request_id.to_string();
            tokio::spawn(async move {
                let _ = rec
                    .record(RolloutRecord::InteractionResolution {
                        request_id,
                        outcome,
                        at: chrono::Utc::now(),
                    })
                    .await;
            });
        }
        true
    }

    /// v1.12:登记一条挂起交互并落盘 `InteractionRequest` 审计。
    async fn register_pending(&self, interaction: PendingInteraction) -> PendingLease {
        if let Some(rec) = &self.recorder {
            let rec = Arc::clone(rec);
            let snapshot = interaction.clone();
            tokio::spawn(async move {
                let _ = rec
                    .record(RolloutRecord::InteractionRequest {
                        request_id: snapshot.request_id.clone(),
                        kind: snapshot.kind.clone(),
                        summary: snapshot.summary.clone(),
                        created_at_ms: snapshot.created_at_ms,
                        expires_at_ms: snapshot.expires_at_ms,
                        payload: snapshot.payload.clone(),
                    })
                    .await;
            });
        }
        PendingLease::register(&self.pending, interaction)
    }

    /// v1.12:当前 unix 毫秒。
    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default()
    }

    /// v1.12:摘要截断(注册表 / 审计记录里的 summary 有界,防止超大
    /// 工具参数 / 询问正文撑爆内存与 JSONL)。
    fn bounded_summary(text: impl Into<String>) -> String {
        const MAX_SUMMARY_CHARS: usize = 200;
        let text: String = text.into();
        let mut out = text.chars().take(MAX_SUMMARY_CHARS).collect::<String>();
        if out.chars().count() < text.chars().count() {
            out.push('…');
        }
        out
    }

    /// v1.x:覆盖 `ask_question` 的绝对死线(秒)。测试注入用;生产走
    /// [`ASK_QUESTION_TIMEOUT_SECS`] 默认值。
    pub fn with_question_timeout_secs(mut self, secs: u64) -> Self {
        self.question_timeout_secs = secs;
        self
    }

    /// 设置当前回合独立的审批策略，不改变规划模式。
    pub fn with_turn_policy(mut self, policy: Option<zhuye_protocol::ApprovalPolicy>) -> Self {
        self.turn_policy = policy;
        self
    }

    /// v1.9 S6:设置审批 TTL(覆盖默认 300s)。gate 构造起算,本 turn
    /// 全部挂起审批共享同一绝对死线。
    pub fn with_approval_ttl(mut self, ttl: std::time::Duration) -> Self {
        self.approval_ttl = Some(ttl);
        self
    }

    /// v1.9 S6:显式关闭审批超时(headless 全自动流水线等场景)。
    pub fn without_approval_ttl(mut self) -> Self {
        self.approval_ttl = None;
        self
    }

    /// v1.9 S6:共享绝对死线(`None` = 未启用 TTL)。
    fn absolute_deadline(&self) -> Option<std::time::Instant> {
        self.approval_ttl.map(|ttl| self.created_at + ttl)
    }

    /// v1.9 S6:清扫全部挂起审批 —— 统一以 Deny(过期)裁决。返回
    /// 过期条数。给 `submission_loop` 的周期任务用;等待侧的 select
    /// 死线是兜底,两者语义一致(过期即 Deny,不可能再执行)。
    ///
    /// v1.12:同步维护挂起注册表 —— 先移除条目再投递 Deny,让等待侧的
    /// rx 分支能凭「条目已不在」识别出清扫方已负责通知/落盘,避免双份
    /// `InteractionResolved` / 双份审计记录。
    pub fn expire_pending_approvals(&self) -> usize {
        let reason = super::expired_reason();
        let mut map = self.waiters.lock();
        let n = map.len();
        // 先摘注册表条目(v1.12),再投递 Deny:rx 分支以条目缺席判清扫。
        let swept: Vec<PendingInteraction> = map
            .drain()
            .filter_map(|(request_id, tx)| {
                let entry = super::pending::pending_resolve(&self.pending, &request_id);
                let _ = tx.send(ReviewDecision::Deny {
                    reason: reason.clone(),
                });
                entry
            })
            .collect();
        drop(map);
        if !swept.is_empty() {
            // 通知 + 落盘在无运行时上下文时跳过(同步调用点可能不在
            // tokio runtime 内,如部分测试);注册表清理本身已生效。
            let swept_len = swept.len();
            if tokio::runtime::Handle::try_current().is_ok() {
                for entry in &swept {
                    let event_tx = self.event_tx.clone();
                    let sub_id = self.sub_id.clone();
                    let recorder = self.recorder.clone();
                    let entry = entry.clone();
                    tokio::spawn(async move {
                        let ev = Event::new(
                            sub_id,
                            EventMsg::InteractionResolved(InteractionResolvedEvent {
                                request_id: entry.request_id.clone(),
                                kind: entry.kind.clone(),
                                outcome: InteractionOutcome::Expired,
                            }),
                        );
                        let _ = event_tx.send(ev).await;
                        if let Some(rec) = recorder {
                            let _ = rec
                                .record(RolloutRecord::InteractionResolution {
                                    request_id: entry.request_id,
                                    outcome: InteractionOutcome::Expired,
                                    at: chrono::Utc::now(),
                                })
                                .await;
                        }
                    });
                }
            }
            tracing::warn!(
                expired = swept_len,
                "approval gate: TTL sweep expired pending approvals"
            );
        }
        n
    }

    async fn send_approval_event(
        &self,
        event: Event,
        cancel: &CancellationToken,
    ) -> Result<(), ReviewDecision> {
        let ttl: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
            match self.ttl_sleep() {
                Some(d) => Box::pin(tokio::time::sleep(d)),
                None => Box::pin(std::future::pending()),
            };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ReviewDecision::Deny { reason: "cancelled".into() }),
            _ = self.wait_turn_cancel() => Err(ReviewDecision::Deny { reason: "turn interrupted".into() }),
            _ = ttl => Err(ReviewDecision::Deny { reason: super::expired_reason() }),
            result = self.event_tx.send(event) => result.map_err(|_| ReviewDecision::Deny { reason: "approval channel closed".into() }),
        }
    }

    /// v1.9 S6:剩余到死线的等待时长(`None` = 未启用 TTL)。
    fn ttl_sleep(&self) -> Option<std::time::Duration> {
        self.absolute_deadline()
            .map(|d| d.saturating_duration_since(std::time::Instant::now()))
    }

    /// v1.5 R1:置位/解除回合级 deny-all。置位后,本回合所有走 gate 的
    /// 审批(`ask_tool` / `ask_hook`)一律直接 `Deny`,不弹 modal。
    /// 由 `submission_loop` 在 `approval_policy = deny` 时调用 —— gate
    /// 为回合级实例,作用域天然限定单回合。
    pub fn set_deny_all(&self, on: bool) {
        self.deny_all.store(on, std::sync::atomic::Ordering::SeqCst);
    }

    /// F04:当前是否处于 deny-all(`approval_policy = deny` 会话级硬拒绝)。
    ///
    /// 暴露给 queue:只读命令的自动放行(`action_permission` → `Auto`)
    /// **必须**让位于 deny-all —— 否则纯读命令会绕过会话级拒绝策略。
    pub fn is_deny_all(&self) -> bool {
        self.deny_all.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// P2 `yolo-classifier`:注入 Auto 模式启发式审批分类器 + 可选置信度阈值。
    /// `threshold` < 0 时维持当前值。
    pub fn set_yolo_classifier(
        &self,
        classifier: Option<Arc<dyn zhuye_permissions::YoloClassifier>>,
        threshold: Option<f32>,
    ) {
        *self.yolo_classifier.lock() = classifier;
        if let Some(t) = threshold {
            if t >= 0.0 {
                *self.yolo_threshold.lock() = t;
            }
        }
    }

    pub fn sub_id(&self) -> &str {
        &self.sub_id
    }

    pub fn waiters(&self) -> &ApprovalWaiters {
        &self.waiters
    }

    /// v1.1.0 P1 #14:per-gate waiter map for `ask_user_question` 工具。
    pub fn question_waiters(&self) -> &AskUserQuestionWaiters {
        &self.question_waiters
    }

    /// v1.1.0 P1 #15:per-gate waiter map for `ask_user` 工具。
    pub fn user_input_waiters(&self) -> &AskUserInputWaiters {
        &self.user_input_waiters
    }

    pub fn session_allow_handle(&self) -> Arc<Mutex<HashSet<String>>> {
        self.session_allow.clone()
    }

    pub fn is_session_allowed(&self, tool_name: &str) -> bool {
        self.session_allow.lock().contains(tool_name)
    }

    pub fn allow_for_session(&self, tool_name: impl Into<String>) {
        self.session_allow.lock().insert(tool_name.into());
    }

    /// F06:该工具是否已被「拒绝且不再问」记住。
    pub fn is_session_denied(&self, tool_name: &str) -> bool {
        self.session_deny.lock().contains_key(tool_name)
    }

    /// F06:记住「拒绝且不再问」,返回该工具未来调用应复用的拒绝原因。
    ///
    /// 与 [`allow_for_session`](Self::allow_for_session) 对称:一个永久
    /// 放行、一个永久拒绝,都是 fail-closed 语义的一部分。
    pub fn deny_for_session(&self, tool_name: impl Into<String>, reason: impl Into<String>) {
        self.session_deny
            .lock()
            .insert(tool_name.into(), reason.into());
    }

    /// F06:已记忆的会话级拒绝原因(供 queue 直接合成错误结果)。
    pub fn session_deny_reason(&self, tool_name: &str) -> Option<String> {
        self.session_deny.lock().get(tool_name).cloned()
    }

    /// F06:会话级拒绝表句柄(与 `session_allow_handle` 同款,供
    /// `submission_loop` / UI 查询与清空)。
    pub fn session_deny_handle(&self) -> Arc<Mutex<HashMap<String, String>>> {
        self.session_deny.clone()
    }

    /// 完成一个待处理的审批请求。`submission_loop` 在收到 `Op::ToolApproval`
    /// 或 `Op::HookApproval` 时调用。返回 `true` 表示找到匹配的 waiter
    /// 并完成决策。
    pub fn complete(&self, request_id: &str, decision: ReviewDecision) -> bool {
        complete_approval(&self.waiters, request_id, decision)
    }

    /// v1.1.0 P1 #14:per-gate 包装的 `complete_ask_user_question`(per-gate
    /// `question_waiters` 模式)。`submission_loop` 默认走全局
    /// `complete_ask_user_question` 函数(共享 map),tool 端调试时可用
    /// 这个 method。返回 `true` 表示找到 waiter 并成功发送。
    pub fn complete_question(&self, request_id: &str, answers: AskUserAnswer) -> bool {
        complete_ask_user_question(&self.question_waiters, request_id, answers)
    }

    /// 向用户请求审批一次工具调用。emit `ApprovalRequest` 后等待 oneshot,
    /// 直到对应的 `Op::ToolApproval` 到达、cancel token 触发或 event channel
    /// 关闭。
    ///
    /// S5a:入口先查 `permission_resolver`(若挂载):
    /// - `RuleMatch::Allow` → 直接返回 `Approve`(**不**写 session_allow,
    ///   一次 tool call 的 allow 不该污染"session 持久"语义;
    ///   真正想"session 内总是允许"用 `/permissions allow <tool>` 持久规则)。
    /// - `RuleMatch::Deny` → 直接返回 `Deny { reason }`。
    /// - `RuleMatch::Ask` 或 `NoMatch` → 走正常 modal 流程(后者退化)。
    ///
    /// 短路优先于 session_allow 缓存 + 用户 modal 三段决策链 —— 显式规则
    /// 比"上次点了 Always Allow"更权威。
    pub async fn ask_tool(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        risk: RiskLevel,
        cancel: &CancellationToken,
    ) -> ReviewDecision {
        self.ask_tool_with_policy(tool_name, args, risk, cancel, None)
            .await
    }

    /// 队列已判定策略要求询问时,不能再被会话模式或分类器自动放行。
    pub(crate) async fn ask_tool_with_policy(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        risk: RiskLevel,
        cancel: &CancellationToken,
        policy_decision: Option<zhuye_permissions::RuleMatch>,
    ) -> ReviewDecision {
        // 0. v1.5 R1:回合级 deny-all(`approval_policy = deny`)最优先。
        if self.deny_all.load(std::sync::atomic::Ordering::SeqCst) {
            return ReviewDecision::Deny {
                reason: "approval_policy = deny(本回合拒绝所有需审批的工具)".into(),
            };
        }
        // 1. S5a:resolver 短路。Ask / NoMatch / None → fall through。
        // 带 bash 命令上下文调 `resolve_with_context`,让 `shell_pattern`
        // 规则(如 `Bash: git *`)能命中 —— 否则该类规则静默失效。
        //
        // 0.3:bash 命令必须经 `resolve_bash_command` 取(兼容 `cmd` /
        // `command` / `cmdline` 三个别名),不能只读 `args["cmd"]` ——
        // 否则模型发 `{"command": "rm -rf /"}` 时 bash_cmd 为 None,
        // 所有 shell_pattern 规则(含 Deny)被整体跳过。
        let decision = if let Some(decision) = policy_decision {
            decision
        } else if let Some(resolver) = &self.permission_resolver {
            let bash_cmd = crate::bash_command::resolve_bash_command(args);
            let resource = crate::queue::permission_resource(args);
            resolver
                .resolve_with_resource(tool_name, bash_cmd, resource)
                .await
        } else {
            zhuye_permissions::RuleMatch::NoMatch
        };
        let force_modal = match decision {
            zhuye_permissions::RuleMatch::Allow => return ReviewDecision::Approve,
            zhuye_permissions::RuleMatch::Deny => {
                return ReviewDecision::Deny {
                    reason: format!("denied by rule for {tool_name}"),
                };
            }
            zhuye_permissions::RuleMatch::Ask => true,
            zhuye_permissions::RuleMatch::NoMatch => false,
        };

        if self.turn_policy == Some(zhuye_protocol::ApprovalPolicy::FullAccess) {
            return ReviewDecision::Approve;
        }
        if !force_modal {
            match self.turn_policy {
                Some(zhuye_protocol::ApprovalPolicy::AutoApprove) if risk != RiskLevel::High => {
                    return ReviewDecision::Approve;
                }
                _ => {}
            }
        }

        // 2. 会话 `PermissionMode` 短路(AcceptEdits / Bubble / Deny)。
        // 显式 `RuleMatch::Ask` 规则优先,不覆盖。
        //
        // v1.3 safety baseline(plan §五):即使会话模式通常会放行工具
        // (`Bubble` 一律放行;文件编辑类工具在 `AcceptEdits` 下放行),
        // `risk == High` 仍然 override,强制弹出人工审批 modal。
        // 这堵死了"Bubble 一刀切全部放行"和
        // "ApproveForSession 在 bash 上覆盖 Dangerous"两个后门:
        // 任何高危副作用(`sudo` / `rm -rf` / shell-installer 等)
        // 每次都必须显式人工确认。
        if !force_modal
            && self
                .turn_policy
                .is_none_or(|p| p == zhuye_protocol::ApprovalPolicy::Auto)
        {
            if let Some(mode_ref) = &self.session_permission_mode {
                let mode = *mode_ref.read();
                if mode == PermissionMode::Deny {
                    return ReviewDecision::Deny {
                        reason: "session permission mode is deny".into(),
                    };
                }
                if risk != RiskLevel::High && self.is_session_allowed(tool_name) {
                    return ReviewDecision::Approve;
                }
                if risk != RiskLevel::High && mode.auto_approves_tool(tool_name) {
                    if mode == PermissionMode::Bubble {
                        self.emit_permission_bubble(tool_name, args, risk).await;
                    }
                    return ReviewDecision::Approve;
                }
            } else if risk != RiskLevel::High && self.is_session_allowed(tool_name) {
                return ReviewDecision::Approve;
            }
        }

        // 3. P2 `yolo-classifier`:Auto 模式下(resolver 无规则 + mode 未自动批准),
        // 查启发式分类器。Allow 且置信度 >= 阈值 → 自动批准(免 modal);
        // Deny → 拒绝;Ask / 低置信 → 继续 modal。无分类器时维持原 modal 行为。
        if !force_modal
            && self
                .turn_policy
                .is_none_or(|p| p == zhuye_protocol::ApprovalPolicy::Auto)
        {
            if let Some(classifier) = self.yolo_classifier.lock().clone() {
                let args_str = serde_json::to_string(args).unwrap_or_default();
                let class = classifier.classify(tool_name, &args_str);
                let threshold = *self.yolo_threshold.lock();
                match class.suggestion {
                    zhuye_permissions::YoloSuggestion::Allow if class.confidence >= threshold => {
                        return ReviewDecision::Approve;
                    }
                    zhuye_permissions::YoloSuggestion::Deny => {
                        return ReviewDecision::Deny {
                            reason: format!("denied by yolo classifier: {}", class.reason),
                        };
                    }
                    // Allow 低置信 / Ask → 落到 modal。
                    _ => {}
                }
            }
        }

        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel::<ReviewDecision>();
        // 检查上限与注册必须在同一把锁内完成。
        let Some(_lease) = WaiterLease::register(&self.waiters, request_id.clone(), tx) else {
            return ReviewDecision::Deny {
                reason: format!(
                    "approval gate: pending approvals at cap ({MAX_PENDING_APPROVALS}); fail-closed deny"
                ),
            };
        };

        // v1.12:登记挂起交互快照(waiter 注册成功后立即入表;任何退出
        // 路径经下方显式 resolve 或 lease Drop 兜底清理)。
        let created_at_ms = Self::now_ms();
        let expires_at_ms = self
            .approval_ttl
            .map(|ttl| created_at_ms.saturating_add(ttl.as_millis() as u64));
        // 摘要优先取 bash 命令原文(与 resolver 同源),否则退化为紧凑 JSON。
        let args_preview = crate::bash_command::resolve_bash_command(args).map_or_else(
            || serde_json::to_string(args).unwrap_or_default(),
            str::to_string,
        );
        let _pending_lease = self
            .register_pending(PendingInteraction {
                request_id: request_id.clone(),
                kind: InteractionKind::Tool {
                    tool_name: tool_name.to_string(),
                },
                summary: Self::bounded_summary(format!("{tool_name}: {args_preview}")),
                created_at_ms,
                expires_at_ms,
                // v1.12:原始 kind 载荷 —— reload 后客户端据此完整重建
                // 审批弹窗(含「始终允许这条命令」所需的原始 args)。
                payload: serde_json::json!({
                    "tool_name": tool_name,
                    "args": args,
                }),
            })
            .await;

        let ev = Event::new(
            self.sub_id.clone(),
            EventMsg::ApprovalRequest(ApprovalRequestEvent {
                request_id: request_id.clone(),
                kind: ApprovalKind::Tool {
                    tool_name: tool_name.to_string(),
                    args: args.clone(),
                },
                risk,
            }),
        );

        // 发送事件也受取消和共享死线约束，避免满队列使审批永久挂起。
        if let Err(decision) = self.send_approval_event(ev, cancel).await {
            // 事件没发出去 = 没有任何客户端见过该请求,不广播 InteractionResolved;
            // 注册表条目由 pending_lease / _lease 的 Drop 兜底清理。
            return decision;
        }

        // v1.9 S6:共享绝对死线(TTL 未启用时用 pending 永挂,交给
        // cancel / interrupt / decision 分支)。Box<dyn Future> 是 Unpin,
        // select! 里以 `&mut` 等待。
        let mut ttl_fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
            match self.ttl_sleep() {
                Some(d) => Box::pin(tokio::time::sleep(d)),
                None => Box::pin(std::future::pending()),
            };

        // v1.12:等待结果同时给出裁决与了结路径,便于统一广播 / 落盘。
        let (decision, outcome) = tokio::select! {
            biased;
            _ = cancel.cancelled() => (
                ReviewDecision::Deny { reason: "cancelled".into() },
                InteractionOutcome::Cancelled,
            ),
            // 回合级中断(Op::Interrupt)—— 解除等待,不让回合停在审批门上。
            _ = self.wait_turn_cancel() => (
                ReviewDecision::Deny { reason: "turn interrupted".into() },
                InteractionOutcome::Interrupted,
            ),
            // v1.9 S6:审批超时 —— 过期即 Deny,过期后的审批不可能再执行。
            _ = &mut ttl_fut => (
                ReviewDecision::Deny { reason: super::expired_reason() },
                InteractionOutcome::Expired,
            ),
            res = rx => {
                match res {
                    Ok(decision) => (decision, InteractionOutcome::Answered),
                    Err(_) => (
                        ReviewDecision::Deny { reason: "approval channel dropped".into() },
                        InteractionOutcome::ChannelClosed,
                    ),
                }
            }
        };
        // v1.12:了结注册表条目 + 广播 InteractionResolved + 落盘 resolution。
        // 条目已被 TTL 清扫先行摘除时这里自然跳过(清扫方已负责通知)。
        self.resolve_pending(&request_id, outcome).await;
        decision
        // `_lease` / `pending_lease` 在此 drop:waiter 表与注册表统一兜底清理。
    }

    /// 向用户请求审批一次 hook 决策(`ask_tool` 的 `ApprovalKind::Hook` 版本)。
    /// M6 起 hook 引入 `HookDecision::Ask` 时会接通此路径。
    pub async fn ask_hook(
        &self,
        hook_name: &str,
        decision_preview: impl Into<String>,
        risk: RiskLevel,
        cancel: &CancellationToken,
    ) -> ReviewDecision {
        // v1.5 R1:deny-all 同样覆盖 hook 级审批。
        if self.deny_all.load(std::sync::atomic::Ordering::SeqCst) {
            return ReviewDecision::Deny {
                reason: "approval_policy = deny(本回合拒绝所有需审批的工具)".into(),
            };
        }
        if self.turn_policy == Some(zhuye_protocol::ApprovalPolicy::FullAccess) {
            return ReviewDecision::Approve;
        }
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel::<ReviewDecision>();
        let Some(_lease) = WaiterLease::register(&self.waiters, request_id.clone(), tx) else {
            return ReviewDecision::Deny {
                reason: format!(
                    "approval gate: pending approvals at cap ({MAX_PENDING_APPROVALS}); fail-closed deny"
                ),
            };
        };

        // v1.12:登记挂起交互快照(同 ask_tool)。
        let created_at_ms = Self::now_ms();
        let expires_at_ms = self
            .approval_ttl
            .map(|ttl| created_at_ms.saturating_add(ttl.as_millis() as u64));
        let decision_preview = decision_preview.into();
        let _pending_lease = self
            .register_pending(PendingInteraction {
                request_id: request_id.clone(),
                kind: InteractionKind::Hook {
                    hook_name: hook_name.to_string(),
                },
                summary: Self::bounded_summary(format!("{hook_name}: {decision_preview}")),
                created_at_ms,
                expires_at_ms,
                payload: serde_json::json!({
                    "hook_name": hook_name,
                    "decision_preview": decision_preview,
                }),
            })
            .await;

        let ev = Event::new(
            self.sub_id.clone(),
            EventMsg::ApprovalRequest(ApprovalRequestEvent {
                request_id: request_id.clone(),
                kind: ApprovalKind::Hook {
                    hook_name: hook_name.to_string(),
                    decision_preview: decision_preview.clone(),
                },
                risk,
            }),
        );

        if let Err(decision) = self.send_approval_event(ev, cancel).await {
            return decision;
        }

        // v1.9 S6:共享绝对死线(同 ask_tool)。
        let mut ttl_fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
            match self.ttl_sleep() {
                Some(d) => Box::pin(tokio::time::sleep(d)),
                None => Box::pin(std::future::pending()),
            };

        let (decision, outcome) = tokio::select! {
            biased;
            _ = cancel.cancelled() => (
                ReviewDecision::Deny { reason: "cancelled".into() },
                InteractionOutcome::Cancelled,
            ),
            // 回合级中断(Op::Interrupt)。
            _ = self.wait_turn_cancel() => (
                ReviewDecision::Deny { reason: "turn interrupted".into() },
                InteractionOutcome::Interrupted,
            ),
            // v1.9 S6:审批超时。
            _ = &mut ttl_fut => (
                ReviewDecision::Deny { reason: super::expired_reason() },
                InteractionOutcome::Expired,
            ),
            res = rx => match res {
                Ok(decision) => (decision, InteractionOutcome::Answered),
                Err(_) => (
                    ReviewDecision::Deny { reason: "approval channel dropped".into() },
                    InteractionOutcome::ChannelClosed,
                ),
            },
        };
        // v1.12:了结注册表条目 + 广播 + 落盘(TTL 清扫先行时自动跳过)。
        self.resolve_pending(&request_id, outcome).await;
        decision
        // `_lease` / `pending_lease` 在此 drop:waiter 表与注册表统一兜底清理。
    }

    /// v1.1.0 P1 #14:LLM 主动向用户发起 1-4 道结构化问题。emit
    /// `EventMsg::AskUserQuestion`,oneshot 等待 `Op::AskUserQuestionResponse`
    /// 回执或 cancel。
    ///
    /// # Errors
    ///
    /// - `ToolError::InvalidArgs`:`questions` 为空 / 超过 4 道 / 某道
    ///   `header` 超过 12 字符 / 某道 `options` 数量不在 [2,4] 范围。
    /// - `ToolError::Cancelled`:`cancel` token 在等待响应时触发。
    /// - `ToolError::Execution`:event channel 已关闭,无法 emit。
    ///
    /// # Flow
    ///
    /// 1. 校验 `questions`(数量、每道 `header` / `options` 边界)。
    /// 2. 分配 `request_id` (uuid),oneshot channel 写入 `question_waiters`。
    /// 3. emit `EventMsg::AskUserQuestion` 给 TUI / headless 客户端。
    /// 4. `tokio::select!` 等回执 / cancel / channel 关闭。
    /// 5. 返回 `AskUserAnswer`(可能为空 — 用户按 Esc 取消)。
    pub async fn ask_question(
        &self,
        questions: Vec<Question>,
        cancel: &CancellationToken,
    ) -> Result<AskUserAnswer, ToolError> {
        // 1. 校验 questions 数量。
        if questions.is_empty() {
            return Err(ToolError::InvalidArgs {
                message: "ask_user_question: 'questions' must not be empty".into(),
            });
        }
        if questions.len() > zhuye_protocol::question::MAX_QUESTIONS {
            return Err(ToolError::InvalidArgs {
                message: format!(
                    "ask_user_question: too many questions ({} > {})",
                    questions.len(),
                    zhuye_protocol::question::MAX_QUESTIONS
                ),
            });
        }
        // 2. 校验每道 question(把 `Question::new` 的 Result 转 ToolError)。
        for (i, q) in questions.iter().enumerate() {
            if q.options.len() < zhuye_protocol::question::MIN_OPTIONS
                || q.options.len() > zhuye_protocol::question::MAX_OPTIONS
            {
                return Err(ToolError::InvalidArgs {
                    message: format!(
                        "ask_user_question: question[{i}] options.len() = {} (must be in [{}, {}])",
                        q.options.len(),
                        zhuye_protocol::question::MIN_OPTIONS,
                        zhuye_protocol::question::MAX_OPTIONS
                    ),
                });
            }
            if q.header.chars().count() > zhuye_protocol::question::MAX_HEADER_CHARS {
                return Err(ToolError::InvalidArgs {
                    message: format!(
                        "ask_user_question: question[{i}] header too long ({} > {})",
                        q.header.chars().count(),
                        zhuye_protocol::question::MAX_HEADER_CHARS
                    ),
                });
            }
        }

        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel::<AskUserAnswer>();
        self.question_waiters.lock().insert(request_id.clone(), tx);

        // v1.12:登记挂起交互快照(同 ask_tool;TTL = question_timeout_secs)。
        let created_at_ms = Self::now_ms();
        let expires_at_ms = Some(created_at_ms.saturating_add(self.question_timeout_secs * 1000));
        let summary = Self::bounded_summary(
            questions
                .iter()
                .map(|q| q.question.as_str())
                .collect::<Vec<_>>()
                .join(" / "),
        );
        let _pending_lease = self
            .register_pending(PendingInteraction {
                request_id: request_id.clone(),
                kind: InteractionKind::Question,
                summary,
                created_at_ms,
                expires_at_ms,
                payload: serde_json::json!({ "questions": questions }),
            })
            .await;

        let ev = Event::new(
            self.sub_id.clone(),
            EventMsg::AskUserQuestion(AskUserQuestionEvent::new(request_id.clone(), questions)),
        );

        if self.event_tx.send(ev).await.is_err() {
            self.question_waiters.lock().remove(&request_id);
            return Err(ToolError::Execution(
                "ask_user_question: event channel closed".into(),
            ));
        }

        // v1.12:等待结果同时给出回执与了结路径(取消 / 中断 / 超时 /
        // 应答 / 通道关闭),统一在 select 后清理 waiter + 注册表。
        let (result, outcome) = tokio::select! {
            biased;
            _ = cancel.cancelled() => (
                Err(ToolError::Cancelled),
                InteractionOutcome::Cancelled,
            ),
            // 回合级中断(Op::Interrupt)。
            _ = self.wait_turn_cancel() => (
                Err(ToolError::Cancelled),
                InteractionOutcome::Interrupted,
            ),
            // v1.x 修复:绝对死线 —— 此前 ask_question 无任何 TTL,headless
            // 强开审批(ZHUYE_APPROVALS=1 / serve approvals)且无人应答时
            // 永久挂起,只能 Ctrl-C。与 ask_user 的默认 900s 对齐;GUI 交互
            // 场景用户 15 分钟未答同样作废并回错,优于无限等待。
            _sleep = tokio::time::sleep(std::time::Duration::from_secs(
                self.question_timeout_secs,
            )) => (
                Err(ToolError::Execution(format!(
                    "ask_user_question: timed out after {}s waiting for response",
                    self.question_timeout_secs
                ))),
                InteractionOutcome::Expired,
            ),
            res = rx => match res {
                Ok(answers) => (Ok(answers), InteractionOutcome::Answered),
                Err(_) => (
                    Err(ToolError::Execution(
                        "ask_user_question: waiter dropped before response".into(),
                    )),
                    InteractionOutcome::ChannelClosed,
                ),
            }
        };
        self.question_waiters.lock().remove(&request_id);
        // v1.12:广播 InteractionResolved + 落盘 resolution(幂等:应答方
        // 与其他客户端都能据此清理本地挂起态)。
        self.resolve_pending(&request_id, outcome).await;
        result
        // `pending_lease` 在此 drop:注册表兜底清理。
    }

    /// Bubble 模式:emit 非阻塞通知,不等待用户决策。
    async fn emit_permission_bubble(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        risk: RiskLevel,
    ) {
        let preview = args
            .as_object()
            .map(|o| {
                o.iter()
                    .take(3)
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|s| !s.is_empty());
        let ev = Event::new(
            self.sub_id.clone(),
            EventMsg::PermissionBubble(PermissionBubbleEvent {
                tool_name: tool_name.to_string(),
                args_preview: preview,
                risk,
            }),
        );
        let _ = self.event_tx.send(ev).await;
    }

    /// v1.1.0 P1 #15:LLM 向用户发起自由文本询问。emit `AskUserInput`,
    /// oneshot 等待 `Op::AskUserInputResponse` 或 cancel。
    ///
    /// # 超时(v1.2 review P1:bug-1)
    ///
    /// `timeout_secs` 控制最大等待秒数;`0` 表示永不超时(由 cancel token
    /// 或 event channel 关闭兜底,适合 `request_human_input` 这类持久化
    /// 等待场景)。`> 0` 时 `tokio::time::sleep` 与 cancel / rx 一起
    /// `tokio::select!`;超时到达返回
    /// `ToolError::Execution("ask_user: timed out after N seconds")`,
    /// 同步清理 `user_input_waiters` 防止泄漏。
    ///
    /// # 权限短路(v1.2 review P1:bug-2)
    ///
    /// `tool_name` 用于 resolver 查询(`ask_user` / `request_human_input`)。
    /// - resolver `Deny` rule → `InvalidArgs`(显式规则赢)。
    /// - session `PermissionMode::Deny` → `InvalidArgs`。
    /// - session `PermissionMode::Bubble` → `InvalidArgs`(无法对自由文本
    ///   做"自动同意",保守退化为 Deny)。
    /// - resolver `Allow` rule → **不**短路(自由文本没有合法默认值,
    ///   必须由用户显式输入;该规则对 `ask_user` 无效)。
    ///
    /// # Prompt 长度(v1.2 review P2:bug-2)
    ///
    /// `prompt` 字节数超 [`MAX_ASK_USER_PROMPT_BYTES`] 直接拒;防止
    /// LLM 误发 100 MB 字符串挤爆 event channel + TUI 渲染。
    pub async fn ask_user(
        &self,
        tool_name: &str,
        prompt: impl Into<String>,
        cancel: &CancellationToken,
        timeout_secs: u64,
    ) -> Result<String, ToolError> {
        // v1.2 P0:委托给带选项的核心实现(默认非敏感)。
        self.ask_user_opts(tool_name, prompt, cancel, timeout_secs, false, None)
            .await
    }

    /// v1.2 P0:带 `secret` / `placeholder` 选项的 `ask_user`。
    /// `secret = true` 时 TUI 把输入渲染为 `•` 掩码(密码 / API key);
    /// `placeholder` 提供空输入时的占位提示。回执 `text` 仍为明文。
    pub async fn ask_user_opts(
        &self,
        tool_name: &str,
        prompt: impl Into<String>,
        cancel: &CancellationToken,
        timeout_secs: u64,
        secret: bool,
        placeholder: Option<&str>,
    ) -> Result<String, ToolError> {
        let prompt = prompt.into();
        if prompt.trim().is_empty() {
            return Err(ToolError::InvalidArgs {
                message: "ask_user: 'prompt' must not be empty".into(),
            });
        }
        if prompt.len() > MAX_ASK_USER_PROMPT_BYTES {
            return Err(ToolError::InvalidArgs {
                message: format!(
                    "ask_user: 'prompt' too long ({} > {} bytes)",
                    prompt.len(),
                    MAX_ASK_USER_PROMPT_BYTES
                ),
            });
        }

        // v1.2 review P1:bug-2:权限短路(镜像 ask_tool:294-378)。
        // 1. resolver 短路:`Deny` 立即拒绝;`Allow` 不短路(自由文本无默认)。
        if let Some(resolver) = &self.permission_resolver {
            if let zhuye_permissions::RuleMatch::Deny = resolver.resolve(tool_name).await {
                return Err(ToolError::InvalidArgs {
                    message: format!("ask_user: denied by rule for {tool_name}"),
                });
            }
        }
        // 2. session `PermissionMode` 短路:`Deny` / `Bubble` 拒绝。
        if let Some(mode_ref) = &self.session_permission_mode {
            let mode = *mode_ref.read();
            if mode == PermissionMode::Deny {
                return Err(ToolError::InvalidArgs {
                    message: "ask_user: session permission mode is deny".into(),
                });
            }
            if mode == PermissionMode::Bubble {
                return Err(ToolError::InvalidArgs {
                    message:
                        "ask_user: session permission mode is bubble (no auto-answer for free text)"
                            .into(),
                });
            }
        }

        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel::<String>();
        self.user_input_waiters
            .lock()
            .insert(request_id.clone(), tx);

        // v1.12:登记挂起交互快照(timeout_secs = 0 表示永不超时 → 无过期)。
        let created_at_ms = Self::now_ms();
        let expires_at_ms =
            (timeout_secs > 0).then(|| created_at_ms.saturating_add(timeout_secs * 1000));
        let prompt_text = prompt;
        let _pending_lease = self
            .register_pending(PendingInteraction {
                request_id: request_id.clone(),
                kind: InteractionKind::AskUser,
                summary: Self::bounded_summary(prompt_text.clone()),
                created_at_ms,
                expires_at_ms,
                payload: serde_json::json!({
                    "prompt": prompt_text,
                    "secret": secret,
                    "placeholder": placeholder,
                }),
            })
            .await;

        // v1.2 P0:透传 secret / placeholder 到事件(向后兼容:旧 consumer 默认 false/None)。
        let mut event = AskUserInputEvent::new(request_id.clone(), prompt_text);
        if secret {
            event = event.with_secret(true);
        }
        if let Some(ph) = placeholder {
            event = event.with_placeholder(ph);
        }
        let ev = Event::new(self.sub_id.clone(), EventMsg::AskUserInput(event));

        if self.event_tx.send(ev).await.is_err() {
            self.user_input_waiters.lock().remove(&request_id);
            return Err(ToolError::Execution(
                "ask_user: event channel closed".into(),
            ));
        }

        // v1.2 review P1:bug-1:把"rx 收到用户响应"包装成 Result,避免
        // `match res { ... }` 在 `tokio::select!` handler 里的复杂模式让
        // macro parser 困惑。
        let rx_result: Result<String, String> = if timeout_secs == 0 {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err("cancelled".into()),
                // 回合级中断(Op::Interrupt)。
                _ = self.wait_turn_cancel() => Err("turn interrupted".into()),
                recv = rx => recv.map_err(|_| "dropped".into()),
            }
        } else {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err("cancelled".into()),
                // 回合级中断(Op::Interrupt)。
                _ = self.wait_turn_cancel() => Err("turn interrupted".into()),
                recv = rx => recv.map_err(|_| "dropped".into()),
                _sleep = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)) => {
                    Err(format!("timeout:{timeout_secs}"))
                }
            }
        };

        // v1.12:由等待结果映射了结路径,统一清理 waiter + 注册表。
        let outcome = match &rx_result {
            Ok(_) => InteractionOutcome::Answered,
            Err(reason) if reason == "cancelled" => InteractionOutcome::Cancelled,
            Err(reason) if reason == "turn interrupted" => InteractionOutcome::Interrupted,
            Err(reason) if reason == "dropped" => InteractionOutcome::ChannelClosed,
            Err(reason) if reason.starts_with("timeout:") => InteractionOutcome::Expired,
            Err(_) => InteractionOutcome::ChannelClosed,
        };
        self.user_input_waiters.lock().remove(&request_id);
        self.resolve_pending(&request_id, outcome).await;

        match rx_result {
            Ok(text) => Ok(text),
            Err(reason) if reason == "cancelled" || reason == "turn interrupted" => {
                Err(ToolError::Cancelled)
            }
            Err(reason) if reason == "dropped" => Err(ToolError::Execution(
                "ask_user: waiter dropped before response".into(),
            )),
            Err(reason) if reason.starts_with("timeout:") => Err(ToolError::Execution(format!(
                "ask_user: timed out after {timeout_secs} seconds"
            ))),
            Err(other) => Err(ToolError::Execution(format!(
                "ask_user: unexpected wait outcome: {other}"
            ))),
        }
        // `pending_lease` 在此 drop:注册表兜底清理。
    }
}

#[cfg(test)]
mod lease_tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn concurrent_registration_respects_cap() {
        let map: ApprovalWaiters = Default::default();
        let mut held = Vec::new();
        for i in 0..MAX_PENDING_APPROVALS - 1 {
            let (tx, _rx) = oneshot::channel();
            held.push(WaiterLease::register(&map, i.to_string(), tx).unwrap());
        }
        let barrier = Arc::new(Barrier::new(9));
        // 先 collect 再统一 wait:8 个线程在两次 barrier.wait 之间注册,
        // 链式惰性求值会破坏「并发注册被容量上限约束」的被测行为。
        #[expect(clippy::needless_collect)]
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let (tx, _rx) = oneshot::channel();
                    barrier.wait();
                    let lease = WaiterLease::register(&map, format!("concurrent-{i}"), tx);
                    barrier.wait();
                    lease
                })
            })
            .collect();
        barrier.wait();
        barrier.wait();
        let leases: Vec<_> = threads
            .into_iter()
            .filter_map(|t| t.join().unwrap())
            .collect();
        assert_eq!(leases.len(), 1);
        assert_eq!(map.lock().len(), MAX_PENDING_APPROVALS);
    }
}
