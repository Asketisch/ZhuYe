//! `PendingSubagentRegistry` —— 在飞「异步子代理委派」登记表。
//!
//! ## 为什么在 tools 层
//!
//! 异步子代理派发需要**同一个对象**同时被两边访问:
//! - 工具侧(`call_<role>` 工具):派发时登记、子代理终态时回填结果;
//! - 引擎侧(`zhuye-core` 的 `check_stop` 节点):回合收尾前挂起等待、
//!   拿到结果后注入上下文并续跑本轮。
//!
//! 依赖方向是 `zhuye-subagent → zhuye-core → zhuye-tools`,`zhuye-core`
//! 不能反向依赖 `zhuye-subagent`。因此登记表下沉到 `zhuye-tools`,并由
//! [`crate::ToolContext`] 承载 —— `ToolContext` 是**每线程一份**(基座
//! context 在 `AgentThread::new` 里按 `AgentConfig` 构造),这正是嵌套
//! 场景需要的隔离:子代理线程用自己的表,孙代理的完成不会误唤醒父回合。
//!
//! ## 写入模型
//!
//! 登记方唯一(`register` 由派发方调,`complete` 由该派发方派生的后台
//! 任务调),读方任意多(引擎的 `check_stop`)。表项用 `parking_lot::Mutex`
//! 保护,写者只有派发方自己,读者只做 clone。
//!
//! ## 生命周期:异步子代理不跨越派发它的回合
//!
//! 同步模式下子代理活在工具调用里,工具 future 随父回合结束被丢弃,子代理
//! 一并取消。异步模式把子代理交给后台任务,这条级联**不会自动发生**
//! (后台任务不随工具 future 析构),必须显式接线:
//!
//! - 每条登记带 `owner`(父回合的回合级取消令牌)与 `abort`(放弃句柄);
//! - 后台任务 `select!` 同时监听 `abort` 与 `owner` —— 父回合被打断时子代理
//!   **立刻**停下,不等父回合自己收尾;
//! - 引擎在回合收尾时调 [`PendingSubagentRegistry::abandon_turn`]:仍在表里
//!   的本回合条目(在飞的、已完成未取走的)全部移除并触发 `abort`。
//!
//! 不这么做的后果(修复前的实际行为):用户中断挂起中的父回合后,子代理
//! 继续跑完,结果留在表里被之后某个回合注入;若它卡在审批上,此后每个
//! 回合的 `check_stop` 都会为它挂起 —— 一次中断拖垮整个会话。
//!
//! ## 终态语义
//!
//! `complete` **首个终态为准**(幂等):后台任务正常完成时写入真实结果,
//! 而任务被中止(runtime 掉线 / panic)时由 [`PendingCompletionGuard`]
//! 的析构兜底写入「已放弃」—— 两者竞争时先到者胜,不会出现「真实结果被
//! 兜底覆盖」。条目已被取走或放弃之后到达的终态一律丢弃,不会重建条目。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// 一个异步子代理的终态结果。
#[derive(Debug, Clone)]
pub struct PendingSubagentResult {
    /// 子代理会话 id(与 `SubagentStatusSnapshot.child_id` 同源)。
    pub child_id: String,
    /// 子代理角色名(`call_<role>` 的 role)。
    pub role: String,
    /// 父侧发起这次委派的 tool_use 调用 id(诊断 / 配对用)。
    pub call_id: String,
    /// 是否正常完成(`false` 表示失败或被放弃)。
    pub ok: bool,
    /// 抽取出的最终结果文本(失败时为空)。
    pub text: String,
    /// 失败原因(成功时为 `None`)。
    pub error: Option<String>,
}

/// 表内单条登记。
struct PendingEntry {
    role: String,
    call_id: String,
    /// 派发方(父回合)的回合级取消令牌。引擎在回合收尾时按它定位
    /// 「本回合派发的条目」(见 [`PendingSubagentRegistry::abandon_turn`])。
    owner: CancellationToken,
    /// 放弃句柄:引擎放弃本条目时触发,后台任务据此停掉子代理。
    abort: CancellationToken,
    /// 已终结则为 `Some`;`None` 表示仍在飞。
    result: Option<PendingSubagentResult>,
}

