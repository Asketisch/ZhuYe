//! # 动态工作流控制流(F32)
//!
//! 把流水线从「写死 DAG」升级为**带条件与循环的图**:
//!
//! - **条件边**([`EdgeCondition`])—— 按上游输出决定走哪条分支;
//! - **循环**([`LoopSpec`])—— 固定次数 / 条件驱动 / forEach 集合遍历;
//! - **并行竞速**([`ParallelSpec`])—— 多个分支同时跑,取第一个成功的;
//! - **节点级策略**([`NodePolicy`])—— retry / timeout / 缓存 / 错误处理。
//!
//! ## 为什么「环检测」在这里更重要
//!
//! DAG 时代环是错误;有了循环,环是**特性**。所以必须区分:
//!
//! - **受控环**(`LoopSpec` 显式声明)—— 合法,但有最大迭代上限;
//! - **失控环**(普通边形成的环)—— 仍然是错误。
//!
//! [`validate_flow`] 做这个区分。没有它,一个手写错的边就会让流水线
//! 无限转。
//!
//! ## 与既有 DAG 的关系
//!
//! [`crate::graph::DiGraph`] 仍负责拓扑排序与基础环检测。本模块在其上
//! 增加**控制流语义**,不改动既有 DAG 行为 —— 旧流水线继续按老语义跑。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// 边条件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EdgeCondition {
    /// 无条件(总是走)。
    Always,
    /// 上游节点成功才走。
    OnSuccess,
    /// 上游节点失败才走(错误处理分支)。
    OnFailure,
    /// 上游输出包含某子串才走。
    WhenContains { text: String },
    /// 上游输出匹配某简单条件(数值比较)。
    WhenGreaterThan { field: String, value: f64 },
}

impl EdgeCondition {
    /// 该条件是否依赖上游**成功**。
    pub fn requires_success(&self) -> bool {
        matches!(
            self,
            EdgeCondition::Always
                | EdgeCondition::OnSuccess
                | EdgeCondition::WhenContains { .. }
                | EdgeCondition::WhenGreaterThan { .. }
        )
    }
}

/// 一条带条件的边。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlowEdge {
    pub from: String,
    pub to: String,
    #[serde(default = "default_condition")]
    pub condition: EdgeCondition,
}

fn default_condition() -> EdgeCondition {
    EdgeCondition::Always
}

impl FlowEdge {
    pub fn new(from: impl Into<String>, to: impl Into<String>) -> FlowEdge {
        FlowEdge {
            from: from.into(),
            to: to.into(),
            condition: EdgeCondition::Always,
        }
    }

    pub fn when(mut self, condition: EdgeCondition) -> FlowEdge {
        self.condition = condition;
        self
    }
}

/// 循环规格。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoopSpec {
    /// 固定次数。
    Times { count: u32 },
    /// 条件驱动(最多 `max` 次,防无限)。
    While { max: u32 },
    /// 遍历集合(每个元素跑一次)。
    ForEach { items: Vec<String> },
}

impl LoopSpec {
    /// 该循环最多跑多少次(用于预算与校验)。
    pub fn max_iterations(&self) -> u32 {
        match self {
            LoopSpec::Times { count } => *count,
            LoopSpec::While { max } => *max,
            LoopSpec::ForEach { items } => items.len() as u32,
        }
    }

    /// 是否有界(所有 spec 都有界 —— `While` 也要求 max)。
    pub fn is_bounded(&self) -> bool {
        self.max_iterations() > 0 || self.max_iterations() == 0
    }
}

/// 并行竞速规格。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParallelSpec {
    /// 参与竞速的分支节点名。
    pub branches: Vec<String>,
    /// 是否需要全部成功(`false` = 取第一个成功的)。
    #[serde(default)]
    pub require_all: bool,
}

impl ParallelSpec {
    /// 分支数。
    pub fn width(&self) -> usize {
        self.branches.len()
    }
}

/// 节点级策略。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodePolicy {
    /// 最大重试次数(0 = 不重试)。
    #[serde(default)]
    pub max_retries: u32,
    /// 超时(秒;`None` = 不限)。
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// 是否缓存结果(相同输入复用)。
    #[serde(default)]
    pub cacheable: bool,
    /// 失败时的处理:继续 / 中止 / 走错误分支。
    #[serde(default)]
    pub on_error: NodeErrorPolicy,
}

