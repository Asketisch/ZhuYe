//! 工作流定义解析与进程内执行管理。

use crate::agent::{ApprovalHub, ZhuyeAgentBackend};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;
use zhuye_workflow::{
    AgentNode, Control, Flow, Node, NodeInput, NodeResult, NodeSpec, RunState, SkipCondition,
    Snapshot, WasmTaskNode,
};

fn default_variables() -> Value {
    json!({})
}
fn default_true() -> bool {
    true
}
fn default_max_runs() -> u32 {
    1
}
fn default_timeout_ms() -> u64 {
    30_000
}

/// 工作流定义只在进程内执行,不会在启动时修改外部状态。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    #[serde(default = "default_variables")]
    pub variables: Value,
    pub node_types: HashMap<String, NodeType>,
    pub nodes: Vec<NodeDefinition>,
    #[serde(default)]
    pub finally_nodes: Vec<NodeDefinition>,
}

impl Definition {
    /// 从 TOML 文件加载定义,相对路径以配置文件所在目录为基准。
    pub async fn from_toml_file(path: &Path) -> Result<Self, String> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|e| format!("无法确定当前目录: {e}"))?
                .join(path)
        };
        let source = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| format!("读取工作流配置失败: {e}"))?;
        let mut definition: Self =
            toml::from_str(&source).map_err(|e| format!("工作流 TOML 配置无效: {e}"))?;
        let base = path.parent().ok_or("工作流配置路径没有父目录")?;
        for node in definition.node_types.values_mut() {
            let path = match node {
                NodeType::Wasm { path } => path,
                NodeType::Agent { workspace, .. } => workspace,
                NodeType::Task => continue,
            };
            if path.is_relative() {
                *path = base.join(&*path);
            }
        }
        Ok(definition)
    }
}