/// 在飞异步子代理登记表(每线程一份)。
///
/// 用法:派发方 `register` → 后台任务跑完后 `complete`;引擎在回合收尾
/// 前 `wait_any` 挂起等待,拿到结果后注入上下文并续跑本轮;回合结束时
/// `abandon_turn` 丢弃本回合来不及等待的条目。
pub struct PendingSubagentRegistry {
    entries: Mutex<HashMap<String, PendingEntry>>,
    /// 完成信号:`complete` / `abandon_turn` 时 bump,`wait_any` 监听。
    ///
    /// 用 `watch` 而非 `Notify`:`watch::Sender::subscribe()` 可以在每次
    /// 等待时**新建**一个 receiver(不需要在表里存共享 receiver,也就
    /// 不需要给 receiver 加锁),且 `subscribe` 先于状态检查的顺序能保证
    /// 「检查为空 → 等待」之间不丢唤醒。
    signal: watch::Sender<u64>,
    /// 信号计数值(单调递增,仅用于让 `watch` 感知变化)。
    counter: AtomicU64,
}

impl std::fmt::Debug for PendingSubagentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingSubagentRegistry")
            .field("pending", &self.pending_count())
            .finish_non_exhaustive()
    }
}

impl Default for PendingSubagentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PendingSubagentRegistry {
    /// 构造空表。
    pub fn new() -> Self {
        let (signal, _rx) = watch::channel(0u64);
        Self {
            entries: Mutex::new(HashMap::new()),
            signal,
            counter: AtomicU64::new(0),
        }
    }

    /// 登记一个在飞异步子代理,返回它的**放弃句柄**。
    ///
    /// `owner` 是派发方(父回合)的回合级取消令牌:后台任务应同时监听它
    /// (父回合被打断时立刻停子代理),引擎则在回合收尾时按它定位并放弃
    /// 本回合的条目(见 [`Self::abandon_turn`])。
    ///
    /// 同一 `child_id` 重复登记以**后写为准**(子代理 id 唯一,正常不会
    /// 重复;真重复说明调用方有 bug,后写能保住最新上下文)。
    pub fn register(
        &self,
        child_id: impl Into<String>,
        role: impl Into<String>,
        call_id: impl Into<String>,
        owner: CancellationToken,
    ) -> CancellationToken {
        let child_id = child_id.into();
        let abort = CancellationToken::new();
        self.entries.lock().insert(
            child_id,
            PendingEntry {
                role: role.into(),
                call_id: call_id.into(),
                owner,
                abort: abort.clone(),
                result: None,
            },
        );
        abort
    }

    /// 回填终态结果。**首个终态为准**(幂等):已终结的条目不会被覆盖。
    ///
    /// 条目不存在(已被 `take_completed` 取走,或已被 `abandon_turn` 放弃)
    /// 时**忽略**,不重建条目。
    ///
    /// 修复原因:此前用 `entry().or_insert_with()`,迟到的终态会凭空建出
    /// 一条新记录。触发场景:后台任务写入真实结果后,兜底守卫在析构时补写
    /// 「已放弃」,而引擎恰好在两者之间取走了条目 —— 下一回合就会收到一条
    /// 并不存在的「子代理执行失败」。
    pub fn complete(&self, result: PendingSubagentResult) {
        {
            let mut g = self.entries.lock();
            let Some(entry) = g.get_mut(&result.child_id) else {
                return;
            };
            if entry.result.is_some() {
                // 已有终态(正常完成与兜底 guard 竞争)—— 保留先到者。
                return;
            }
            entry.result = Some(result);
        }
        self.notify();
    }