impl Default for NodePolicy {
    fn default() -> Self {
        NodePolicy {
            max_retries: 0,
            timeout_secs: None,
            cacheable: false,
            on_error: NodeErrorPolicy::Abort,
        }
    }
}

/// 节点失败策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeErrorPolicy {
    /// 中止整个流水线(默认)。
    #[default]
    Abort,
    /// 继续后续节点(该节点记为失败)。
    Continue,
    /// 走 `OnFailure` 边。
    ErrorBranch,
}

/// 一个流程节点。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlowNode {
    pub name: String,
    /// 节点类型(`agent` / `shell` / `review` / `suspend` / …)。
    pub kind: String,
    #[serde(default)]
    pub policy: NodePolicy,
    /// 该节点是循环体时,循环规格。
    #[serde(default)]
    pub loop_spec: Option<LoopSpec>,
}

impl FlowNode {
    pub fn new(name: impl Into<String>, kind: impl Into<String>) -> FlowNode {
        FlowNode {
            name: name.into(),
            kind: kind.into(),
            policy: NodePolicy::default(),
            loop_spec: None,
        }
    }

    pub fn with_policy(mut self, policy: NodePolicy) -> FlowNode {
        self.policy = policy;
        self
    }

    pub fn with_loop(mut self, spec: LoopSpec) -> FlowNode {
        self.loop_spec = Some(spec);
        self
    }
}

/// 工作流定义(声明式 spec)。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FlowSpec {
    #[serde(default)]
    pub nodes: Vec<FlowNode>,
    #[serde(default)]
    pub edges: Vec<FlowEdge>,
    /// 并行竞速组。
    #[serde(default)]
    pub parallel: Vec<ParallelSpec>,
    /// 入口节点。
    #[serde(default)]
    pub entry: Option<String>,
}

impl FlowSpec {
    pub fn new() -> FlowSpec {
        FlowSpec::default()
    }

    pub fn add_node(mut self, node: FlowNode) -> FlowSpec {
        self.nodes.push(node);
        self
    }

    pub fn add_edge(mut self, edge: FlowEdge) -> FlowSpec {
        self.edges.push(edge);
        self
    }

    /// 节点名集合。
    pub fn node_names(&self) -> BTreeSet<&str> {
        self.nodes.iter().map(|n| n.name.as_str()).collect()
    }

    /// 校验流程。
    pub fn validate(&self) -> Result<(), FlowError> {
        validate_flow(self)
    }
}

/// 流程错误。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FlowError {
    #[error("empty workflow: no nodes")]
    Empty,
    #[error("duplicate node name: {0}")]
    DuplicateNode(String),
    #[error("edge references unknown node: {0}")]
    UnknownNode(String),
    #[error("entry node not found: {0}")]
    UnknownEntry(String),
    #[error("uncontrolled cycle detected (use LoopSpec for intended loops): {0}")]
    UncontrolledCycle(String),
    #[error("parallel group references unknown branch: {0}")]
    UnknownBranch(String),
    #[error("parallel group must have at least 2 branches")]
    TooFewBranches,
}

/// 校验流程:节点唯一 / 边端点存在 / 入口存在 / 无失控环。
pub fn validate_flow(spec: &FlowSpec) -> Result<(), FlowError> {
    if spec.nodes.is_empty() {
        return Err(FlowError::Empty);
    }
    let mut seen = BTreeSet::new();
    for node in &spec.nodes {
        if !seen.insert(node.name.as_str()) {
            return Err(FlowError::DuplicateNode(node.name.clone()));
        }
    }
    let names = spec.node_names();
    for edge in &spec.edges {
        if !names.contains(edge.from.as_str()) {
            return Err(FlowError::UnknownNode(edge.from.clone()));
        }
        if !names.contains(edge.to.as_str()) {
            return Err(FlowError::UnknownNode(edge.to.clone()));
        }
    }
    if let Some(entry) = &spec.entry
        && !names.contains(entry.as_str())
    {
        return Err(FlowError::UnknownEntry(entry.clone()));
    }
    for group in &spec.parallel {
        if group.branches.len() < 2 {
            return Err(FlowError::TooFewBranches);
        }
        for branch in &group.branches {
            if !names.contains(branch.as_str()) {
                return Err(FlowError::UnknownBranch(branch.clone()));
            }
        }
    }
    // 失控环检测:只有**循环体节点**可以形成回边。
    detect_uncontrolled_cycle(spec)?;
    Ok(())
}

