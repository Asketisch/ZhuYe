//! `steering_queue` —— Steering Queue(P2 `steering-queue`)。
//!
//! v1.9 C6:**两级输入队列**(deepseek-harness inbox 语义):
//! - **turn 级**:turn 边界注入(Attachment / Now);
//! - **step 级**:只注入 tool_exec 回环的 step 边界(`Step`),
//!   不并入 turn 起始输入 —— "针对当前这一步"的补充指令,
//!   turn 开头注入反而错过语境。
//!
//! ATTACHMENT / NOW 双优先级用户消息注入;turn 边界 drain 高优先级队列。

use std::collections::VecDeque;

use zhuye_protocol::UserInputItem;

///  steering 消息优先级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SteeringPriority {
    /// 普通附件,下一 safe point 注入。
    Attachment = 0,
    /// 立即打断当前 turn 规划。
    Now = 1,
    /// v1.9 C6:step 级 —— 只在 step 边界(ToolExec 回环后的下一次
    /// model_call 前)注入,turn 起始不注入。
    Step = 2,
}

/// 一条 steering 消息。
#[derive(Debug, Clone)]
pub struct SteeringMessage {
    pub priority: SteeringPriority,
    pub items: Vec<UserInputItem>,
}

/// 两级 steering 队列。
#[derive(Debug, Default)]
pub struct SteeringQueue {
    attachment: VecDeque<SteeringMessage>,
    now: VecDeque<SteeringMessage>,
    /// v1.9 C6:step 级队列(独立,不随 turn 边界清空)。
    step: VecDeque<SteeringMessage>,
}

impl SteeringQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// 入队 steering 消息(Step 优先级请用 [`SteeringQueue::push_step`],
    /// 语义相同,此处也接受)。
    pub fn push(&mut self, priority: SteeringPriority, items: Vec<UserInputItem>) {
        let msg = SteeringMessage { priority, items };
        match priority {
            SteeringPriority::Attachment => self.attachment.push_back(msg),
            SteeringPriority::Now => self.now.push_back(msg),
            SteeringPriority::Step => self.step.push_back(msg),
        }
    }

    /// v1.9 C6:入队 step 级消息。
    pub fn push_step(&mut self, items: Vec<UserInputItem>) {
        self.push(SteeringPriority::Step, items);
    }

    /// 是否包含 NOW 优先级消息。
    pub fn has_now(&self) -> bool {
        !self.now.is_empty()
    }

    /// v1.9 C6:是否有 step 级消息(step 边界判定用)。
    pub fn has_step(&self) -> bool {
        !self.step.is_empty()
    }

    /// drain 全部 NOW 消息(高优先级优先)。
    pub fn drain_now(&mut self) -> Vec<SteeringMessage> {
        self.now.drain(..).collect()
    }

    /// drain 全部 ATTACHMENT 消息。
    pub fn drain_attachment(&mut self) -> Vec<SteeringMessage> {
        self.attachment.drain(..).collect()
    }

    /// v1.9 C6:drain 全部 step 级消息。
    pub fn drain_step(&mut self) -> Vec<SteeringMessage> {
        self.step.drain(..).collect()
    }

    /// 按优先级 drain:先 NOW 后 ATTACHMENT(**不含 step 级**)。
    ///
    /// turn 边界与 step 边界的收割都走这里 + [`SteeringQueue::drain_step`]
    /// 的组合;pre_loop 按 `state.iteration` 区分:turn 起始只收本函数,
    /// step 回环再收 [`SteeringQueue::drain_step`]。
    pub fn drain_all(&mut self) -> Vec<SteeringMessage> {
        let mut out = self.drain_now();
        out.extend(self.drain_attachment());
        out
    }

    /// v1.9 C6:step 边界收割 = step 级优先,再普通级(Now/Attachment)。
    pub fn drain_for_step(&mut self) -> Vec<SteeringMessage> {
        let mut out = self.drain_step();
        out.extend(self.drain_all());
        out
    }

    pub fn len(&self) -> usize {
        self.attachment.len() + self.now.len() + self.step.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Vec<UserInputItem> {
        vec![UserInputItem::Text { text: s.into() }]
    }

    #[test]
    fn now_drains_before_attachment() {
        let mut q = SteeringQueue::new();
        q.push(SteeringPriority::Attachment, text("attach"));
        q.push(SteeringPriority::Now, text("now"));
        let drained = q.drain_all();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].priority, SteeringPriority::Now);
    }

    #[test]
    fn has_now_reflects_queue_state() {
        let mut q = SteeringQueue::new();
        assert!(!q.has_now());
        q.push(SteeringPriority::Now, vec![]);
        assert!(q.has_now());
    }

    // ── v1.9 C6:两级队列 ──

    #[test]
    fn step_tier_is_not_drained_by_drain_all() {
        let mut q = SteeringQueue::new();
        q.push_step(text("step-scoped"));
        q.push(SteeringPriority::Now, text("turn-scoped"));
        // turn 边界收割:只拿 turn 级。
        let drained = q.drain_all();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].priority, SteeringPriority::Now);
        // step 消息仍在队列。
        assert!(q.has_step());
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn step_boundary_drains_step_first_then_normal() {
        let mut q = SteeringQueue::new();
        q.push(SteeringPriority::Attachment, text("attach"));
        q.push_step(text("step-1"));
        q.push_step(text("step-2"));
        q.push(SteeringPriority::Now, text("now"));
        let drained = q.drain_for_step();
        // step 级先来(FIFO),再 Now,再 Attachment。
        assert_eq!(drained.len(), 4);
        assert_eq!(drained[0].priority, SteeringPriority::Step);
        assert_eq!(drained[1].priority, SteeringPriority::Step);
        assert_eq!(drained[2].priority, SteeringPriority::Now);
        assert_eq!(drained[3].priority, SteeringPriority::Attachment);
        assert!(q.is_empty());
    }

    #[test]
    fn step_queue_survives_turn_drain() {
        // 模拟:turn 1 结束时 drain_all,step 消息留到 turn 2 的
        // step 边界 —— 跨 turn 存活。
        let mut q = SteeringQueue::new();
        q.push_step(text("late instruction"));
        let _ = q.drain_all(); // turn 边界
        assert_eq!(q.len(), 1);
        let drained = q.drain_for_step(); // 下一 turn 的 step 边界
        assert_eq!(drained.len(), 1);
        assert!(q.is_empty());
    }

    #[test]
    fn has_step_reflects_queue_state() {
        let mut q = SteeringQueue::new();
        assert!(!q.has_step());
        q.push_step(text("s"));
        assert!(q.has_step());
    }

    #[test]
    fn push_routes_step_priority_via_push_too() {
        let mut q = SteeringQueue::new();
        q.push(SteeringPriority::Step, text("via push"));
        assert!(q.has_step());
        assert_eq!(q.drain_step().len(), 1);
    }
}