    /// 放弃某个父回合派发、仍登记在表的全部条目(在飞的与已完成未取走的
    /// 都算),返回放弃的数量。
    ///
    /// 引擎在**回合收尾**时调用:异步子代理不跨越派发它的回合。正常收尾时
    /// 表里已无本回合条目(`check_stop` 等到了全部结果),是空操作;回合被
    /// 打断 / 出错 / 触顶收尾时,来不及等待的子代理在此一并取消。
    ///
    /// 按 `owner` 令牌匹配而非清空整表:同一线程上可能叠着别的回合的条目,
    /// 不能被本回合的收尾误伤。
    pub fn abandon_turn(&self, owner: &CancellationToken) -> usize {
        let abandoned: Vec<CancellationToken> = {
            let mut g = self.entries.lock();
            // `CancellationToken` 的 `PartialEq` 按实例指针比较:克隆体相等,
            // `child_token()` 派生的子令牌不相等。
            g.extract_if(|_, entry| entry.owner == *owner)
                .map(|(_, entry)| entry.abort)
                .collect()
        };
        if abandoned.is_empty() {
            return 0;
        }
        // 锁外触发:后台任务被唤醒后可能回调本表,不在持锁时放行。
        for abort in &abandoned {
            abort.cancel();
        }
        self.notify();
        abandoned.len()
    }

    /// 表内条目数(含已完成但尚未被引擎取走的)。
    pub fn pending_count(&self) -> usize {
        self.entries.lock().len()
    }

    /// 表里是否还有条目(在飞,或已完成待取走)。
    pub fn has_pending(&self) -> bool {
        !self.entries.lock().is_empty()
    }

    /// 是否还有**某个父回合**派发的条目。
    ///
    /// 登记表是**每线程一份**,而同一线程上可能并发跑着多个回合
    /// (`submission_loop` 每条 UserInput 都 spawn 一个回合任务)。因此回合
    /// 只能看**自己**派发的条目 —— 否则 A 回合的子代理会被 B 回合等到,
    /// 并把结果注入 B 的上下文。
    pub fn has_pending_for(&self, owner: &CancellationToken) -> bool {
        self.entries
            .lock()
            .values()
            .any(|entry| entry.owner == *owner)
    }

