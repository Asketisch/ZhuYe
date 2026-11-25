//! 将工作流 Agent 节点接入烛夜门面,并桥接工具与 hook 审批。

use async_trait::async_trait;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::oneshot;
use zhuye::{
    ApprovalKind, ApprovalRequestEvent, EventMsg, Op, ReviewDecision, Submission, TurnStatus, Zhuye,
};
use zhuye_workflow::{AgentBackend, AgentReply};

struct Pending {
    request: ApprovalRequestEvent,
    reply: oneshot::Sender<ReviewDecision>,
}

/// 一个工作流独有的审批表。
#[derive(Default)]
pub struct ApprovalHub {
    pending: Mutex<HashMap<String, Pending>>,
}
impl ApprovalHub {
    pub fn list(&self) -> Result<Vec<ApprovalRequestEvent>, String> {
        let pending = self.pending.lock().map_err(|_| "审批表不可用")?;
        Ok(pending.values().map(|p| p.request.clone()).collect())
    }
    pub fn respond(&self, id: &str, decision: ReviewDecision) -> Result<(), String> {
        let pending = self
            .pending
            .lock()
            .map_err(|_| "审批表不可用")?
            .remove(id)
            .ok_or("审批请求不存在或已处理")?;
        pending
            .reply
            .send(decision)
            .map_err(|_| "审批请求已经结束".into())
    }
    async fn wait(
        self: &Arc<Self>,
        request: ApprovalRequestEvent,
    ) -> Result<ReviewDecision, String> {
        let id = request.request_id.clone();
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().map_err(|_| "审批表不可用")?;
            if pending.contains_key(&id) {
                return Err("重复的审批请求 ID".into());
            }
            pending.insert(id.clone(), Pending { request, reply: tx });
        }
        // 取消或超时会丢弃等待 future,同步移除对应待审批项。
        let _guard = PendingGuard {
            hub: Arc::clone(self),
            id,
        };
        rx.await.map_err(|_| "审批请求已取消".into())
    }
}
struct PendingGuard {
    hub: Arc<ApprovalHub>,
    id: String,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.hub.pending.lock() {
            pending.remove(&self.id);
        }
    }
}

// 与 headless 一致组合配置、项目和用户权限规则,避免嵌入门面时规则失效。
async fn permissions(
    workspace: &std::path::Path,
) -> Result<Arc<dyn zhuye_permissions::PermissionResolver>, String> {
    use zhuye_permissions::{
        ChainedPermissionStore, FilePermissionStore, InMemoryPermissionStore, PermissionStore,
        StorePermissionResolver,
    };
    let config = zhuye_config::load_default();
    let config_store: Arc<dyn PermissionStore> = Arc::new(InMemoryPermissionStore::new());
    if let Some(section) = config.permissions {
        for rule in section.expanded_rules() {
            config_store
                .add(rule)
                .await
                .map_err(|e| format!("权限规则加载失败: {e}"))?;
        }
    }
    let project_store: Arc<dyn PermissionStore> = Arc::new(FilePermissionStore::with_path(
        workspace.join(".zhuye/permissions.toml"),
    ));
    let user_store: Arc<dyn PermissionStore> = match FilePermissionStore::with_default_home() {
        Ok(store) => Arc::new(store),
        Err(_) => Arc::new(InMemoryPermissionStore::new()),
    };
    let store: Arc<dyn PermissionStore> = Arc::new(ChainedPermissionStore::new(vec![
        config_store,
        project_store,
        user_store,
    ]));
    Ok(Arc::new(StorePermissionResolver::new(store)))
}

/// 每次执行使用独立烛夜会话,避免不同工作流共享上下文。
pub struct ZhuyeAgentBackend {
    pub model: String,
    pub workspace: PathBuf,
    pub approvals: Arc<ApprovalHub>,
}

// 调度器取消节点时会丢弃 future;令牌必须随之取消,否则后台 turn 仍会运行。
struct CancelOnDrop(zhuye::Zhuye);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel_token().cancel();
    }
}

#[async_trait]
impl AgentBackend for ZhuyeAgentBackend {
    async fn run(&self, prompt: String, attempt_id: uuid::Uuid) -> Result<AgentReply, String> {
        let resolver = permissions(&self.workspace).await?;
        let mut builder = Zhuye::builder(&self.model)
            .workspace(&self.workspace)
            .approvals(true)
            .with_permission_resolver(resolver);
        let config = zhuye_config::load_default();
        let group = self
            .model
            .split_once('/')
            .map_or(self.model.as_str(), |(group, _)| group);
        if config
            .model_group
            .iter()
            .any(|entry| entry.id.trim() == group)
        {
            // 工作流此前只读取厂商密钥环境变量,忽略配置中的自定义端点。
            // 复用配置注册表,让工作流与 CLI 连接同一个模型组和端点。
            let registry = config
                .to_registry()
                .map_err(|e| format!("模型组配置错误: {e}"))?;
            if !registry.list().iter().any(|name| name == group) {
                return Err(format!("模型组不可用: {group}"));
            }
            builder = builder.with_model_registry(Arc::new(registry));
        }
        let agent = builder
            .build()
            .map_err(|e| format!("Agent 初始化失败: {e}"))?;
        let agent = CancelOnDrop(agent);
        // 使用工作流尝试 ID 作为 submission ID,取消或失败时仍可关联回合。
        let run_id = attempt_id.to_string();
        let submission = Submission::with_id(run_id.clone(), Op::user_input_text(prompt));
        let mut stream = agent.0.submit(submission).await;
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            match event.msg {
                EventMsg::AgentMessageDelta(delta) => text.push_str(&delta.delta),
                EventMsg::AgentMessage(message) => text = message.text,
                EventMsg::AgentMessageReset(_) => text.clear(),
                EventMsg::TurnComplete(done) if done.status == TurnStatus::Success => {
                    return Ok(AgentReply { run_id, text });
                }
                EventMsg::TurnComplete(done) => {
                    return Err(format!("Agent 未正常结束: {:?}", done.status));
                }
                EventMsg::TurnAborted(abort) => {
                    return Err(format!("Agent 已中止: {:?}", abort.reason));
                }
                EventMsg::Error(error) => return Err(format!("Agent 运行错误: {error:?}")),
                EventMsg::ApprovalRequest(request) => {
                    let kind = request.kind.clone();
                    let id = request.request_id.clone();
                    if !matches!(kind, ApprovalKind::Tool { .. } | ApprovalKind::Hook { .. }) {
                        return Err("当前工作流服务不支持此审批类型".into());
                    }
                    let decision = self.approvals.wait(request).await?;
                    let op = match kind {
                        ApprovalKind::Tool { .. } => Op::ToolApproval { id, decision },
                        ApprovalKind::Hook { .. } => Op::HookApproval { id, decision },
                        _ => unreachable!("审批类型已校验"),
                    };
                    let _response = agent
                        .0
                        .submit(Submission::with_id(uuid::Uuid::new_v4().to_string(), op))
                        .await;
                }
                EventMsg::AskUserInput(_) | EventMsg::AskUserQuestion(_) => {
                    return Err("Agent 需要用户输入,当前工作流服务不支持该交互".into());
                }
                _ => {}
            }
        }
        Err("Agent 事件流在终态前关闭".into())
    }
}
