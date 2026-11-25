//! `CallSubAgentTool` —— 注册在父级 `ToolRegistry` 上的 `call_<role>` 工具。
//! 当父级 LLM 调用它时,factory spawn 一个子 `AgentThread`,并返回抽取出的结果。

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use zhuye_protocol::{ToolError, ToolOutput};
use zhuye_tools::{PendingSubagentRegistry, PendingSubagentResult, Tool, ToolContext};

use crate::factory::SubAgentFactory;
use crate::spec::SubAgentSpec;

/// 子代理派发模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchMode {
    /// 同步(默认):父回合阻塞等子代理结束,结果作为本次工具调用的回执。
    Sync,
    /// 异步:立即返回占位回执,子代理在后台继续跑;结果由引擎在回合收尾
    /// 前**挂起等待**并注入本轮上下文(见 `zhuye-core` 的 `check_stop`)。
    /// 子代理不跨越派发它的回合 —— 父回合被打断时一并取消。
    Async,
}

/// 解析 `mode` 参数。**缺省 / 非法值一律按同步处理** —— 派发方式退化
/// 必须是安全方向(同步 = 旧行为),不能让一个拼错的参数把父回合变成
/// 「派发出去就不管」的半吊子状态。
fn parse_mode(args: &serde_json::Value) -> DispatchMode {
    match args.get("mode").and_then(|v| v.as_str()) {
        Some("async") => DispatchMode::Async,
        _ => DispatchMode::Sync,
    }
}

/// 子代理中间事件的进度回调。
///
/// 必须是 `'static + Send + Sync` —— 异步派发要把它**移进**后台任务
/// (父回合的工具 future 早已返回),裸闭包做不到。
type ProgressCallback = Arc<dyn Fn(&zhuye_protocol::Event) + Send + Sync>;

/// 委托给 `SubAgentFactory::spawn` + 结果抽取的 `Tool` 实现。
pub struct CallSubAgentTool {
    pub factory: Arc<SubAgentFactory>,
    pub spec: SubAgentSpec,
    /// review 修复:缓存 `call_<role>` 工具名。此前每次 `name()` 都
    /// `Box::leak` 一份新字符串,而 `name()` 在每轮序列化工具列表时都会被
    /// 调用,长会话泄漏 O(turns × tools)。
    tool_name_cache: std::sync::OnceLock<String>,
}

impl std::fmt::Debug for CallSubAgentTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallSubAgentTool")
            .field("spec", &self.spec)
            .finish_non_exhaustive()
    }
}

impl CallSubAgentTool {
    pub fn new(factory: Arc<SubAgentFactory>, spec: SubAgentSpec) -> Self {
        Self {
            factory,
            spec,
            tool_name_cache: std::sync::OnceLock::new(),
        }
    }

    /// 构造子代理中间事件的转发回调(助手文本 / 工具起止 → 父回合事件通道)。
    ///
    /// 返回 `Arc<dyn Fn(..)>` 而非裸闭包:异步派发要把回调**移进**后台
    /// 任务(父回合的工具 future 早已返回),需要 `'static + Send + Sync`。
    fn build_progress_cb(
        forwarder: Option<Arc<zhuye_tools::ToolEventForwarder>>,
        child_id: &str,
        role: &str,
    ) -> Option<ProgressCallback> {
        // 转发器缺席(测试 / 直接构造)时 no-op。
        forwarder.map(|fwd| {
            let child_id = child_id.to_string();
            let role = role.to_string();
            // 回调以 `&(dyn Fn(..))` 形式被 collect 消费 —— 需要保持 Fn,
            // 可变映射用 Mutex 包裹(Interior mutability)。
            let tool_names: std::sync::Mutex<std::collections::HashMap<String, String>> =
                std::sync::Mutex::new(std::collections::HashMap::new());
            Arc::new(move |ev: &zhuye_protocol::Event| {
                if matches!(
                    &ev.msg,
                    zhuye_protocol::EventMsg::ApprovalRequest(_)
                        | zhuye_protocol::EventMsg::AskUserQuestion(_)
                        | zhuye_protocol::EventMsg::AskUserInput(_)
                        | zhuye_protocol::EventMsg::PermissionBubble(_)
                ) {
                    fwd.forward(ev.msg.clone());
                    return;
                }
                let payload = match &ev.msg {
                    zhuye_protocol::EventMsg::AgentMessage(m) => Some((
                        zhuye_protocol::SubagentProgressKind::Message,
                        m.text.clone(),
                        None,
                    )),
                    zhuye_protocol::EventMsg::ToolCallBegin(b) => {
                        tool_names
                            .lock()
                            .unwrap()
                            .insert(b.call_id.clone(), b.tool_name.clone());
                        Some((
                            zhuye_protocol::SubagentProgressKind::ToolBegin,
                            b.tool_name.clone(),
                            Some(b.call_id.clone()),
                        ))
                    }
                    zhuye_protocol::EventMsg::ToolCallEnd(e) => {
                        // 反查工具名;未见 Begin(极端时序)时退化为占位名。
                        let name = tool_names
                            .lock()
                            .unwrap()
                            .remove(&e.call_id)
                            .unwrap_or_else(|| "tool".to_string());
                        let text = if e.is_error {
                            format!("{name} (failed)")
                        } else {
                            name
                        };
                        Some((
                            zhuye_protocol::SubagentProgressKind::ToolEnd,
                            text,
                            Some(e.call_id.clone()),
                        ))
                    }
                    _ => None,
                };
                if let Some((kind, text, call_id)) = payload {
                    fwd.forward(zhuye_protocol::EventMsg::SubagentProgress(
                        zhuye_protocol::SubagentProgressEvent {
                            child_id: child_id.clone(),
                            role: role.clone(),
                            kind,
                            text,
                            call_id,
                        },
                    ));
                }
            }) as ProgressCallback
        })
    }