    /// 在飞子代理的快照(诊断用;不含结果文本)。
    pub fn pending_snapshot(&self) -> Vec<(String, String, String)> {
        let g = self.entries.lock();
        let mut out: Vec<(String, String, String)> = g
            .iter()
            .map(|(id, e)| (id.clone(), e.role.clone(), e.call_id.clone()))
            .collect();
        // HashMap 迭代序随机 —— 排序保证输出确定性(测试 / 诊断可复现)。
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// 取走所有已终结的条目并从表中移除。
    pub fn take_completed(&self) -> Vec<PendingSubagentResult> {
        let mut g = self.entries.lock();
        let mut out = Vec::new();
        g.retain(|_, e| match e.result.take() {
            Some(r) => {
                out.push(r);
                false
            }
            None => true,
        });
        out
    }

    /// 取走**某个父回合**派发的、已终结的条目并从表中移除。
    ///
    /// 与 [`Self::take_completed`] 的差别只在归属:别的回合的条目原样留在
    /// 表里,等它们自己的回合来取。
    pub fn take_completed_for(&self, owner: &CancellationToken) -> Vec<PendingSubagentResult> {
        let mut g = self.entries.lock();
        let mut out = Vec::new();
        g.retain(|_, e| {
            if e.owner != *owner {
                // 别的回合的条目:原样保留。
                return true;
            }
            match e.result.take() {
                Some(r) => {
                    out.push(r);
                    false
                }
                None => true,
            }
        });
        out
    }

    /// 唤醒全部等待者重新检查表状态。
    fn notify(&self) {
        let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        self.signal.send_replace(n);
    }

    /// 挂起等待:直到有子代理终结(或被取消)。
    ///
    /// 返回 `Some(已终结的条目)` 表示有结果可注入;返回 `None` 表示
    /// **等待被取消**(回合中断 / 会话关闭)—— 调用方据此走取消路径,
    /// 不能把它当成「没有子代理」而继续收尾。
    ///
    /// 登记表里**没有在飞项**时立即返回空结果(不挂起)—— 否则调用方
    /// 在「检查有在飞项」与「进入等待」之间遇到一次竞态(条目刚被取走)
    /// 就会永久阻塞。
    ///
    /// 取消语义对齐 `CallSubAgentTool` 同步路径的 `tokio::select!`:取消优先
    /// 于完成(一个刚完成的子代理在回合被打断后不再值得回填)。
    ///
    /// 等待中的条目被 [`Self::abandon_turn`] 放弃时同样会醒来:表空则返回
    /// `Some(空)`,调用方据此继续收尾,不会为已放弃的条目挂死。
    ///
    /// **这是跨回合无差别版本** —— 同线程并发多个回合时请改用
    /// [`Self::wait_any_for`]。
    pub async fn wait_any(&self, cancel: &CancellationToken) -> Option<Vec<PendingSubagentResult>> {
        self.wait_inner(cancel, None).await
    }

    /// 挂起等待:**某个父回合**派发的子代理终结(或被取消)。
    ///
    /// 与 [`Self::wait_any`] 的差别只在归属:别的回合派发的条目既不会被等
    /// 到,也不会被取走。
    pub async fn wait_any_for(
        &self,
        owner: &CancellationToken,
        cancel: &CancellationToken,
    ) -> Option<Vec<PendingSubagentResult>> {
        self.wait_inner(cancel, Some(owner)).await
    }

    /// 等待实现:`owner` 为 `Some` 时只关心该回合的条目。
    async fn wait_inner(
        &self,
        cancel: &CancellationToken,
        owner: Option<&CancellationToken>,
    ) -> Option<Vec<PendingSubagentResult>> {
        loop {
            // 先订阅再检查:订阅动作把「当前值」标记为已见,随后
            // 取结果若为空,则下一次 `complete` 的 bump 一定能唤醒本等待者
            // (不丢唤醒)。
            let mut rx = self.signal.subscribe();
            let done = match owner {
                Some(o) => self.take_completed_for(o),
                None => self.take_completed(),
            };
            if !done.is_empty() {
                return Some(done);
            }
            let pending = match owner {
                Some(o) => self.has_pending_for(o),
                None => self.has_pending(),
            };
            if !pending {
                // 没有在飞项 → 无事可等,立即返回(不挂起)。
                return Some(done);
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return None,
                changed = rx.changed() => {
                    if changed.is_err() {
                        // `signal` 的 sender 随 registry 存活,正常不会
                        // 掉;真掉了说明表已销毁,按「无结果」处理。
                        return None;
                    }
                }
            }
        }
    }
}

/// 未完成兜底守卫:析构时若该条目仍在表里且未终结,标记为「已放弃」。
///
/// 用法:派发方 `register` 后立即构造本守卫并移进后台任务;任务正常
/// 跑完时先 `complete`(真实结果),守卫析构时的兜底 `complete` 因
/// 「首个终态为准」被忽略;后台任务意外中止(runtime 掉线、panic)时
/// 守卫是唯一写终态的一方,条目不会永远停在「在飞」。
///
/// 注意它**管不到**父回合被打断的情形 —— 那条路径上后台任务并未中止,
/// 守卫不会析构。让子代理随父回合取消的是 `register` 的 `owner` 令牌 +
/// 引擎收尾时的 [`PendingSubagentRegistry::abandon_turn`](见模块头)。
/// 条目已被取走 / 放弃后守卫才析构时,补写的终态因条目不存在被丢弃。
pub struct PendingCompletionGuard {
    registry: Arc<PendingSubagentRegistry>,
    child_id: String,
    role: String,
    call_id: String,
}

impl std::fmt::Debug for PendingCompletionGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingCompletionGuard")
            .field("child_id", &self.child_id)
            .finish_non_exhaustive()
    }
}

impl PendingCompletionGuard {
    /// 为已登记的子代理构造兜底守卫。
    pub fn new(
        registry: Arc<PendingSubagentRegistry>,
        child_id: impl Into<String>,
        role: impl Into<String>,
        call_id: impl Into<String>,
    ) -> Self {
        Self {
            registry,
            child_id: child_id.into(),
            role: role.into(),
            call_id: call_id.into(),
        }
    }
}

