//! 串行工作流的节点契约和调度核心。

use async_trait::async_trait;
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::watch;
use uuid::Uuid;

/// 单次运行的总节点尝试上限,避免极大重试次数导致队列长期空转。
pub const MAX_NODE_ATTEMPTS: u32 = 10_000;

/// 节点及工作流状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Success,
    Failed,
    Skipped,
    Blocked,
    Cancelled,
}

/// 节点完成后的流转动作。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    #[default]
    Continue,
    Back {
        target: String,
    },
    Wait {
        reason: String,
    },
}

/// 节点调用输入。每次尝试都获得独立快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInput {
    pub node_id: String,
    pub node_type: String,
    pub attempt_id: Uuid,
    pub attempt: u32,
    pub config: Value,
    pub variables: Value,
    /// 已完成节点最近一次的输出,按节点实例 ID 索引。
    #[serde(default)]
    pub outputs: Map<String, Value>,
}

/// 节点返回的数据与明确声明的变量更新。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeResult {
    pub status: Status,
    pub output: Value,
    #[serde(default)]
    pub updates: Map<String, Value>,
    #[serde(default)]
    pub action: Action,
    pub error: Option<String>,
    pub agent_run_id: Option<String>,
    /// 由宿主校验后插入的节点实例。
    #[serde(default)]
    pub insertions: Vec<Insertion>,
}

/// 节点对后续队列提出的插入请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Insertion {
    pub id: String,
    pub node_type: String,
    pub after: String,
    #[serde(default)]
    pub config: Value,
}
impl NodeResult {
    pub fn success(output: Value) -> Self {
        Self {
            status: Status::Success,
            output,
            updates: Map::new(),
            action: Action::Continue,
            error: None,
            agent_run_id: None,
            insertions: Vec::new(),
        }
    }
    fn failure(error: String) -> Self {
        Self {
            status: Status::Failed,
            output: Value::Null,
            updates: Map::new(),
            action: Action::Continue,
            error: Some(error),
            agent_run_id: None,
            insertions: Vec::new(),
        }
    }
}

/// 基础节点；任务节点与 Agent 节点均实现此契约。
#[async_trait]
pub trait Node: Send + Sync {
    async fn execute(&self, input: NodeInput) -> Result<NodeResult, String>;
    /// 可提前分配的 Agent 运行标识,用于失败或取消时的追踪。
    fn agent_run_id(&self, _attempt_id: Uuid) -> Option<String> {
        None
    }
}

/// 队列中的节点实例。
pub struct NodeSpec {
    pub id: String,
    pub node_type: String,
    pub node: Arc<dyn Node>,
    pub config: Value,
    pub enabled: bool,
    pub skip_if: Option<SkipCondition>,
    pub max_runs: u32,
    pub timeout: Duration,
}
/// 按共享变量值判断是否跳过节点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkipCondition {
    pub variable: String,
    pub equals: Value,
}
impl NodeSpec {
    pub fn new(id: impl Into<String>, node: Arc<dyn Node>) -> Self {
        Self {
            id: id.into(),
            node_type: "native".into(),
            node,
            config: Value::Null,
            enabled: true,
            skip_if: None,
            max_runs: 1,
            timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Run,
    Pause,
    Cancel,
}
/// 可克隆的运行控制句柄。
#[derive(Clone)]
pub struct Control {
    tx: watch::Sender<Command>,
    state: watch::Sender<Snapshot>,
}
impl Default for Control {
    fn default() -> Self {
        let (tx, _) = watch::channel(Command::Run);
        let (state, _) = watch::channel(Snapshot::default());
        Self { tx, state }
    }
}
impl Control {
    /// 返回当前工作流快照,可在节点运行或等待时查询。
    pub fn snapshot(&self) -> Snapshot {
        self.state.borrow().clone()
    }
    fn publish(
        &self,
        state: RunState,
        current_node: Option<String>,
        reason: Option<String>,
        variables: &Value,
        history: &[Record],
        finally_history: &[Record],
    ) {
        self.state.send_replace(Snapshot {
            state,
            current_node,
            reason,
            variables: variables.clone(),
            history: history.to_vec(),
            finally_history: finally_history.to_vec(),
        });
    }
    pub fn pause(&self) {
        self.tx.send_if_modified(|v| {
            if *v == Command::Run {
                *v = Command::Pause;
                true
            } else {
                false
            }
        });
    }
    pub fn resume(&self) {
        self.tx.send_if_modified(|v| {
            if *v == Command::Pause {
                *v = Command::Run;
                true
            } else {
                false
            }
        });
    }
    pub fn cancel(&self) {
        self.tx.send_replace(Command::Cancel);
    }
}

/// 工作流运行生命周期。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Running,
    Paused,
    Blocked,
    Cleaning,
    Completed,
    Failed,
    Cancelled,
}

/// 进程内可查询的执行快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub state: RunState,
    pub current_node: Option<String>,
    pub reason: Option<String>,
    pub variables: Value,
    pub history: Vec<Record>,
    pub finally_history: Vec<Record>,
}
impl Default for Snapshot {
    fn default() -> Self {
        Self {
            state: RunState::Queued,
            current_node: None,
            reason: None,
            variables: Value::Null,
            history: Vec::new(),
            finally_history: Vec::new(),
        }
    }
}

