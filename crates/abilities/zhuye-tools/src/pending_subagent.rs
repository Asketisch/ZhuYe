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
//! ## 终态语义
//!
//! `complete` **首个终态为准**(幂等):后台任务正常完成时写入真实结果,
//! 而任务被中止(会话关闭 / runtime 掉线)时由 [`PendingCompletionGuard`]
//! 的析构兜底写入「已放弃」—— 两者竞争时先到者胜,不会出现「真实结果被
//! 兜底覆盖」。
//!
//! 兜底守卫的存在是**必需**的:没有它,一个被中止的异步子代理会让条目
//! 永远停在「在飞」,此后每一个回合的 `check_stop` 都会为它挂起等待 ——
//! 一次中止拖垮整个会话。

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
    /// 已终结则为 `Some`;`None` 表示仍在飞。
    result: Option<PendingSubagentResult>,
}

/// 在飞异步子代理登记表(会话级)。
///
/// 用法:派发方 `register` → 后台任务跑完后 `complete`;引擎在回合收尾
/// 前 `wait_any` 挂起等待,拿到结果后注入上下文并续跑本轮。
pub struct PendingSubagentRegistry {
    entries: Mutex<HashMap<String, PendingEntry>>,
    /// 完成信号:`complete` 时 bump,`wait_any` 监听。
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

    /// 登记一个在飞异步子代理。
    ///
    /// 同一 `child_id` 重复登记以**后写为准**(子代理 id 唯一,正常不会
    /// 重复;真重复说明调用方有 bug,后写能保住最新上下文)。
    pub fn register(
        &self,
        child_id: impl Into<String>,
        role: impl Into<String>,
        call_id: impl Into<String>,
    ) {
        let child_id = child_id.into();
        self.entries.lock().insert(
            child_id,
            PendingEntry {
                role: role.into(),
                call_id: call_id.into(),
                result: None,
            },
        );
    }

    /// 回填终态结果。**首个终态为准**(幂等):已终结的条目不会被覆盖。
    pub fn complete(&self, result: PendingSubagentResult) {
        {
            let mut g = self.entries.lock();
            let entry = g
                .entry(result.child_id.clone())
                .or_insert_with(|| PendingEntry {
                    role: result.role.clone(),
                    call_id: result.call_id.clone(),
                    result: None,
                });
            if entry.result.is_some() {
                // 已有终态(正常完成与兜底 guard 竞争)—— 保留先到者。
                return;
            }
            entry.result = Some(result);
        }
        let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        self.signal.send_replace(n);
    }

    /// 仍在飞的子代理数量。
    pub fn pending_count(&self) -> usize {
        self.entries.lock().len()
    }

    /// 是否还有在飞子代理。
    pub fn has_pending(&self) -> bool {
        !self.entries.lock().is_empty()
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
    /// 取消语义对齐 `CallSubAgentTool` 现有的 `tokio::select!`:取消优先
    /// 于完成(一个刚完成的子代理在回合被打断后不再值得回填)。
    pub async fn wait_any(&self, cancel: &CancellationToken) -> Option<Vec<PendingSubagentResult>> {
        loop {
            // 先订阅再检查:订阅动作把「当前值」标记为已见,随后
            // `take_completed` 若为空,则下一次 `complete` 的 bump 一定
            // 能唤醒本等待者(不丢唤醒)。
            let mut rx = self.signal.subscribe();
            let done = self.take_completed();
            if !done.is_empty() {
                return Some(done);
            }
            if !self.has_pending() {
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

/// 未完成兜底守卫:析构时若该条目仍未终结,标记为「已放弃」。
///
/// 用法:派发方 `register` 后立即构造本守卫并移进后台任务;任务正常
/// 跑完时先 `complete`(真实结果),守卫析构时的兜底 `complete` 因
/// 「首个终态为准」被忽略;任务被中止(会话关闭 / runtime 掉线)时
/// 守卫是唯一写终态的一方,条目不会永远停在「在飞」。
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
            error: Some("子代理任务被放弃(会话关闭或后台任务中断)".to_string()),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        reg.register("c1", "explorer", "call-1");
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
        reg.register("c1", "explorer", "call-1");
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
        reg.register("c1", "explorer", "call-1");
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
        reg.register("c1", "explorer", "call-1");
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
        reg.register("c1", "explorer", "call-1");
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
        reg.register("c1", "explorer", "call-1");
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
        reg.register("c1", "a", "call-1");
        reg.register("c2", "b", "call-2");
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
        reg.register("c3", "a", "call-3");
        reg.register("c1", "b", "call-1");
        let snap = reg.pending_snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].0, "c1");
        assert_eq!(snap[1].0, "c3");
    }
}