impl Drop for PendingCompletionGuard {
    fn drop(&mut self) {
        self.registry.complete(PendingSubagentResult {
            child_id: self.child_id.clone(),
            role: self.role.clone(),
            call_id: self.call_id.clone(),
            ok: false,
            text: String::new(),
            error: Some("子代理任务被放弃(后台任务中断)".to_string()),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 派发方(父回合)的令牌;各用例互不相干,每次新建。
    fn owner() -> CancellationToken {
        CancellationToken::new()
    }

    fn result(child_id: &str, ok: bool) -> PendingSubagentResult {
        PendingSubagentResult {
            child_id: child_id.to_string(),
            role: "explorer".to_string(),
            call_id: "call-1".to_string(),
            ok,
            text: "结果文本".to_string(),
            error: (!ok).then(|| "失败原因".to_string()),
        }
    }

    #[test]
    fn register_then_complete_marks_entry() {
        let reg = PendingSubagentRegistry::new();
        reg.register("c1", "explorer", "call-1", owner());
        assert_eq!(reg.pending_count(), 1);
        assert!(reg.has_pending());

        reg.complete(result("c1", true));
        // 终结后条目仍在表里(等 take_completed 取走)。
        assert_eq!(reg.pending_count(), 1);

        let taken = reg.take_completed();
        assert_eq!(taken.len(), 1);
        assert!(taken[0].ok);
        assert_eq!(taken[0].text, "结果文本");
        assert_eq!(reg.pending_count(), 0, "取走后应从表中移除");
        assert!(!reg.has_pending());
    }

    /// 首个终态为准:兜底 guard 的写入不能覆盖真实结果。
    #[test]
    fn first_terminal_wins() {
        let reg = PendingSubagentRegistry::new();
        reg.register("c1", "explorer", "call-1", owner());
        reg.complete(result("c1", true));
        reg.complete(PendingSubagentResult {
            child_id: "c1".into(),
            role: "explorer".into(),
            call_id: "call-1".into(),
            ok: false,
            text: String::new(),
            error: Some("兜底".into()),
        });
        let taken = reg.take_completed();
        assert_eq!(taken.len(), 1);
        assert!(taken[0].ok, "真实结果必须胜出");
    }

    #[tokio::test]
    async fn wait_any_returns_on_complete() {
        let reg = Arc::new(PendingSubagentRegistry::new());
        reg.register("c1", "explorer", "call-1", owner());
        let cancel = CancellationToken::new();
        let reg2 = reg.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            reg2.complete(result("c1", true));
        });
        let got = reg.wait_any(&cancel).await;
        writer.await.unwrap();
        let got = got.expect("等待应返回结果");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].child_id, "c1");
    }

    /// 取消优先:等待被取消时返回 `None`(调用方不能当成「无子代理」)。
    #[tokio::test]
    async fn wait_any_returns_none_on_cancel() {
        let reg = PendingSubagentRegistry::new();
        reg.register("c1", "explorer", "call-1", owner());
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(reg.wait_any(&cancel).await.is_none());
    }

    /// 无在飞条目时不应挂起 —— 直接返回空结果。
    #[tokio::test]
    async fn wait_any_on_empty_registry_returns_immediately() {
        let reg = PendingSubagentRegistry::new();
        let cancel = CancellationToken::new();
        let got = reg.wait_any(&cancel).await.expect("不应被取消");
        assert!(got.is_empty());
    }

    /// 兜底守卫:后台任务被中止时,条目必须落到终态(否则后续回合会为它
    /// 永久挂起)。
    #[tokio::test]
    async fn completion_guard_marks_abandoned_entry() {
        let reg = Arc::new(PendingSubagentRegistry::new());
        reg.register("c1", "explorer", "call-1", owner());
        {
            let _guard = PendingCompletionGuard::new(reg.clone(), "c1", "explorer", "call-1");
            // 作用域结束即模拟「任务被中止」—— 守卫析构写终态。
        }
        let taken = reg.take_completed();
        assert_eq!(taken.len(), 1);
        assert!(!taken[0].ok);
        assert!(taken[0].error.is_some());
        assert_eq!(reg.pending_count(), 0);
    }

    /// 正常完成路径:真实结果先写入,守卫析构的兜底被忽略。
    #[tokio::test]
    async fn completion_guard_does_not_override_real_result() {
        let reg = Arc::new(PendingSubagentRegistry::new());
        reg.register("c1", "explorer", "call-1", owner());
        {
            let _guard = PendingCompletionGuard::new(reg.clone(), "c1", "explorer", "call-1");
            reg.complete(result("c1", true));
        }
        let taken = reg.take_completed();
        assert_eq!(taken.len(), 1);
        assert!(taken[0].ok, "守卫兜底不能覆盖真实结果");
    }

    /// 多子代理:一次等待取走全部已终结条目。
    #[tokio::test]
    async fn wait_any_collects_multiple_results() {
        let reg = Arc::new(PendingSubagentRegistry::new());
        reg.register("c1", "a", "call-1", owner());
        reg.register("c2", "b", "call-2", owner());
        let cancel = CancellationToken::new();
        let reg2 = reg.clone();
        tokio::spawn(async move {
            reg2.complete(result("c1", true));
            reg2.complete(result("c2", true));
        });
        let got = reg.wait_any(&cancel).await.expect("不应被取消");
        assert_eq!(got.len(), 2);
    }

    /// 快照按 child_id 排序 —— HashMap 迭代序随机,排序保证确定性。
    #[test]
    fn pending_snapshot_is_sorted() {
        let reg = PendingSubagentRegistry::new();
        reg.register("c3", "a", "call-3", owner());
        reg.register("c1", "b", "call-1", owner());
        let snap = reg.pending_snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].0, "c1");
        assert_eq!(snap[1].0, "c3");
    }

    /// 迟到的终态不能凭空建出条目。
    ///
    /// 回归场景:后台任务写入真实结果 → 引擎取走条目 → 兜底守卫才析构。
    /// 此前守卫的补写会让表里重新出现一条「已放弃」,下一回合据此注入一条
    /// 并不存在的「子代理执行失败」。
    #[test]
    fn late_terminal_does_not_resurrect_taken_entry() {
        let reg = Arc::new(PendingSubagentRegistry::new());
        let abort = reg.register("c1", "explorer", "call-1", owner());
        let guard = PendingCompletionGuard::new(reg.clone(), "c1", "explorer", "call-1");
        reg.complete(result("c1", true));
        assert_eq!(reg.take_completed().len(), 1);
        // 守卫在条目被取走之后才析构。
        drop(guard);
        assert!(!reg.has_pending(), "迟到的兜底终态不应重建条目");
        // 条目不存在时直接写入同样被忽略。
        reg.complete(result("ghost", false));
        assert!(!reg.has_pending());
        assert!(!abort.is_cancelled(), "已正常完成的条目不算被放弃");
    }

    /// 放弃按父回合令牌精确匹配:只动本回合的条目,并触发它们的放弃句柄。
    #[test]
    fn abandon_turn_targets_only_that_turn() {
        let reg = PendingSubagentRegistry::new();
        let turn_a = owner();
        let turn_b = owner();
        let abort_a = reg.register("a1", "explorer", "call-a1", turn_a.clone());
        let abort_b = reg.register("b1", "explorer", "call-b1", turn_b);
        let abort_c = reg.register("a2", "reviewer", "call-a2", turn_a.clone());

        assert_eq!(reg.abandon_turn(&turn_a), 2);
        assert!(
            abort_a.is_cancelled() && abort_c.is_cancelled(),
            "本回合条目的句柄应触发"
        );
        assert!(!abort_b.is_cancelled(), "别的回合的条目不能被误伤");
        assert_eq!(
            reg.pending_snapshot(),
            vec![("b1".into(), "explorer".into(), "call-b1".into())]
        );
        // 幂等:再放弃一次已无条目。
        assert_eq!(reg.abandon_turn(&turn_a), 0);
    }

    /// 已完成但来不及取走的结果随回合一起丢弃 —— 回合都结束了,再回填
    /// 只会把它塞进之后某个无关的回合。
    #[test]
    fn abandon_turn_drops_completed_untaken_results() {
        let reg = PendingSubagentRegistry::new();
        let turn = owner();
        reg.register("c1", "explorer", "call-1", turn.clone());
        reg.complete(result("c1", true));
        assert_eq!(reg.abandon_turn(&turn), 1);
        assert!(reg.take_completed().is_empty());
        assert!(!reg.has_pending());
    }

    /// 令牌身份按实例比较:同一回合令牌的克隆仍算同一条;`child_token()`
    /// 派生出的子令牌是另一个实例,不算同一个回合。
    #[test]
    fn abandon_turn_matches_token_identity() {
        let reg = PendingSubagentRegistry::new();
        let turn = owner();
        reg.register("c1", "explorer", "call-1", turn.clone());
        assert_eq!(
            reg.abandon_turn(&turn.child_token()),
            0,
            "子令牌不是同一个回合"
        );
        assert_eq!(reg.abandon_turn(&turn), 1, "克隆体是同一个回合");
    }

    /// 等待中的条目被放弃后,等待者必须醒来并拿到空结果(不能挂死)。
    #[tokio::test]
    async fn abandon_turn_releases_waiter() {
        let reg = Arc::new(PendingSubagentRegistry::new());
        let turn = owner();
        reg.register("c1", "explorer", "call-1", turn.clone());
        let waiter_reg = reg.clone();
        let waiter_cancel = owner();
        let waiter = tokio::spawn(async move { waiter_reg.wait_any(&waiter_cancel).await });
        // 让等待者先进入挂起(放弃发生在它检查之后才会真正检验唤醒)。
        tokio::task::yield_now().await;
        assert_eq!(reg.abandon_turn(&turn), 1);
        let got = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("放弃条目后等待者应被唤醒")
            .unwrap()
            .expect("等待未被取消");
        assert!(got.is_empty());
    }

    /// 按回合隔离:等待与取走都只认本回合的条目。
    ///
    /// 登记表是每线程一份,而同一线程上可能并发跑多个回合 —— 若按整表等待,
    /// A 回合的子代理会被 B 回合等到并把结果注入 B 的上下文。
    #[tokio::test]
    async fn wait_any_for_only_takes_that_turns_entries() {
        let reg = PendingSubagentRegistry::new();
        let turn_a = owner();
        let turn_b = owner();
        reg.register("a1", "explorer", "call-a1", turn_a.clone());
        reg.register("b1", "reviewer", "call-b1", turn_b.clone());

        assert!(reg.has_pending_for(&turn_a));
        assert!(reg.has_pending_for(&turn_b));

        // 两个回合的子代理都完成了 —— 先完成 B 的那条。
        reg.complete(result("b1", true));
        reg.complete(result("a1", true));
        let cancel = owner();
        let got = reg
            .wait_any_for(&turn_a, &cancel)
            .await
            .expect("不应被取消");
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(
            got[0].child_id, "a1",
            "A 回合只应取走自己派发的条目,不该取走 B 回合的"
        );

        // B 回合随后取走自己的 —— 之前被 A 回合跳过的结果仍在表里。
        let got = reg
            .wait_any_for(&turn_b, &cancel)
            .await
            .expect("不应被取消");
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].child_id, "b1");
    }

    /// 按回合等待:本回合没有在飞项时立即返回(不挂起)。
    #[tokio::test]
    async fn wait_any_for_on_other_turn_only_returns_immediately() {
        let reg = PendingSubagentRegistry::new();
        let turn_a = owner();
        let turn_b = owner();
        reg.register("b1", "explorer", "call-b1", turn_b);
        let cancel = owner();
        let got = reg
            .wait_any_for(&turn_a, &cancel)
            .await
            .expect("不应被取消");
        assert!(got.is_empty());
    }
}