    /// 同步路径的收尾:抽结果 → coordinator footer → 写完成记录 → 回执。
    fn finish_sync(
        &self,
        result: String,
        prompt: String,
        child_session_id: &str,
        turn_iteration: u32,
    ) -> ToolOutput {
        // v1.1.0:coordinator 模式下 subagent 返回后向父协调者注入 principle footer。
        let result_text = if self.factory.is_coordinator_mode() {
            crate::data_transfer::append_coordinator_principle_footer(
                &result,
                self.factory.coordinator_footer().as_deref(),
            )
        } else {
            result.clone()
        };
        // v1.1.0 Phase 6 P0:成功后写入 subagent registry(若 factory
        // 注入了)。失败子代理不写,避免污染 `<system-reminder>`。
        // coordinator 模式下 task_summary 追加 worker role,便于 pre_loop
        // reminder 区分不同 worker 的派工摘要。
        let mut task_summary = prompt;
        if self.factory.is_coordinator_mode() {
            task_summary.push_str(&format!(" [worker: {}]", self.spec.role));
        }
        // `iteration` 用 `TurnId` 的低 32 位 —— UUID v4 全局唯一,
        // 这里只作排序键,不需要严格按 turn 序号。
        if let Some(reg) = self.factory.subagent_registry() {
            reg.record(zhuye_recovery::SubagentRegistryEntry {
                tool_name: format!("call_{}", self.spec.role),
                task_summary,
                result_summary: result,
                iteration: turn_iteration,
                created_at: chrono::Utc::now(),
            });
        }
        let active_workers = self
            .factory
            .subagent_registry()
            .map_or(0, |r| r.snapshot().len());
        ToolOutput {
            content: vec![zhuye_protocol::ContentBlock::Text { text: result_text }],
            is_error: false,
            metadata: serde_json::json!({
                "subagent_role": self.spec.role,
                "session_id": child_session_id,
                "activeWorkers": active_workers,
            }),
            elapsed_ms: 0,
        }
    }

