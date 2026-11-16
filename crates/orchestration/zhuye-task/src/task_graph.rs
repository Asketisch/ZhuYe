//! # 任务系统增强(F56)
//!
//! 三件事:
//!
//! 1. **依赖图**([`TaskGraph`])—— A 完成才轮到 B。支持 blocked-by 关系、
//!    拓扑排序、就绪队列、死锁检测。
//! 2. **重复防护**([`DuplicateDetector`])—— 同一任务被重复创建时合并
//!    (而不是产生两个并行的重复工作)。
//! 3. **项目容器**([`ProjectBundle`])—— 目标 + 规则 + 记忆打包,便于
//!    切换项目上下文。
//!
//! ## 为什么死锁检测必须有
//!
//! 依赖图里 A 等 B、B 等 A → 两个任务永远不就绪。没有检测的话,用户
//! 看到的是「任务卡住了」但不知道为什么。[`TaskGraph::detect_cycle`]
//! 直接报出环上的任务名。

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

/// 任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// 被依赖阻塞。
    Blocked,
    /// 就绪可执行。
    Ready,
    /// 进行中。
    Running,
    /// 已完成。
    Done,
    /// 失败。
    Failed,
    /// 已取消。
    Cancelled,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Blocked => "blocked",
            TaskState::Ready => "ready",
            TaskState::Running => "running",
            TaskState::Done => "done",
            TaskState::Failed => "failed",
            TaskState::Cancelled => "cancelled",
        }
    }

    /// 是否终态。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Done | TaskState::Failed | TaskState::Cancelled
        )
    }

    /// 是否算"已完成"(依赖满足)。
    pub fn satisfies_dependency(self) -> bool {
        matches!(self, TaskState::Done)
    }
}

/// 一个任务节点。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskNode {
    pub id: String,
    pub title: String,
    pub state: TaskState,
    /// 依赖的任务 id(必须全部 Done 才就绪)。
    #[serde(default)]
    pub blocked_by: Vec<String>,
}

impl TaskNode {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> TaskNode {
        TaskNode {
            id: id.into(),
            title: title.into(),
            state: TaskState::Blocked,
            blocked_by: Vec::new(),
        }
    }

    pub fn blocked_by(mut self, deps: Vec<String>) -> TaskNode {
        self.blocked_by = deps;
        self
    }
}

/// 任务图。
#[derive(Debug, Default)]
pub struct TaskGraph {
    tasks: BTreeMap<String, TaskNode>,
}

impl TaskGraph {
    pub fn new() -> TaskGraph {
        TaskGraph::default()
    }

    /// 添加任务(依赖未知 id 时报错)。
    pub fn add(&mut self, node: TaskNode) -> Result<(), TaskGraphError> {
        for dep in &node.blocked_by {
            if dep == &node.id {
                return Err(TaskGraphError::SelfDependency(node.id.clone()));
            }
        }
        self.tasks.insert(node.id.clone(), node);
        Ok(())
    }

