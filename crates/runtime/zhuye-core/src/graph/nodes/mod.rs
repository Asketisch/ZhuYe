//! 4 节点图的节点实现(M2/M3)。
//!
//! 每个函数接收 `&mut AgentState` 与共享上下文(registry、hooks、tools 等),
//! 返回下一个要运行的节点;若 turn 结束则返回 `None`。
//!
//! 实现拆分到兄弟模块:
//! - [`pre_loop`] — 完整 turn 前置流水线(compact、memory、system prompt)。
//! - [`model_call`] — LLM 调用 + 流式 + retry/failover。
//! - [`tool_exec`] — 经 queue 分发 tool_use blocks。
//! - [`check_stop`] — 分发 `Stop` hook。
//! - [`nudge`] — progress-nudge / loop-guard 辅助 + `url_host`。
//! - [`retry`] — LLM 错误 → retry/failover 分类。
//!
//! 下方的 `pub use` 重新导出保留历史 `crate::graph::nodes::<item>` API
//! 形态(被 `graph::StateGraph` 与集成测试使用)。

mod check_stop;
mod model_call;
mod nudge;
mod pre_loop;
mod retry;
mod tool_exec;

pub use check_stop::check_stop;
pub use model_call::model_call;
pub use nudge::{maybe_inject_progress_nudge, url_host};
pub use pre_loop::pre_loop;
pub(crate) use tool_exec::read_latest_plan_file;
pub(crate) use tool_exec::render_todo_checklist;
pub use tool_exec::tool_exec;

use zhuye_protocol::{ContentBlock, MessageRole, RolloutRecord};

use crate::submission_loop::NodeContext;

/// 按持久化顺序提交待办,避免并发回合中的快照读到过期版本。
pub(super) async fn persist_tool_results(
    ctx: &NodeContext,
    blocks: &[ContentBlock],
    checklist: Option<String>,
) {
    if let Some(checklist) = checklist {
        let _order = ctx.todo_persistence.lock().await;
        persist_assistant_step(ctx, blocks).await;
        *ctx.todos_slot.write() = Some(checklist);
    } else {
        persist_assistant_step(ctx, blocks).await;
    }
}

pub(super) async fn persist_dynamic_checklist(
    ctx: &NodeContext,
    recorder: &dyn zhuye_protocol::RolloutRecorder,
) {
    // 修复原因:读取和落盘必须与 TodoWrite 提交互斥,否则并发的
    // 新回执先落盘时,旧快照可能在恢复时覆盖它。
    let _order = ctx.todo_persistence.lock().await;
    let checklist = ctx.todos_slot.read().clone().unwrap_or_default();
    let _ = recorder
        .record(RolloutRecord::DynamicContextSnapshot {
            turn_id: ctx.turn_id,
            checklist,
        })
        .await;
}

/// 把本步已完成的 assistant 内容增量落盘。
///
/// 之前 assistant 消息只在**回合结束**时整体写一条。回合未结束(阻塞在 plan
/// 审批、被中断、进程退出)时,JSONL 里没有任何 assistant 记录,解析
/// session 文件复现不出助手历史。现在每个已完成步骤(一次 model_call 的
/// 文本 + 工具调用,或一批工具结果)立即以**同一 turn_id** 追加一条 assistant
/// 记录。
///
/// 消费端按 turn 归并:`records_to_preload` 把同 turn 的连续 assistant 记录
/// 拼成一条,块顺序与改动前的整体落盘逐块一致。回合收尾**不再**写
/// 一次整体记录, 否则历史会被重复回填。
///
/// best-effort:写失败只记日志,不阻断回合。空块不写。
pub(crate) async fn persist_assistant_step(ctx: &NodeContext, blocks: &[ContentBlock]) {
    if blocks.is_empty() {
        return;
    }
    let Some(recorder) = ctx.recorder.as_ref() else {
        return;
    };
    if let Err(error) = recorder
        .record(RolloutRecord::message(
            ctx.turn_id,
            MessageRole::Assistant,
            serde_json::to_value(blocks).unwrap_or(serde_json::Value::Null),
        ))
        .await
    {
        tracing::warn!(error = %error, "incremental assistant rollout record failed");
    }
}