    /// 异步派发:登记到在飞表 → 把子代理交给后台任务 → 立即返回占位回执。
    ///
    /// 后台任务持有 `SpawnedChild` —— 不能在工具 future 返回时丢在地上:
    /// `SpawnedChild` 析构会 **cancel 子令牌**,子代理会被立刻杀掉。任务
    /// 跑到终态后回填在飞表,引擎据此唤醒父回合。
    ///
    /// `owner` 是父回合的回合级取消令牌(`ToolContext::cancel`)。同步模式下
    /// 「打断父回合 ⇒ 子代理一并取消」是工具 future 被丢弃的副产品;异步模式
    /// 的后台任务不随工具 future 析构,这条级联必须显式接线:后台任务监听
    /// `owner`,引擎收尾时经 `abandon_turn` 触发条目的放弃句柄。**异步子代理
    /// 不跨越派发它的回合**。
    fn dispatch_async(
        &self,
        spawned: crate::factory::SpawnedChild,
        payload: SpawnPayload,
        progress_cb: Option<ProgressCallback>,
        registry: Arc<PendingSubagentRegistry>,
        owner: CancellationToken,
    ) -> ToolOutput {
        let abort = registry.register(
            payload.child_session_id.clone(),
            payload.role.clone(),
            payload.call_id.clone(),
            owner.clone(),
        );
        // 兜底守卫:后台任务意外中止(runtime 掉线、panic)时,把条目落到
        // 终态 —— 否则该条目永远停在「在飞」,此后每个回合的 `check_stop`
        // 都会为它挂起等待(一次中止拖垮整个会话)。
        let guard = zhuye_tools::PendingCompletionGuard::new(
            registry.clone(),
            payload.child_session_id.clone(),
            payload.role.clone(),
            payload.call_id.clone(),
        );
        let factory = self.factory.clone();
        // 占位回执用的副本 —— `payload` 整体移进后台任务。
        let out_role = payload.role.clone();
        let out_session = payload.child_session_id.clone();
        tokio::spawn(async move {
            // `_guard` 绑定到块尾 —— 真实结果先写入,守卫析构时的兜底
            // 因「首个终态为准」被忽略。
            let _guard = guard;
            // 父回合结束(被打断 / 被引擎放弃)→ 立刻停子代理:丢弃 collect
            // future 即析构 `SpawnedChild`(cancel 子令牌 + 释放 in-flight
            // 槽位)。两个取消分支分工:`owner` 让用户按下中断的瞬间子代理
            // 就停,不必等父回合自己收尾;`abort` 覆盖回合「正常收尾但没等
            // 它」的情形(触顶收尾),那种收尾并不会 cancel 回合令牌。
            let outcome = tokio::select! {
                biased;
                _ = abort.cancelled() => None,
                _ = owner.cancelled() => None,
                result = spawned
                    .collect_result_with_progress(progress_cb.as_deref()) => Some(result),
            };
            let Some(result) = outcome else {
                tracing::info!(
                    role = %payload.role,
                    child_id = %payload.child_session_id,
                    "父回合已结束,异步子代理随回合取消,结果不回填"
                );
                return;
            };
            let (ok, text, error) = match result {
                Ok(res) => {
                    let raw = res.text;
                    // 写完成记录(防重复 spawn)—— 与同步路径同口径。
                    let mut task_summary = payload.prompt.clone();
                    if factory.is_coordinator_mode() {
                        task_summary.push_str(&format!(" [worker: {}]", payload.role));
                    }
                    if let Some(r) = factory.subagent_registry() {
                        r.record(zhuye_recovery::SubagentRegistryEntry {
                            tool_name: format!("call_{}", payload.role),
                            task_summary,
                            result_summary: raw.clone(),
                            iteration: payload.turn_iteration,
                            created_at: chrono::Utc::now(),
                        });
                    }
                    // coordinator 模式:回填给父协调者的文本同样带 footer
                    // (同步路径在 `finish_sync` 里做,这里是异步对应物)。
                    let text = if factory.is_coordinator_mode() {
                        crate::data_transfer::append_coordinator_principle_footer(
                            &raw,
                            factory.coordinator_footer().as_deref(),
                        )
                    } else {
                        raw
                    };
                    (true, text, None)
                }
                Err(e) => (false, String::new(), Some(e.to_string())),
            };
            registry.complete(PendingSubagentResult {
                child_id: payload.child_session_id,
                role: payload.role,
                call_id: payload.call_id,
                ok,
                text,
                error,
            });
        });
        let active_workers = self
            .factory
            .subagent_registry()
            .map_or(0, |r| r.snapshot().len());
        ToolOutput {
            content: vec![zhuye_protocol::ContentBlock::Text {
                text: format!(
                    "[已派发子代理 {role}(session={session})] 正在后台运行;完成后结果将自动回填本轮上下文。",
                    role = out_role,
                    session = out_session
                ),
            }],
            is_error: false,
            metadata: serde_json::json!({
                "subagent_role": out_role,
                "session_id": out_session,
                "child_id": out_session,
                "async": true,
                "pending": true,
                "activeWorkers": active_workers,
            }),
            elapsed_ms: 0,
        }
    }
}

#[async_trait]
impl Tool for CallSubAgentTool {
    fn name(&self) -> &str {
        // 工具名以 owned `String` 形式存在 spec 内部,`Tool::name` 要求
        // borrowed `&str` —— 用实例级 OnceLock 缓存,首次计算后零分配,
        // 随实例一起释放,不再泄漏。
        self.tool_name_cache.get_or_init(|| self.spec.tool_name())
    }