/// 节点类型声明与队列实例分离,动态插入只能引用预先声明的类型。
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NodeType {
    Task,
    Wasm {
        path: PathBuf,
    },
    Agent {
        model: String,
        workspace: PathBuf,
        instructions: String,
        #[serde(default)]
        context_keys: Vec<String>,
        #[serde(default)]
        output_keys: Vec<String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeDefinition {
    pub id: String,
    pub node_type: String,
    #[serde(default)]
    pub config: Value,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_max_runs")]
    pub max_runs: u32,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub skip_if: Option<SkipCondition>,
}

/// 内置纯数据任务;配置即节点结果,可用于数据整形和控制流。
struct DataTask;
#[async_trait]
impl Node for DataTask {
    async fn execute(&self, input: NodeInput) -> Result<NodeResult, String> {
        serde_json::from_value(input.config).map_err(|e| format!("任务配置不是有效的节点结果: {e}"))
    }
}

/// 每个服务进程维护自己的运行表;状态不会跨进程恢复。
#[derive(Default)]
pub struct Manager {
    runs: Mutex<HashMap<Uuid, RunEntry>>,
}
#[derive(Clone)]
struct RunEntry {
    control: Control,
    approvals: Arc<ApprovalHub>,
}
impl Manager {
    pub async fn start(&self, definition: Definition) -> Result<Uuid, String> {
        if definition.nodes.is_empty() {
            return Err("工作流至少需要一个主节点".into());
        }
        if definition.nodes.len() + definition.finally_nodes.len() > 256
            || definition.node_types.len() > 256
        {
            return Err("工作流节点或类型超过上限 256".into());
        }
        let approvals = Arc::new(ApprovalHub::default());
        let mut registry: HashMap<String, Arc<dyn Node>> = HashMap::new();
        for (name, kind) in definition.node_types {
            if name.is_empty() {
                return Err("节点类型名不能为空".into());
            }
            let node: Arc<dyn Node> = match kind {
                NodeType::Task => Arc::new(DataTask),
                NodeType::Wasm { path } => {
                    let wasm = tokio::task::spawn_blocking(move || {
                        zhuye_wasm::WasmWorkflowNode::from_file(&path)
                    })
                    .await
                    .map_err(|e| format!("组件加载任务失败: {e}"))?
                    .map_err(|e| format!("组件加载失败: {e}"))?;
                    Arc::new(WasmTaskNode(wasm))
                }
                NodeType::Agent {
                    model,
                    workspace,
                    instructions,
                    context_keys,
                    output_keys,
                } => {
                    if model.is_empty() || instructions.is_empty() {
                        return Err("Agent 模型和指令不能为空".into());
                    }
                    Arc::new(AgentNode {
                        backend: Arc::new(ZhuyeAgentBackend {
                            model,
                            workspace,
                            approvals: Arc::clone(&approvals),
                        }),
                        instructions,
                        context_keys,
                        output_keys,
                    })
                }
            };
            registry.insert(name, node);
        }
        let mut flow = Flow::new(definition.variables)?;
        for (name, node) in &registry {
            flow.register(name.clone(), Arc::clone(node))?;
        }
        for node in definition.nodes {
            flow.push(build_node(node, &registry)?)?;
        }
        for node in definition.finally_nodes {
            flow.push_finally(build_node(node, &registry)?)?;
        }
        let control = Control::default();
        let id = Uuid::new_v4();
        {
            let mut runs = self.runs.lock().map_err(|_| "运行表不可用")?;
            if runs.len() >= 128 {
                return Err("工作流运行表已满,请先清理已结束的运行".into());
            }
            runs.insert(
                id,
                RunEntry {
                    control: control.clone(),
                    approvals,
                },
            );
        }
        tokio::spawn(async move {
            flow.run(control).await;
        });
        Ok(id)
    }

    fn control(&self, id: Uuid) -> Result<Control, String> {
        self.runs
            .lock()
            .map_err(|_| "运行表不可用")?
            .get(&id)
            .map(|r| r.control.clone())
            .ok_or_else(|| "工作流运行不存在".into())
    }
    pub fn status(&self, id: Uuid) -> Result<Snapshot, String> {
        Ok(self.control(id)?.snapshot())
    }
    pub fn pause(&self, id: Uuid) -> Result<Snapshot, String> {
        let c = self.control(id)?;
        c.pause();
        Ok(c.snapshot())
    }
    pub fn resume(&self, id: Uuid) -> Result<Snapshot, String> {
        let c = self.control(id)?;
        c.resume();
        Ok(c.snapshot())
    }
    pub fn cancel(&self, id: Uuid) -> Result<Snapshot, String> {
        let c = self.control(id)?;
        c.cancel();
        Ok(c.snapshot())
    }
    pub fn approvals(&self, id: Uuid) -> Result<Vec<zhuye::ApprovalRequestEvent>, String> {
        let hub = self
            .runs
            .lock()
            .map_err(|_| "运行表不可用")?
            .get(&id)
            .map(|r| Arc::clone(&r.approvals))
            .ok_or("工作流运行不存在")?;
        hub.list()
    }
    pub fn approve(
        &self,
        id: Uuid,
        request_id: &str,
        decision: zhuye::ReviewDecision,
    ) -> Result<(), String> {
        let hub = self
            .runs
            .lock()
            .map_err(|_| "运行表不可用")?
            .get(&id)
            .map(|r| Arc::clone(&r.approvals))
            .ok_or("工作流运行不存在")?;
        hub.respond(request_id, decision)
    }
    pub fn forget(&self, id: Uuid) -> Result<(), String> {
        let mut runs = self.runs.lock().map_err(|_| "运行表不可用")?;
        let run = runs.get(&id).ok_or("工作流运行不存在")?;
        if !matches!(
            run.control.snapshot().state,
            RunState::Completed | RunState::Failed | RunState::Cancelled
        ) {
            return Err("只能清理已结束的运行".into());
        }
        runs.remove(&id);
        Ok(())
    }
}

fn build_node(
    def: NodeDefinition,
    registry: &HashMap<String, Arc<dyn Node>>,
) -> Result<NodeSpec, String> {
    let node = registry
        .get(&def.node_type)
        .ok_or_else(|| format!("未知节点类型: {}", def.node_type))?;
    if def.max_runs == 0 || def.timeout_ms == 0 {
        return Err("节点执行次数和超时必须大于零".into());
    }
    let mut spec = NodeSpec::new(def.id, Arc::clone(node));
    spec.node_type = def.node_type;
    spec.config = def.config;
    spec.enabled = def.enabled;
    spec.max_runs = def.max_runs;
    spec.timeout = Duration::from_millis(def.timeout_ms);
    spec.skip_if = def.skip_if;
    Ok(spec)
}

#[cfg(test)]
mod toml_tests {
    use super::*;

    #[tokio::test]
    async fn 配置中的节点路径相对文件目录解析() {
        let dir = std::env::temp_dir().join(format!("zhuye-workflow-{}", Uuid::new_v4()));
        tokio::fs::create_dir(&dir).await.unwrap();
        let file = dir.join("workflow.toml");
        tokio::fs::write(
            &file,
            r#"
[node_types.component]
kind = "wasm"
path = "node.wasm"
[node_types.assistant]
kind = "agent"
model = "mock"
workspace = "workspace"
instructions = "处理输入"
[[nodes]]
id = "first"
node_type = "assistant"
"#,
        )
        .await
        .unwrap();
        let definition = Definition::from_toml_file(&file).await.unwrap();
        assert!(
            matches!(&definition.node_types["component"], NodeType::Wasm { path } if path == &dir.join("node.wasm"))
        );
        assert!(
            matches!(&definition.node_types["assistant"], NodeType::Agent { workspace, .. } if workspace == &dir.join("workspace"))
        );
        tokio::fs::remove_dir_all(dir).await.unwrap();
    }
}