/// 每次节点尝试的可观测记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub node_id: String,
    pub node_type: String,
    /// 记录实际流转动作,以便恢复后的历史保留阻塞原因。
    pub action: Action,
    pub attempt_id: Uuid,
    pub attempt: u32,
    /// 输入摘要只记录变量键,避免历史中重复保存敏感值。
    pub input_keys: Vec<String>,
    pub status: Status,
    pub output: Value,
    pub error: Option<String>,
    pub agent_run_id: Option<String>,
}
/// 主流程和清理流程的独立报告。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub status: Status,
    pub variables: Value,
    pub history: Vec<Record>,
    pub finally_history: Vec<Record>,
}

/// 队列和执行前的初始上下文。
pub struct Flow {
    queue: Vec<NodeSpec>,
    finally: Vec<NodeSpec>,
    variables: Value,
    ids: HashSet<String>,
    registry: HashMap<String, Arc<dyn Node>>,
}
impl Flow {
    pub fn new(variables: Value) -> Result<Self, String> {
        if !variables.is_object() {
            return Err("工作流变量必须为 JSON 对象".into());
        }
        Ok(Self {
            queue: Vec::new(),
            finally: Vec::new(),
            variables,
            ids: HashSet::new(),
            registry: HashMap::new(),
        })
    }
    /// 注册允许在执行期间插入的节点类型。
    pub fn register(&mut self, kind: impl Into<String>, node: Arc<dyn Node>) -> Result<(), String> {
        let kind = kind.into();
        if kind.is_empty() || self.registry.contains_key(&kind) {
            return Err("节点类型为空或重复".into());
        }
        self.registry.insert(kind, node);
        Ok(())
    }
    pub fn push(&mut self, node: NodeSpec) -> Result<(), String> {
        self.add(node, false)
    }
    pub fn push_finally(&mut self, node: NodeSpec) -> Result<(), String> {
        self.add(node, true)
    }
    fn add(&mut self, node: NodeSpec, cleanup: bool) -> Result<(), String> {
        if node.id.is_empty() || !self.ids.insert(node.id.clone()) {
            return Err("节点 ID 为空或重复".into());
        }
        if cleanup {
            self.finally.push(node);
        } else {
            self.queue.push(node);
        }
        Ok(())
    }
    pub async fn run(self, control: Control) -> Report {
        let Flow {
            mut queue,
            finally,
            mut ids,
            registry,
            mut variables,
        } = self;
        let mut history = Vec::new();
        let mut finally_history = Vec::new();
        let mut counts = HashMap::<String, u32>::new();
        let mut total_attempts = 0_u32;
        let mut outputs = Map::new();
        let mut rx = control.tx.subscribe();
        let mut status = Status::Success;
        let mut index = 0;
        control.publish(
            RunState::Running,
            None,
            None,
            &variables,
            &history,
            &finally_history,
        );
        while index < queue.len() {
            while *rx.borrow_and_update() == Command::Pause {
                control.publish(
                    RunState::Paused,
                    None,
                    None,
                    &variables,
                    &history,
                    &finally_history,
                );
                if rx.changed().await.is_err() {
                    break;
                }
            }
            if *rx.borrow() == Command::Cancel {
                status = Status::Cancelled;
                break;
            }
            let node = &queue[index];
            if should_skip(node, &variables) {
                let mut result = NodeResult::success(Value::Null);
                result.status = Status::Skipped;
                history.push(record(node, Uuid::new_v4(), 0, &variables, &result));
                index += 1;
                continue;
            }
            let count = counts.entry(node.id.clone()).or_default();
            if *count >= node.max_runs {
                let result = NodeResult::failure("节点执行次数已达上限".into());
                history.push(record(node, Uuid::new_v4(), *count, &variables, &result));
                status = Status::Failed;
                break;
            }
            if total_attempts >= MAX_NODE_ATTEMPTS {
                // 多节点互相回退时,仅看单节点次数会允许极长的空转。
                let result = NodeResult::failure("工作流节点执行总次数已达上限".into());
                history.push(record(node, Uuid::new_v4(), *count, &variables, &result));
                status = Status::Failed;
                break;
            }
            *count += 1;
            total_attempts += 1;
            let attempt = *count;
            let id = Uuid::new_v4();
            control.publish(
                RunState::Running,
                Some(node.id.clone()),
                None,
                &variables,
                &history,
                &finally_history,
            );
            let input = NodeInput {
                node_id: node.id.clone(),
                node_type: node.node_type.clone(),
                attempt_id: id,
                attempt,
                config: node.config.clone(),
                variables: variables.clone(),
                outputs: outputs.clone(),
            };
            let mut result = tokio::select! {
                biased;
                _ = wait_cancel(&mut rx) => {
                    let mut cancelled = NodeResult::failure("节点已取消".into());
                    cancelled.status = Status::Cancelled;
                    history.push(record(node, id, attempt, &variables, &cancelled));
                    status = Status::Cancelled;
                    break;
                }
                result = execute_node(node, input) => result
            };
            // 持有控制命令写锁提交结果,使取消与提交有唯一先后顺序;此前只读一次
            // 取消状态,取消可能在检查后到达,迟到结果仍会改写共享变量。
            let mut committed = false;
            control.tx.send_if_modified(|command| {
                if *command == Command::Cancel {
                    return false;
                }
                if result.status == Status::Success || result.status == Status::Blocked {
                    if let Action::Back { target } = &result.action {
                        if !queue.iter().take(index + 1).any(|node| &node.id == target) {
                            result = NodeResult::failure(format!("回退目标无效: {target}"));
                        }
                    }
                }
                if result.status == Status::Success || result.status == Status::Blocked {
                    if let Err(error) =
                        insert_nodes(&mut queue, &mut ids, &registry, index, &result.insertions)
                    {
                        result = NodeResult::failure(error);
                    }
                }
                let node = &queue[index];
                history.push(record(node, id, attempt, &variables, &result));
                if result.status == Status::Success || result.status == Status::Blocked {
                    outputs.insert(node.id.clone(), result.output.clone());
                    if let Some(map) = variables.as_object_mut() {
                        map.extend(result.updates.clone());
                    }
                }
                committed = true;
                false
            });
            if !committed {
                let mut cancelled = NodeResult::failure("节点已取消".into());
                cancelled.status = Status::Cancelled;
                history.push(record(&queue[index], id, attempt, &variables, &cancelled));
                status = Status::Cancelled;
                break;
            }
            let node = &queue[index];
            if (result.status == Status::Success || result.status == Status::Blocked)
                && (result.status == Status::Blocked
                    || matches!(result.action, Action::Wait { .. }))
            {
                control.pause();
                let reason = match &result.action {
                    Action::Wait { reason } => Some(reason.clone()),
                    _ => result.error.clone(),
                };
                control.publish(
                    RunState::Blocked,
                    Some(node.id.clone()),
                    reason,
                    &variables,
                    &history,
                    &finally_history,
                );
                while *rx.borrow_and_update() == Command::Pause {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
                if *rx.borrow() == Command::Cancel {
                    status = Status::Cancelled;
                    break;
                }
                index += 1;
                continue;
            }
            if result.status == Status::Skipped {
                index += 1;
                continue;
            }
            if result.status != Status::Success {
                status = result.status;
                break;
            }
            match result.action {
                Action::Continue => index += 1,
                Action::Back { target } => match queue.iter().position(|n| n.id == target) {
                    Some(i) if i <= index => index = i,
                    _ => {
                        // 配置错误不能留下看似成功的节点记录。
                        if let Some(last) = history.last_mut() {
                            last.status = Status::Failed;
                            last.error = Some(format!("回退目标无效: {target}"));
                        }
                        status = Status::Failed;
                        break;
                    }
                },
                Action::Wait { .. } => unreachable!("等待动作已在前面处理"),
            }
            // 纯内存节点可能始终立即完成,主动让出执行器以处理取消。
            tokio::task::yield_now().await;
        }
        control.publish(
            RunState::Cleaning,
            None,
            None,
            &variables,
            &history,
            &finally_history,
        );
        for node in &finally {
            if should_skip(node, &variables) {
                let mut skipped = NodeResult::success(Value::Null);
                skipped.status = Status::Skipped;
                finally_history.push(record(node, Uuid::new_v4(), 0, &variables, &skipped));
                continue;
            }
            let id = Uuid::new_v4();
            let input = NodeInput {
                node_id: node.id.clone(),
                node_type: node.node_type.clone(),
                attempt_id: id,
                attempt: 1,
                config: node.config.clone(),
                variables: variables.clone(),
                outputs: outputs.clone(),
            };
            let result = execute_node(node, input).await;
            finally_history.push(record(node, id, 1, &variables, &result));
            if result.status == Status::Success {
                outputs.insert(node.id.clone(), result.output.clone());
                if let Some(map) = variables.as_object_mut() {
                    map.extend(result.updates);
                }
            }
        }
        let terminal = match status {
            Status::Success | Status::Skipped => RunState::Completed,
            Status::Cancelled => RunState::Cancelled,
            Status::Failed | Status::Blocked => RunState::Failed,
        };
        control.publish(terminal, None, None, &variables, &history, &finally_history);
        Report {
            status,
            variables,
            history,
            finally_history,
        }
    }
}
// 先在影子队列完成全部校验,避免后续请求非法时留下部分插入。
fn insert_nodes(
    queue: &mut Vec<NodeSpec>,
    ids: &mut HashSet<String>,
    registry: &HashMap<String, Arc<dyn Node>>,
    current: usize,
    requests: &[Insertion],
) -> Result<(), String> {
    if queue.len().saturating_add(requests.len()) > 256 {
        return Err("工作流节点数量超过上限 256".into());
    }
    let mut names: Vec<String> = queue.iter().map(|node| node.id.clone()).collect();
    let mut seen = ids.clone();
    let mut after_last = HashMap::<String, String>::new();
    let mut positions = Vec::with_capacity(requests.len());
    for request in requests {
        if request.id.is_empty() || !seen.insert(request.id.clone()) {
            return Err(format!("插入节点 ID 为空或重复: {}", request.id));
        }
        if !registry.contains_key(&request.node_type) {
            return Err(format!("未注册的节点类型: {}", request.node_type));
        }
        let anchor = after_last.get(&request.after).unwrap_or(&request.after);
        let position = names
            .iter()
            .position(|name| name == anchor)
            .ok_or_else(|| format!("插入目标不存在: {}", request.after))?;
        if position < current {
            return Err("不能在已执行节点之后插入".into());
        }
        names.insert(position + 1, request.id.clone());
        positions.push(position + 1);
        after_last.insert(request.after.clone(), request.id.clone());
    }
    for (request, position) in requests.iter().zip(positions) {
        let mut node = NodeSpec::new(&request.id, Arc::clone(&registry[&request.node_type]));
        node.node_type = request.node_type.clone();
        node.config = request.config.clone();
        queue.insert(position, node);
        ids.insert(request.id.clone());
    }
    Ok(())
}

// Rust 节点 panic 不能丢掉工作流终态和清理队列,需要在单个节点边界捕获。
async fn execute_node(node: &NodeSpec, input: NodeInput) -> NodeResult {
    match tokio::time::timeout(
        node.timeout,
        std::panic::AssertUnwindSafe(node.node.execute(input)).catch_unwind(),
    )
    .await
    {
        Ok(Ok(Ok(result))) => result,
        Ok(Ok(Err(error))) => NodeResult::failure(error),
        Ok(Err(_)) => NodeResult::failure("节点执行发生 panic".into()),
        Err(_) => NodeResult::failure("节点执行超时".into()),
    }
}

fn should_skip(node: &NodeSpec, variables: &Value) -> bool {
    !node.enabled
        || node
            .skip_if
            .as_ref()
            .is_some_and(|condition| variables.get(&condition.variable) == Some(&condition.equals))
}

fn record(
    node: &NodeSpec,
    attempt_id: Uuid,
    attempt: u32,
    variables: &Value,
    result: &NodeResult,
) -> Record {
    Record {
        node_id: node.id.clone(),
        node_type: node.node_type.clone(),
        action: result.action.clone(),
        attempt_id,
        attempt,
        input_keys: variables
            .as_object()
            .map_or_else(Vec::new, |map| map.keys().cloned().collect()),
        status: result.status,
        output: result.output.clone(),
        error: result.error.clone(),
        agent_run_id: if attempt > 0 {
            node.node.agent_run_id(attempt_id)
        } else {
            None
        }
        .or_else(|| result.agent_run_id.clone()),
    }
}
async fn wait_cancel(rx: &mut watch::Receiver<Command>) {
    loop {
        if *rx.borrow_and_update() == Command::Cancel {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// WASM 任务节点,组件输出必须符合工作流节点结果契约。
pub struct WasmTaskNode(pub zhuye_wasm::WasmWorkflowNode);

#[async_trait]
impl Node for WasmTaskNode {
    async fn execute(&self, input: NodeInput) -> Result<NodeResult, String> {
        let input = serde_json::to_string(&input).map_err(|e| e.to_string())?;
        let output = self.0.invoke(input).await.map_err(|e| e.to_string())?;
        serde_json::from_str(&output).map_err(|e| format!("节点结果非法: {e}"))
    }
}

/// Agent 执行返回的会话标识和业务结果文本。
#[derive(Debug, Clone)]
pub struct AgentReply {
    pub run_id: String,
    pub text: String,
}

/// Agent 调用能力由运行时装配,工作流核心不反向依赖运行时。
#[async_trait]
pub trait AgentBackend: Send + Sync {
    async fn run(&self, prompt: String, run_id: Uuid) -> Result<AgentReply, String>;
}

/// 基于烛夜 Agent 执行能力的节点。
pub struct AgentNode {
    pub backend: Arc<dyn AgentBackend>,
    pub instructions: String,
    /// 仅这些变量会交给 Agent。
    pub context_keys: Vec<String>,
    /// 仅这些上游节点输出会交给 Agent。
    pub output_keys: Vec<String>,
}

#[async_trait]
impl Node for AgentNode {
    fn agent_run_id(&self, attempt_id: Uuid) -> Option<String> {
        Some(attempt_id.to_string())
    }
    async fn execute(&self, input: NodeInput) -> Result<NodeResult, String> {
        let context: Map<String, Value> = self
            .context_keys
            .iter()
            .filter_map(|key| {
                input
                    .variables
                    .get(key)
                    .cloned()
                    .map(|value| (key.clone(), value))
            })
            .collect();
        let outputs: Map<String, Value> = self
            .output_keys
            .iter()
            .filter_map(|key| {
                input
                    .outputs
                    .get(key)
                    .cloned()
                    .map(|value| (key.clone(), value))
            })
            .collect();
        let prompt = format!(
            "{}\n\n输入数据(JSON):\n{}\n\n必须仅返回一个 JSON 对象,不要返回说明文字。格式示例: {{\"status\":\"success\",\"output\":{{\"result\":\"你的结果\"}}}}。status 必须是 success、failed、skipped、blocked、cancelled 之一;成功时使用 success。output 必须存在,updates、action、error 可选。",
            self.instructions,
            serde_json::json!({"variables": context, "outputs": outputs, "config": input.config})
        );
        let reply = self.backend.run(prompt, input.attempt_id).await?;
        if reply.run_id != input.attempt_id.to_string() {
            return Err("Agent 返回了不匹配的运行 ID".into());
        }
        // 会话结束本身不能证明业务完成,必须解析显式状态。
        // 部分模型即使被要求只返回 JSON 仍会加代码围栏;只剥离完整围栏,
        // 不从自由文本中猜测业务状态,避免把不完整回复误判为成功。
        let text = reply.text.trim();
        let text = text
            .strip_prefix("```json\n")
            .or_else(|| text.strip_prefix("```\n"))
            .and_then(|body| body.strip_suffix("```"))
            .map_or(text, str::trim);
        let mut value: Value =
            serde_json::from_str(text).map_err(|error| format!("Agent 节点结果非法: {error}"))?;
        // 模型常将成功状态写成 ok;它是明确状态值,可安全归一化为调度器契约。
        if value.get("status").and_then(Value::as_str) == Some("ok") {
            value["status"] = Value::String("success".into());
        }
        let mut result: NodeResult = serde_json::from_value(value)
            .map_err(|error| format!("Agent 节点结果非法: {error}"))?;
        result.agent_run_id = Some(reply.run_id);
        Ok(result)
    }
}