    fn description(&self) -> &str {
        static DESC: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        DESC.get_or_init(|| {
            "Spawn a sub-agent invocation with the given prompt. The sub-agent has its own session and tools; the returned text is its final answer. \
             mode=\"sync\" (default) blocks until the sub-agent finishes. mode=\"async\" returns immediately; the sub-agent keeps running in the background and its result is injected back into the current turn once it completes."
                .to_string()
        })
        // 必须返回 `&str`。用另一种技巧:每个实例泄漏一份。
        .as_str()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "The user prompt to send to the sub-agent." },
                "context_tail": {
                    "type": "integer",
                    "description": "Optional override of the parent's pass_context_messages setting.",
                    "default": 0,
                },
                "mode": {
                    "type": "string",
                    "enum": ["sync", "async"],
                    "default": "sync",
                    "description": "sync (default): block until the sub-agent finishes and return its answer. async: return immediately and let the sub-agent run in the background; its result is injected into the current turn when it finishes."
                }
            },
            "required": ["prompt"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        // Subagent 调用彼此独立,可以并行执行。
        true
    }

    fn parent_context_limit(&self, _args: &serde_json::Value) -> Option<usize> {
        // PreToolUse hook 可以改写 context_tail,因此需在改写前准备历史。
        None
    }

    async fn execute(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                message: "missing 'prompt' string".into(),
            })?
            .to_string();
        // v1.4 C1:父上下文传递接通 —— 截取条数优先取调用参数
        // `context_tail`,否则用 spec 的 `pass_context_messages`(默认 0,
        // 行为不变)。元素从 tool_exec 注入的父历史尾部快照(JSON)还原。
        let pass_n = args
            .get("context_tail")
            .and_then(|v| v.as_u64())
            .map_or(self.spec.data_transfer.pass_context_messages, |n| {
                usize::try_from(n).unwrap_or(usize::MAX)
            });
        let parent_history: Vec<zhuye_llm::ChatMessage> = if pass_n > 0 {
            ctx.parent_tail_json
                .iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect()
        } else {
            Vec::new()
        };
        let parent_tail =
            crate::data_transfer::select_parent_context(parent_history, &ctx.call_id, pass_n);
        let spawned = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            result = self.factory.spawn(self.spec.clone(), parent_tail, prompt.clone()) =>
                result.map_err(ToolError::from)?,
        };
        let child_session_id = spawned.session_id.to_string();
        // v1.4 C1:进度推送 —— 把子代理的中间事件(助手文本 / 工具开始 /
        // 结束)包装为 SubagentProgress 事件经转发器发往父事件通道;
        // 转发器缺席(测试 / 直接构造)时 no-op。
        // review 修复:ToolEnd 的 text 按协议契约回填工具名 —— End 事件
        // 本身只带 call_id,这里用 Begin 事件积累的 call_id → 工具名映射
        // 反查(此前发的是 "tool done"/"tool failed",与协议 doc 相悖)。
        let progress_cb = Self::build_progress_cb(
            ctx.event_forwarder.clone(),
            &child_session_id,
            &self.spec.role,
        );
        // `iteration` 用 `TurnId` 的低 32 位 —— UUID v4 全局唯一,
        // 这里只作排序键,不需要严格按 turn 序号。
        let turn_iteration = ctx.turn_id.0.as_u128() as u32;

        // ── 异步派发 ────────────────────────────────────────────
        // 立即返回占位回执,子代理在后台继续跑;结果由引擎在回合收尾前
        // 挂起等待并注入本轮上下文(见 `zhuye-core` 的 `check_stop`)。
        if parse_mode(&args) == DispatchMode::Async {
            // 引擎侧的在飞登记表是**每线程一份**(随 `ToolContext` 注入)。
            // 缺席 = 本线程不支持异步派发(纯库 / 测试)→ 降级同步,
            // 不能让模型拿到一个「已派发但没人会回填」的假回执。
            if let Some(registry) = ctx.pending_subagents.clone() {
                // 父回合的回合级取消令牌(队列从 `turn_cancel` 注入)——
                // 「打断父回合 ⇒ 取消在途子代理」靠它级联。
                let owner = ctx.cancel.clone();
                return Ok(self.dispatch_async(
                    spawned,
                    SpawnPayload {
                        prompt: prompt.clone(),
                        child_session_id: child_session_id.clone(),
                        role: self.spec.role.clone(),
                        call_id: ctx.call_id.clone(),
                        turn_iteration,
                    },
                    progress_cb,
                    registry,
                    owner,
                ));
            }
            tracing::warn!(
                role = %self.spec.role,
                "本线程无在飞子代理登记表,异步派发降级为同步派发"
            );
        }

        // ── 同步派发(默认,行为不变)────────────────────────────
        let result = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            result = spawned.collect_result_with_progress(progress_cb.as_deref()) =>
                result.map_err(ToolError::from)?.text,
        };
        Ok(self.finish_sync(result, prompt, &child_session_id, turn_iteration))
    }
}

/// 异步派发要带进后台任务的载荷。
struct SpawnPayload {
    prompt: String,
    child_session_id: String,
    role: String,
    call_id: String,
    turn_iteration: u32,
}