    /// 校验全部依赖指向存在任务。
    pub fn validate(&self) -> Result<(), TaskGraphError> {
        for node in self.tasks.values() {
            for dep in &node.blocked_by {
                if !self.tasks.contains_key(dep) {
                    return Err(TaskGraphError::UnknownDependency {
                        task: node.id.clone(),
                        dependency: dep.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&TaskNode> {
        self.tasks.get(id)
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// 更新状态。
    pub fn set_state(&mut self, id: &str, state: TaskState) -> Result<(), TaskGraphError> {
        let node = self
            .tasks
            .get_mut(id)
            .ok_or_else(|| TaskGraphError::UnknownTask(id.to_string()))?;
        node.state = state;
        Ok(())
    }

    /// 刷新阻塞状态:依赖全 Done 的 Blocked 任务 → Ready。
    ///
    /// 返回本次变为 Ready 的任务 id。
    pub fn refresh_ready(&mut self) -> Vec<String> {
        let done: BTreeSet<String> = self
            .tasks
            .values()
            .filter(|t| t.state.satisfies_dependency())
            .map(|t| t.id.clone())
            .collect();
        let mut became_ready = Vec::new();
        for node in self.tasks.values_mut() {
            if node.state != TaskState::Blocked {
                continue;
            }
            let all_done = node
                .blocked_by
                .iter()
                .all(|dep| done.contains(dep.as_str()));
            if all_done {
                node.state = TaskState::Ready;
                became_ready.push(node.id.clone());
            }
        }
        became_ready.sort();
        became_ready
    }

    /// 就绪队列(Ready 状态,按 id 排序)。
    pub fn ready_queue(&self) -> Vec<&TaskNode> {
        let mut out: Vec<&TaskNode> = self
            .tasks
            .values()
            .filter(|t| t.state == TaskState::Ready)
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// 拓扑排序(依赖在前)。有环时返回环。
    pub fn topo_order(&self) -> Result<Vec<&str>, TaskGraphError> {
        let mut indegree: BTreeMap<&str, usize> = BTreeMap::new();
        let mut adjacency: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for node in self.tasks.values() {
            indegree.entry(node.id.as_str()).or_insert(0);
            for dep in &node.blocked_by {
                if self.tasks.contains_key(dep) {
                    adjacency
                        .entry(dep.as_str())
                        .or_default()
                        .push(node.id.as_str());
                    *indegree.entry(node.id.as_str()).or_insert(0) += 1;
                }
            }
        }
        let mut queue: VecDeque<&str> = indegree
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(id, _)| *id)
            .collect();
        // 确定性:按 id 排序入队。
        let mut sorted: Vec<&str> = queue.into_iter().collect();
        sorted.sort_unstable();
        queue = sorted.into();

        let mut order = Vec::new();
        while let Some(id) = queue.pop_front() {
            order.push(id);
            if let Some(children) = adjacency.get(id) {
                let mut next: Vec<&str> = Vec::new();
                for child in children {
                    if let Some(d) = indegree.get_mut(child) {
                        *d -= 1;
                        if *d == 0 {
                            next.push(child);
                        }
                    }
                }
                next.sort_unstable();
                for n in next {
                    queue.push_back(n);
                }
            }
        }
        if order.len() != self.tasks.len() {
            return Err(TaskGraphError::Cycle(
                self.detect_cycle().unwrap_or_default(),
            ));
        }
        Ok(order)
    }

    /// 检测环(返回环上的任务 id)。
    pub fn detect_cycle(&self) -> Option<Vec<String>> {
        let mut color: BTreeMap<&str, u8> = BTreeMap::new();
        let mut stack: Vec<&str> = Vec::new();
        for id in self.tasks.keys() {
            if color.get(id.as_str()).copied().unwrap_or(0) == 0
                && let Some(cycle) = self.dfs_cycle(id, &mut color, &mut stack)
            {
                return Some(cycle);
            }
        }
        None
    }

    fn dfs_cycle<'a>(
        &'a self,
        id: &'a str,
        color: &mut BTreeMap<&'a str, u8>,
        stack: &mut Vec<&'a str>,
    ) -> Option<Vec<String>> {
        color.insert(id, 1);
        stack.push(id);
        if let Some(node) = self.tasks.get(id) {
            for dep in &node.blocked_by {
                let dep_str = dep.as_str();
                match color.get(dep_str).copied().unwrap_or(0) {
                    0 => {
                        if let Some(c) = self.dfs_cycle(dep_str, color, stack) {
                            return Some(c);
                        }
                    }
                    1 => {
                        let pos = stack.iter().position(|n| *n == dep_str).unwrap_or(0);
                        return Some(stack[pos..].iter().map(|n| (*n).to_string()).collect());
                    }
                    _ => {}
                }
            }
        }
        stack.pop();
        color.insert(id, 2);
        None
    }

    /// 自动推进:完成 `id` 后刷新就绪队列。
    pub fn complete_and_advance(&mut self, id: &str) -> Result<Vec<String>, TaskGraphError> {
        self.set_state(id, TaskState::Done)?;
        Ok(self.refresh_ready())
    }

    /// 全部完成。
    pub fn all_done(&self) -> bool {
        !self.tasks.is_empty() && self.tasks.values().all(|t| t.state == TaskState::Done)
    }

    /// 统计。
    pub fn counts(&self) -> BTreeMap<&'static str, usize> {
        let mut out = BTreeMap::new();
        for t in self.tasks.values() {
            *out.entry(t.state.as_str()).or_insert(0) += 1;
        }
        out
    }
}

/// 任务图错误。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TaskGraphError {
    #[error("task {0} depends on itself")]
    SelfDependency(String),
    #[error("task {task} depends on unknown task {dependency}")]
    UnknownDependency { task: String, dependency: String },
    #[error("unknown task: {0}")]
    UnknownTask(String),
    #[error("dependency cycle: {}", .0.join(" → "))]
    Cycle(Vec<String>),
}

/// 重复任务检测器。
#[derive(Debug, Default)]
pub struct DuplicateDetector {
    /// 归一化标题 → 任务 id。
    seen: BTreeMap<String, String>,
}

impl DuplicateDetector {
    pub fn new() -> DuplicateDetector {
        DuplicateDetector::default()
    }

    /// 归一化标题(小写 + 去空白 + 去标点)。
    pub fn normalize(title: &str) -> String {
        title
            .chars()
            .filter(|c| c.is_alphanumeric() || c.is_whitespace())
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }

    /// 检查是否重复。返回已存在的任务 id(若有)。
    pub fn check(&self, title: &str) -> Option<&str> {
        self.seen.get(&Self::normalize(title)).map(String::as_str)
    }

    /// 登记任务。返回 `true` = 新增,`false` = 重复(已合并)。
    pub fn register(&mut self, id: &str, title: &str) -> bool {
        let key = Self::normalize(title);
        if self.seen.contains_key(&key) {
            return false;
        }
        self.seen.insert(key, id.to_string());
        true
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

/// 项目容器:目标 + 规则 + 记忆打包。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectBundle {
    pub name: String,
    /// 项目目标。
    #[serde(default)]
    pub goals: Vec<String>,
    /// 项目规则(AGENTS.md 片段)。
    #[serde(default)]
    pub rules: Vec<String>,
    /// 项目记忆条目。
    #[serde(default)]
    pub memories: Vec<String>,
    /// 关联的工作区路径。
    #[serde(default)]
    pub workspace: Option<String>,
}

impl ProjectBundle {
    pub fn new(name: impl Into<String>) -> ProjectBundle {
        ProjectBundle {
            name: name.into(),
            goals: Vec::new(),
            rules: Vec::new(),
            memories: Vec::new(),
            workspace: None,
        }
    }

    pub fn with_goals(mut self, goals: Vec<String>) -> ProjectBundle {
        self.goals = goals;
        self
    }

    pub fn with_rules(mut self, rules: Vec<String>) -> ProjectBundle {
        self.rules = rules;
        self
    }

    pub fn with_memories(mut self, memories: Vec<String>) -> ProjectBundle {
        self.memories = memories;
        self
    }

    /// 是否空容器。
    pub fn is_empty(&self) -> bool {
        self.goals.is_empty() && self.rules.is_empty() && self.memories.is_empty()
    }

    /// 渲染成注入 system prompt 的段落。
    pub fn render_prompt_section(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut out = format!("## 项目:{}\n", self.name);
        if !self.goals.is_empty() {
            out.push_str("目标:\n");
            for g in &self.goals {
                out.push_str(&format!("- {g}\n"));
            }
        }
        if !self.rules.is_empty() {
            out.push_str("规则:\n");
            for r in &self.rules {
                out.push_str(&format!("- {r}\n"));
            }
        }
        if !self.memories.is_empty() {
            out.push_str("记忆:\n");
            for m in &self.memories {
                out.push_str(&format!("- {m}\n"));
            }
        }
        Some(out)
    }

    /// 序列化(便于保存 / 迁移)。
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    pub fn from_json(s: &str) -> Result<ProjectBundle, String> {
        serde_json::from_str(s).map_err(|e| format!("parse project bundle: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph_with_deps() -> TaskGraph {
        let mut g = TaskGraph::new();
        g.add(TaskNode::new("a", "task a")).unwrap();
        g.add(TaskNode::new("b", "task b").blocked_by(vec!["a".into()]))
            .unwrap();
        g.add(TaskNode::new("c", "task c").blocked_by(vec!["b".into()]))
            .unwrap();
        g
    }

    #[test]
    fn self_dependency_rejected() {
        let mut g = TaskGraph::new();
        let err = g
            .add(TaskNode::new("a", "a").blocked_by(vec!["a".into()]))
            .unwrap_err();
        assert_eq!(err, TaskGraphError::SelfDependency("a".into()));
    }

    #[test]
    fn validate_unknown_dependency() {
        let mut g = TaskGraph::new();
        g.add(TaskNode::new("a", "a").blocked_by(vec!["ghost".into()]))
            .unwrap();
        let err = g.validate().unwrap_err();
        assert!(matches!(err, TaskGraphError::UnknownDependency { .. }));
    }

    #[test]
    fn refresh_ready_unblocks_in_order() {
        let mut g = graph_with_deps();
        // 初始:全部 blocked(a 无依赖 → 应就绪)
        let ready = g.refresh_ready();
        assert_eq!(ready, vec!["a"], "只有无依赖的 a 就绪");
        assert_eq!(g.get("b").unwrap().state, TaskState::Blocked);
    }

    #[test]
    fn complete_and_advance_chain() {
        let mut g = graph_with_deps();
        g.refresh_ready();
        let next = g.complete_and_advance("a").unwrap();
        assert_eq!(next, vec!["b"], "完成 a 后 b 就绪");
        let next = g.complete_and_advance("b").unwrap();
        assert_eq!(next, vec!["c"], "完成 b 后 c 就绪");
        g.complete_and_advance("c").unwrap();
        assert!(g.all_done());
    }

    #[test]
    fn topo_order_respects_dependencies() {
        let g = graph_with_deps();
        let order = g.topo_order().expect("no cycle");
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    #[test]
    fn cycle_detected() {
        let mut g = TaskGraph::new();
        g.add(TaskNode::new("a", "a").blocked_by(vec!["b".into()]))
            .unwrap();
        g.add(TaskNode::new("b", "b").blocked_by(vec!["a".into()]))
            .unwrap();
        let cycle = g.detect_cycle().expect("应检测到环");
        assert!(cycle.contains(&"a".to_string()));
        assert!(cycle.contains(&"b".to_string()));
        assert!(matches!(g.topo_order(), Err(TaskGraphError::Cycle(_))));
    }

    #[test]
    fn no_cycle_in_dag() {
        let g = graph_with_deps();
        assert!(g.detect_cycle().is_none());
    }

    #[test]
    fn ready_queue_sorted() {
        let mut g = TaskGraph::new();
        g.add(TaskNode::new("z", "z")).unwrap();
        g.add(TaskNode::new("a", "a")).unwrap();
        g.refresh_ready();
        let ids: Vec<&str> = g.ready_queue().iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "z"]);
    }

    #[test]
    fn set_state_unknown_task_errors() {
        let mut g = TaskGraph::new();
        assert!(matches!(
            g.set_state("ghost", TaskState::Done),
            Err(TaskGraphError::UnknownTask(_))
        ));
    }

    #[test]
    fn counts_by_state() {
        let mut g = graph_with_deps();
        g.refresh_ready();
        let counts = g.counts();
        assert_eq!(counts["ready"], 1);
        assert_eq!(counts["blocked"], 2);
    }

    #[test]
    fn duplicate_detection_normalizes() {
        let mut d = DuplicateDetector::new();
        assert!(d.register("t1", "Fix the login bug"));
        // 大小写 / 标点 / 空白差异应视为同一任务。
        assert_eq!(d.check("fix  the login bug!"), Some("t1"));
        assert!(!d.register("t2", "FIX THE LOGIN BUG."), "重复应合并");
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn duplicate_detector_distinguishes_different() {
        let mut d = DuplicateDetector::new();
        d.register("t1", "fix login");
        assert!(d.check("fix logout").is_none());
        assert!(d.register("t2", "fix logout"));
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn project_bundle_renders() {
        let bundle = ProjectBundle::new("zhuye")
            .with_goals(vec!["ship v1".into()])
            .with_rules(vec!["run cargo clippy".into()])
            .with_memories(vec!["tests use nextest".into()]);
        let section = bundle.render_prompt_section().expect("section");
        assert!(section.contains("项目:zhuye"), "{section}");
        assert!(section.contains("ship v1"));
        assert!(section.contains("cargo clippy"));
        assert!(section.contains("nextest"));
    }

    #[test]
    fn empty_bundle_not_rendered() {
        let bundle = ProjectBundle::new("empty");
        assert!(bundle.is_empty());
        assert!(bundle.render_prompt_section().is_none());
    }

    #[test]
    fn project_bundle_json_roundtrip() {
        let bundle = ProjectBundle::new("p").with_goals(vec!["g".into()]);
        let json = bundle.to_json();
        let back = ProjectBundle::from_json(&json).expect("parse");
        assert_eq!(back, bundle);
    }

    #[test]
    fn task_node_serializes() {
        let node = TaskNode::new("a", "title").blocked_by(vec!["b".into()]);
        let json = serde_json::to_string(&node).expect("serialize");
        let back: TaskNode = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, node);
    }
}