/// 检测失控环。
///
/// 规则:普通边形成的环是错误;若环中至少有一个节点声明了 `loop_spec`
/// (循环体),则该环受控。
fn detect_uncontrolled_cycle(spec: &FlowSpec) -> Result<(), FlowError> {
    let mut adjacency: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for edge in &spec.edges {
        adjacency
            .entry(edge.from.as_str())
            .or_default()
            .push(edge.to.as_str());
    }
    let loop_nodes: BTreeSet<&str> = spec
        .nodes
        .iter()
        .filter(|n| n.loop_spec.is_some())
        .map(|n| n.name.as_str())
        .collect();

    // DFS 三色标记,记录当前路径;发现回边时检查路径上是否有循环体。
    let mut color: BTreeMap<&str, u8> = BTreeMap::new(); // 0=未访 1=在栈 2=完成
    for node in spec.nodes.iter().map(|n| n.name.as_str()) {
        if color.get(node).copied().unwrap_or(0) == 0
            && let Some(path) = dfs_cycle(node, &adjacency, &mut color, &mut Vec::new())
        {
            // 环 = 回边起点到终点之间的路径。
            let controlled = path.iter().any(|n| loop_nodes.contains(n.as_str()));
            if !controlled {
                return Err(FlowError::UncontrolledCycle(path.join(" → ")));
            }
        }
    }
    Ok(())
}

fn dfs_cycle<'a>(
    node: &'a str,
    adjacency: &BTreeMap<&'a str, Vec<&'a str>>,
    color: &mut BTreeMap<&'a str, u8>,
    stack: &mut Vec<&'a str>,
) -> Option<Vec<String>> {
    color.insert(node, 1);
    stack.push(node);
    if let Some(neighbors) = adjacency.get(node) {
        for next in neighbors {
            match color.get(next).copied().unwrap_or(0) {
                0 => {
                    if let Some(path) = dfs_cycle(next, adjacency, color, stack) {
                        return Some(path);
                    }
                }
                1 => {
                    // 回边:取出栈中从 next 开始的部分作为环。
                    let pos = stack.iter().position(|n| n == next).unwrap_or(0);
                    let cycle: Vec<String> =
                        stack[pos..].iter().map(|n| (*n).to_string()).collect();
                    return Some(cycle);
                }
                _ => {}
            }
        }
    }
    stack.pop();
    color.insert(node, 2);
    None
}