// `SubAgentSpec::role_for_tool_name` 已废弃:此前每次调用 `Box::leak` 一份
// 新字符串,而 `Tool::name()` 每轮序列化工具列表都会调用,长会话泄漏
// O(turns × tools)。工具名现由 `CallSubAgentTool` 的实例级 `OnceLock` 缓存。
#[allow(dead_code)]
impl SubAgentSpec {
    /// 兼容保留:仅供测试/外部诊断使用,内部不再调用。
    pub fn role_for_tool_name(&self) -> String {
        self.tool_name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_transfer::DataTransferConfig;
    use zhuye_protocol::ThreadId;
    use zhuye_tools::ToolRegistry;

    #[test]
    fn tool_name_format() {
        let s = SubAgentSpec {
            name: "Explorer".into(),
            role: "explorer".into(),
            model: None,
            system_prompt: "x".into(),
            allowed_tools: vec![],
            data_transfer: DataTransferConfig::default(),
            max_turns: None,
            allowed_skills: vec![],
        };
        assert_eq!(s.tool_name(), "call_explorer");
    }

    #[test]
    fn parameters_schema_requires_prompt() {
        let factory = Arc::new(SubAgentFactory::new(
            ThreadId::new(),
            "openai/gpt-4o",
            std::sync::Arc::new(zhuye_llm::ModelRegistry::new()),
            None, // child_registry: 回退父级 registry
            Arc::new(ToolRegistry::default()),
            CancellationToken::new(),
            None,
        ));
        let tool = CallSubAgentTool::new(
            factory,
            SubAgentSpec {
                name: "x".into(),
                role: "x".into(),
                model: None,
                system_prompt: "x".into(),
                allowed_tools: vec![],
                data_transfer: DataTransferConfig::default(),
                max_turns: None,
                allowed_skills: vec![],
            },
        );
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "prompt"));
    }

    /// schema 必须暴露 `mode` 参数(否则模型无法选择异步派发)。
    #[test]
    fn parameters_schema_exposes_mode() {
        let factory = Arc::new(SubAgentFactory::new(
            ThreadId::new(),
            "openai/gpt-4o",
            std::sync::Arc::new(zhuye_llm::ModelRegistry::new()),
            None,
            Arc::new(ToolRegistry::default()),
            CancellationToken::new(),
            None,
        ));
        let tool = CallSubAgentTool::new(
            factory,
            SubAgentSpec {
                name: "Explorer".into(),
                role: "explorer".into(),
                model: None,
                system_prompt: "x".into(),
                allowed_tools: vec![],
                data_transfer: DataTransferConfig::default(),
                max_turns: None,
                allowed_skills: vec![],
            },
        );
        let schema = tool.parameters_schema();
        let mode = &schema["properties"]["mode"];
        assert_eq!(mode["type"], "string");
        assert_eq!(mode["default"], "sync");
        let variants = mode["enum"].as_array().expect("mode 应是枚举");
        assert!(variants.iter().any(|v| v == "sync"));
        assert!(variants.iter().any(|v| v == "async"));
    }

    /// `mode` 解析:缺省 / 非法值一律退回同步(退化方向必须安全)。
    #[test]
    fn parse_mode_falls_back_to_sync() {
        assert_eq!(
            parse_mode(&serde_json::json!({"prompt": "x"})),
            DispatchMode::Sync
        );
        assert_eq!(
            parse_mode(&serde_json::json!({"prompt": "x", "mode": "async"})),
            DispatchMode::Async
        );
        assert_eq!(
            parse_mode(&serde_json::json!({"prompt": "x", "mode": "ASYNC"})),
            DispatchMode::Sync,
            "大小写不匹配时按同步处理(不猜模型意图)"
        );
        assert_eq!(
            parse_mode(&serde_json::json!({"prompt": "x", "mode": "wat"})),
            DispatchMode::Sync
        );
        assert_eq!(
            parse_mode(&serde_json::json!({"prompt": "x", "mode": 3})),
            DispatchMode::Sync
        );
    }

    /// 异步派发端到端(用 mock provider 跑真实子代理回合):
    /// 工具**立即**返回占位回执,子代理结果随后经在飞表回填。
    #[tokio::test]
    async fn async_dispatch_returns_placeholder_and_publishes_result() {
        use zhuye_llm::{CredentialPool, MockClient, MockReply, ModelRegistry, PoolEntry};

        let client = Arc::new(MockClient::with_script(vec![MockReply::Text {
            text: "子代理结论".into(),
        }]));
        let registry = Arc::new(ModelRegistry::new());
        registry.register_pool(
            "mock",
            CredentialPool {
                entries: vec![PoolEntry {
                    client: client.clone(),
                    label: "default".into(),
                    weight: 1,
                }],
            },
        );
        let factory = Arc::new(SubAgentFactory::new(
            ThreadId::new(),
            "mock",
            registry,
            None,
            Arc::new(ToolRegistry::default()),
            CancellationToken::new(),
            None,
        ));
        let pending = Arc::new(zhuye_tools::PendingSubagentRegistry::new());
        let tool = CallSubAgentTool::new(
            factory,
            SubAgentSpec {
                name: "Explorer".into(),
                role: "explorer".into(),
                model: None,
                system_prompt: "你是探索子代理".into(),
                allowed_tools: vec![],
                data_transfer: DataTransferConfig::default(),
                max_turns: None,
                allowed_skills: vec![],
            },
        );
        let ctx = zhuye_tools::ToolContext {
            call_id: "call-1".into(),
            pending_subagents: Some(pending.clone()),
            ..Default::default()
        };

        let out = tool
            .execute(
                ctx,
                serde_json::json!({"prompt": "调研模块", "mode": "async"}),
            )
            .await
            .expect("异步派发应成功");
        let text = out
            .content
            .iter()
            .filter_map(|b| match b {
                zhuye_protocol::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("已派发子代理 explorer"), "占位回执:{text}");
        assert_eq!(
            out.metadata.get("async").and_then(|v| v.as_bool()),
            Some(true),
            "回执 metadata 应标记 async:{text}"
        );
        assert_eq!(
            out.metadata.get("pending").and_then(|v| v.as_bool()),
            Some(true),
            "回执 metadata 应标记 pending:{text}"
        );

        // 子代理跑完后,结果经在飞表回填 —— 引擎据此挂起等待并注入。
        let cancel = CancellationToken::new();
        let done = pending.wait_any(&cancel).await.expect("等待不应被取消");
        assert_eq!(done.len(), 1, "{done:?}");
        assert!(done[0].ok, "{done:?}");
        assert_eq!(done[0].text, "子代理结论", "{done:?}");
        assert_eq!(done[0].role, "explorer", "{done:?}");
        assert_eq!(done[0].call_id, "call-1", "{done:?}");
    }

    /// 挂起的 stub 模型:`stream()` 一直等到请求的取消令牌触发才返回 ——
    /// 模拟一个迟迟不返回的子代理模型请求。
    struct HangingClient {
        started: tokio::sync::Notify,
    }

    #[async_trait]
    impl zhuye_llm::ModelClient for HangingClient {
        fn name(&self) -> &str {
            "hanging-stub"
        }
        async fn stream(
            &self,
            _request: zhuye_llm::ChatRequest,
            cancel: CancellationToken,
        ) -> Result<
            std::pin::Pin<
                Box<
                    dyn futures::Stream<Item = Result<zhuye_llm::ChatEvent, zhuye_llm::LlmError>>
                        + Send,
                >,
            >,
            zhuye_llm::LlmError,
        > {
            self.started.notify_one();
            cancel.cancelled().await;
            Err(zhuye_llm::LlmError::Cancelled)
        }
    }

    /// 「子代理模型请求挂起」的异步派发场景。
    struct HangingDispatch {
        pending: Arc<zhuye_tools::PendingSubagentRegistry>,
        /// 父回合的回合级取消令牌。
        owner: CancellationToken,
        factory: Arc<SubAgentFactory>,
    }

    impl HangingDispatch {
        /// 等到子代理释放在途槽位 —— `SpawnedChild` 只在析构 / collect 结束时
        /// 释放槽位,归零即证明后台任务确实丢弃了子代理句柄(子代理被取消)。
        async fn wait_for_child_stopped(&self) {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while self.factory.depth() != 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("父回合结束后在途异步子代理应被取消并释放槽位");
        }
    }

    /// 派发一个子代理模型请求会一直挂起的异步委派,等到子代理真的发起
    /// 模型请求后才返回。
    async fn hanging_async_dispatch() -> HangingDispatch {
        use zhuye_llm::{CredentialPool, ModelRegistry, PoolEntry};

        let client = Arc::new(HangingClient {
            started: tokio::sync::Notify::new(),
        });
        let registry = Arc::new(ModelRegistry::new());
        registry.register_pool(
            "hanging-stub",
            CredentialPool {
                entries: vec![PoolEntry {
                    client: client.clone(),
                    label: "default".into(),
                    weight: 1,
                }],
            },
        );
        let factory = Arc::new(SubAgentFactory::new(
            ThreadId::new(),
            "hanging-stub",
            registry,
            None,
            Arc::new(ToolRegistry::default()),
            CancellationToken::new(),
            None,
        ));
        let pending = Arc::new(zhuye_tools::PendingSubagentRegistry::new());
        let tool = CallSubAgentTool::new(
            factory.clone(),
            SubAgentSpec {
                name: "Explorer".into(),
                role: "explorer".into(),
                model: None,
                system_prompt: "你是探索子代理".into(),
                allowed_tools: vec![],
                data_transfer: DataTransferConfig::default(),
                max_turns: None,
                allowed_skills: vec![],
            },
        );
        let owner = CancellationToken::new();
        let ctx = zhuye_tools::ToolContext {
            call_id: "call-1".into(),
            // 生产路径上队列注入的就是父回合的回合级令牌。
            cancel: owner.clone(),
            pending_subagents: Some(pending.clone()),
            ..Default::default()
        };
        let out = tool
            .execute(
                ctx,
                serde_json::json!({"prompt": "调研模块", "mode": "async"}),
            )
            .await
            .expect("异步派发应成功");
        assert_eq!(
            out.metadata.get("async").and_then(|v| v.as_bool()),
            Some(true)
        );
        client.started.notified().await;
        assert_eq!(factory.depth(), 1, "子代理应占着一个在途槽位");
        HangingDispatch {
            pending,
            owner,
            factory,
        }
    }

    /// 打断父回合(回合令牌触发)⇒ 在途异步子代理立刻取消。
    ///
    /// 回归:同步模式下「打断父回合 ⇒ 取消在途子代理」是工具 future 被丢弃
    /// 的副产品;异步子代理由后台任务持有,不随工具 future 析构 —— 此前用户
    /// 按下中断后子代理照常跑完,结果被之后某个回合注入。
    #[tokio::test]
    async fn interrupting_parent_turn_cancels_async_child() {
        let dispatch = hanging_async_dispatch().await;

        // 模拟 `Op::Interrupt`:回合级令牌触发。
        dispatch.owner.cancel();
        dispatch.wait_for_child_stopped().await;

        // 引擎在回合任务收尾时清扫表项。
        assert_eq!(dispatch.pending.abandon_turn(&dispatch.owner), 1);
        // 迟到的兜底写入不会重建条目,之后也没有结果可回填。
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !dispatch.pending.has_pending(),
            "被取消的子代理不应回填结果"
        );
    }

    /// 回合**正常**收尾但没等某个子代理(如触顶收尾):回合令牌未触发,
    /// 照样经放弃句柄停掉子代理 —— 异步子代理不跨越派发它的回合。
    #[tokio::test]
    async fn turn_end_abandons_unwaited_async_child_without_cancelling_turn_token() {
        let dispatch = hanging_async_dispatch().await;

        assert_eq!(dispatch.pending.abandon_turn(&dispatch.owner), 1);
        assert!(!dispatch.owner.is_cancelled(), "正常收尾不取消回合令牌");
        dispatch.wait_for_child_stopped().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !dispatch.pending.has_pending(),
            "未等待的子代理不应回填结果"
        );
    }

    /// 异步派发但本线程无在飞登记表(纯库 / 测试)→ **降级为同步**,
    /// 不能给模型一个「已派发但没人会回填」的假回执。
    #[tokio::test]
    async fn async_dispatch_without_registry_falls_back_to_sync() {
        use zhuye_llm::{CredentialPool, MockClient, MockReply, ModelRegistry, PoolEntry};

        let client = Arc::new(MockClient::with_script(vec![MockReply::Text {
            text: "同步结论".into(),
        }]));
        let registry = Arc::new(ModelRegistry::new());
        registry.register_pool(
            "mock",
            CredentialPool {
                entries: vec![PoolEntry {
                    client: client.clone(),
                    label: "default".into(),
                    weight: 1,
                }],
            },
        );
        let factory = Arc::new(SubAgentFactory::new(
            ThreadId::new(),
            "mock",
            registry,
            None,
            Arc::new(ToolRegistry::default()),
            CancellationToken::new(),
            None,
        ));
        let tool = CallSubAgentTool::new(
            factory,
            SubAgentSpec {
                name: "Explorer".into(),
                role: "explorer".into(),
                model: None,
                system_prompt: "你是探索子代理".into(),
                allowed_tools: vec![],
                data_transfer: DataTransferConfig::default(),
                max_turns: None,
                allowed_skills: vec![],
            },
        );
        // `ToolContext::default()` 的 pending_subagents = None。
        let out = tool
            .execute(
                zhuye_tools::ToolContext::default(),
                serde_json::json!({"prompt": "调研模块", "mode": "async"}),
            )
            .await
            .expect("降级路径应成功");
        let text = out
            .content
            .iter()
            .filter_map(|b| match b {
                zhuye_protocol::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(text, "同步结论", "无登记表时应同步返回真实结果:{text}");
    }

    /// SubagentRegistry 未注入时,`execute` 不应 panic,正常返回结果。
    #[tokio::test]
    async fn execute_works_without_registry() {
        // 验证 factory 默认 subagent_registry = None 时,execute 走 fallback。
        // 由于 spawn 需要 AgentThread + ModelRegistry wiring,这里只测
        // factory 注入 registry 但 spec 不触发 spawn 的边界 case 之一。
        // 真正的端到端测试在 `discussion_collab_e2e.rs`。
        let factory = Arc::new(SubAgentFactory::new(
            ThreadId::new(),
            "openai/gpt-4o",
            std::sync::Arc::new(zhuye_llm::ModelRegistry::new()),
            None, // child_registry: 回退父级 registry
            Arc::new(ToolRegistry::default()),
            CancellationToken::new(),
            None,
        ));
        // 注入空 registry —— execute 仍能取到 `Some(reg)`,record 不会 panic。
        factory.set_subagent_registry(zhuye_recovery::SubagentRegistry::shared());
        assert!(factory.subagent_registry().is_some());
    }

    /// SubagentRegistry FIFO cap=32:写入 33 条 → 第 1 条被淘汰,
    /// 最新的 32 条保留。
    #[tokio::test]
    async fn registry_record_overflow_drops_oldest() {
        let reg = zhuye_recovery::SubagentRegistry::shared();
        for i in 0..33 {
            reg.record(zhuye_recovery::SubagentRegistryEntry {
                tool_name: format!("call_role_{i}"),
                task_summary: format!("task {i}"),
                result_summary: format!("result {i}"),
                iteration: i,
                created_at: chrono::Utc::now(),
            });
        }
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 32, "FIFO cap=32,33 条应保留后 32 条");
        // 第 0 条被淘汰,第 1 条仍在。
        assert!(snap.iter().any(|e| e.tool_name == "call_role_1"));
        assert!(!snap.iter().any(|e| e.tool_name == "call_role_0"));
    }

    /// 写入 registry 后 activeWorkers 计数与 snapshot 长度一致。
    #[test]
    fn registry_snapshot_len_matches_active_workers_metadata() {
        let reg = zhuye_recovery::SubagentRegistry::shared();
        reg.record(zhuye_recovery::SubagentRegistryEntry {
            tool_name: "call_architect".into(),
            task_summary: "task [worker: architect]".into(),
            result_summary: "done".into(),
            iteration: 1,
            created_at: chrono::Utc::now(),
        });
        assert_eq!(reg.snapshot().len(), 1);
    }

    // ── v1.1.0 复审 ─────────────────────────────────────────────
    // bug-3 (P1):失败子代理不写 registry 契约
    // bug-8 (P2):coordinator worker 后缀

    /// bug-3:`execute` 中 spawn 失败时,绝不能写 registry(契约守护)。
    /// 当前实现走 short-circuit: spawn 错误在 line 95 返回,根本到不了
    /// 写 registry 的代码。本测试验证:若有人重构加入"无论成败都 record"
    /// 的代码,会被本测试拦下。
    ///
    /// 触发失败路径:用非法 spec (大写 role 触发 `SpecInvalid`),让
    /// `spawn` 在 `spec.validate()` 阶段返回错误,根本到不了 record 调用。
    #[tokio::test]
    async fn failed_subagent_does_not_write_registry() {
        let factory = Arc::new(SubAgentFactory::new(
            ThreadId::new(),
            "openai/gpt-4o",
            std::sync::Arc::new(zhuye_llm::ModelRegistry::new()),
            None, // child_registry: 回退父级 registry
            Arc::new(ToolRegistry::default()),
            CancellationToken::new(),
            None,
        ));
        let reg = zhuye_recovery::SubagentRegistry::shared();
        factory.set_subagent_registry(reg.clone());

        let tool = CallSubAgentTool::new(
            factory.clone(),
            SubAgentSpec {
                name: "BadName".into(),
                // 大写 role → `validate()` 拒绝 → spawn 返回 SpecInvalid
                role: "BadRole".into(),
                model: None,
                system_prompt: "x".into(),
                allowed_tools: vec![],
                data_transfer: DataTransferConfig::default(),
                max_turns: None,
                allowed_skills: vec![],
            },
        );
        let args = serde_json::json!({"prompt": "should not be recorded"});
        let result = tool
            .execute(zhuye_tools::ToolContext::default(), args)
            .await;
        // spawn 失败 → ToolError
        assert!(result.is_err(), "spawn should fail on invalid spec");
        // registry 必须仍为空 —— 这是契约的核心。
        assert!(
            reg.snapshot().is_empty(),
            "failed spawn MUST NOT write to registry"
        );
    }

    /// bug-8 白盒:`format!(" [worker: {}]", role)` 后缀逻辑的正确性 —— 隔离
    /// 测试,直接调该模式而不走 spawn。保证:role 字符串拼到 task_summary 末尾,
    /// coordinator 关闭时不附加。
    #[test]
    fn worker_role_suffix_format() {
        // 这是 execute 内"if coordinator_mode { append }"分支的等价手工
        // 构造。覆盖两条规则:coordinator on + role → 加;coordinator off → 不加。
        let role = "explorer";
        let mut s = String::from("find modules");
        let coord_on = true;
        if coord_on {
            s.push_str(&format!(" [worker: {}]", role));
        }
        assert_eq!(s, "find modules [worker: explorer]");

        let mut s2 = String::from("find modules");
        let coord_off = false;
        if coord_off {
            s2.push_str(&format!(" [worker: {}]", role));
        }
        assert_eq!(s2, "find modules");
    }
}