/// 计算总迭代预算(所有循环体的最大迭代之和 × 节点数)。
///
/// 用于在跑之前估算最坏情况成本,防止一个 `Times { count: 10000 }`
/// 把预算吃光。
pub fn worst_case_iterations(spec: &FlowSpec) -> u64 {
    let mut total: u64 = 0;
    for node in &spec.nodes {
        let iterations = node
            .loop_spec
            .as_ref()
            .map_or(1, |s| u64::from(s.max_iterations()));
        total = total.saturating_add(iterations);
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple() -> FlowSpec {
        FlowSpec::new()
            .add_node(FlowNode::new("a", "agent"))
            .add_node(FlowNode::new("b", "agent"))
            .add_edge(FlowEdge::new("a", "b"))
            .entry("a")
    }

    trait WithEntry {
        fn entry(self, name: &str) -> Self;
    }
    impl WithEntry for FlowSpec {
        fn entry(mut self, name: &str) -> Self {
            self.entry = Some(name.to_string());
            self
        }
    }

    #[test]
    fn valid_linear_flow() {
        assert!(simple().validate().is_ok());
    }

    #[test]
    fn empty_flow_rejected() {
        assert_eq!(FlowSpec::new().validate(), Err(FlowError::Empty));
    }

    #[test]
    fn duplicate_node_rejected() {
        let spec = FlowSpec::new()
            .add_node(FlowNode::new("a", "agent"))
            .add_node(FlowNode::new("a", "shell"));
        assert_eq!(spec.validate(), Err(FlowError::DuplicateNode("a".into())));
    }

    #[test]
    fn unknown_edge_endpoint_rejected() {
        let spec = FlowSpec::new()
            .add_node(FlowNode::new("a", "agent"))
            .add_edge(FlowEdge::new("a", "ghost"));
        assert_eq!(spec.validate(), Err(FlowError::UnknownNode("ghost".into())));
    }

    #[test]
    fn unknown_entry_rejected() {
        let mut spec = simple();
        spec.entry = Some("ghost".into());
        assert_eq!(
            spec.validate(),
            Err(FlowError::UnknownEntry("ghost".into()))
        );
    }

    #[test]
    fn plain_cycle_is_uncontrolled() {
        let spec = FlowSpec::new()
            .add_node(FlowNode::new("a", "agent"))
            .add_node(FlowNode::new("b", "agent"))
            .add_edge(FlowEdge::new("a", "b"))
            .add_edge(FlowEdge::new("b", "a"));
        let err = spec.validate().unwrap_err();
        assert!(
            matches!(err, FlowError::UncontrolledCycle(_)),
            "普通环应被拒:{err:?}"
        );
    }

    #[test]
    fn loop_body_cycle_is_controlled() {
        let spec = FlowSpec::new()
            .add_node(FlowNode::new("a", "agent"))
            .add_node(FlowNode::new("body", "agent").with_loop(LoopSpec::Times { count: 3 }))
            .add_edge(FlowEdge::new("a", "body"))
            .add_edge(FlowEdge::new("body", "body"));
        assert!(spec.validate().is_ok(), "循环体上的回边是受控环");
    }

    #[test]
    fn parallel_group_validation() {
        let spec = FlowSpec::new()
            .add_node(FlowNode::new("a", "agent"))
            .add_node(FlowNode::new("b", "agent"))
            .add_node(FlowNode::new("c", "agent"))
            .add_edge(FlowEdge::new("a", "b"))
            .add_edge(FlowEdge::new("a", "c"));
        let mut with_parallel = spec.clone();
        with_parallel.parallel.push(ParallelSpec {
            branches: vec!["b".into(), "c".into()],
            require_all: false,
        });
        assert!(with_parallel.validate().is_ok());

        let mut too_few = spec;
        too_few.parallel.push(ParallelSpec {
            branches: vec!["b".into()],
            require_all: false,
        });
        assert_eq!(too_few.validate(), Err(FlowError::TooFewBranches));
    }

    #[test]
    fn parallel_unknown_branch_rejected() {
        let mut spec = simple();
        spec.parallel.push(ParallelSpec {
            branches: vec!["a".into(), "ghost".into()],
            require_all: false,
        });
        assert_eq!(
            spec.validate(),
            Err(FlowError::UnknownBranch("ghost".into()))
        );
    }

    #[test]
    fn edge_conditions() {
        assert!(EdgeCondition::OnSuccess.requires_success());
        assert!(!EdgeCondition::OnFailure.requires_success());
        assert!(EdgeCondition::Always.requires_success());
        assert!(EdgeCondition::WhenContains { text: "x".into() }.requires_success());
    }

    #[test]
    fn loop_spec_iterations() {
        assert_eq!(LoopSpec::Times { count: 5 }.max_iterations(), 5);
        assert_eq!(LoopSpec::While { max: 100 }.max_iterations(), 100);
        assert_eq!(
            LoopSpec::ForEach {
                items: vec!["a".into(), "b".into(), "c".into()]
            }
            .max_iterations(),
            3
        );
    }

    #[test]
    fn worst_case_iterations_sums() {
        let spec = FlowSpec::new()
            .add_node(FlowNode::new("a", "agent"))
            .add_node(FlowNode::new("loop", "agent").with_loop(LoopSpec::Times { count: 10 }));
        assert_eq!(worst_case_iterations(&spec), 11, "1 (a) + 10 (loop)");
    }

    #[test]
    fn node_error_policy_defaults_to_abort() {
        let node = FlowNode::new("x", "agent");
        assert_eq!(node.policy.on_error, NodeErrorPolicy::Abort);
        assert_eq!(node.policy.max_retries, 0);
    }

    #[test]
    fn flow_spec_serializes() {
        let spec = simple();
        let json = serde_json::to_string(&spec).expect("serialize");
        let back: FlowSpec = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, spec);
    }

    #[test]
    fn loop_spec_serializes_with_tag() {
        let json = serde_json::to_string(&LoopSpec::Times { count: 3 }).expect("serialize");
        assert!(json.contains("\"kind\":\"times\""), "{json}");
    }
}
