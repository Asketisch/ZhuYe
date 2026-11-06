//! `zhuye` —— 顶层库门面。
//!
//! 支持两种访问方式:
//!
//! 1. **扁平 re-export**(扁平重导出)—— 适合希望显式写出类型名的深度用户:
//!
//!    ```rust
//!    use zhuye::{AgentThread, Submission, Op, Event, EventMsg, ModelRegistry};
//!    ```
//!
//! 2. **Builder**(构建器)—— 一行启动 + 合理默认:
//!
//!    ```rust,no_run
//!    use zhuye::{Submission, Zhuye};
//!
//!    #[tokio::main]
//!    async fn main() {
//!        let agent = Zhuye::builder("openai/gpt-4o") // 指定模型
//!            .workspace(".")                         // 指定工作目录
//!            .build() // 自带完整 M4(压缩器 + 记忆 + 技能 + JSONL recorder)
//!            .expect("builder 初始化失败");
//!        // 提交输入并消费事件流
//!        let mut stream = agent.submit(Submission::user_input("hello")).await;
//!        while let Some(_event) = stream.next().await {}
//!    }
//!    ```
//!
//! 门面映射关系见 `docs/architecture.md` 的「门面映射」一节。

// 门面是对外 SDK 形象:本 crate 内新定义的 pub item 缺文档即告警
// (re-export 不受影响,沿用来源 crate 的文档)。
#![warn(missing_docs)]

pub mod builder;
pub mod m4_bootstrap;
pub mod stream;

// ── Core(保持原状)─────────────────────────────────────────────────────────
pub use zhuye_core::{AgentConfig, AgentThread, TurnHandle};

// ── Side-channel(用户驱动并发 agent,WP9/13)────────────────────────────
pub use zhuye_core::side_channel::{
    MAX_CONCURRENT, SideChannelDriver, SideChannelEvent, SideChannelId, SideChannelInfo,
    SideChannelRegistry, SideChannelStartError, SideChannelStatus, SideThreadFactory,
};

// ── LLM ────────────────────────────────────────────────────────────────────
pub use zhuye_llm::{
    AnthropicClient, AnthropicConfig, Capabilities, ChatEvent, ChatMessage, ChatRequest,
    ContentBlock as LlmContentBlock, CredentialPool, LlmError, ModelClient, ModelRegistry,
    OllamaClient, OllamaConfig, OpenAIClient, OpenAIConfig, PoolEntry, ProviderKind, RateCard,
    SharedModelRegistry, SystemBlocks, ThinkingConfig, price, pricing_for, pricing_table,
    providers, rate_card, rate_cards,
};

// ── 协议(Protocol)───────────────────────────────────────────────────────
pub use zhuye_protocol::{
    AbortReason, AgentMessage, AgentMessageDelta, ApprovalKind, ApprovalRequestEvent, ContentBlock,
    ContextCompactedEvent, ContextCompactedStrategy, ErrorEvent, Event, EventMsg, MessageRole,
    ModelUsage, NullRecorder, Op, PromptAction, PromptListEvent, PromptSwitchedEvent,
    ProtocolError, ReviewDecision, RiskLevel, RolloutRecord, RolloutRecorder, SandboxPolicy,
    SessionConfiguredEvent, SessionInfo, SessionStats, StreamErrorEvent, Submission, ThreadId,
    TokenCountEvent, TokenUsage, ToolCallBeginEvent, ToolCallEndEvent, ToolOutput,
    TurnAbortedEvent, TurnCompleteEvent, TurnId, TurnStartedEvent, TurnStatus, UserInputItem,
};

// ── Tools ──────────────────────────────────────────────────────────────────
pub use zhuye_protocol::PermissionMode;
pub use zhuye_tools::{
    Tool, ToolContext, ToolError, ToolOutput as ToolOutputType, ToolRegistry, ToolSpec,
};

// ── Hooks(M6 新暴露)────────────────────────────────────────────────────────
pub use zhuye_hooks::{
    Hook, HookAbortSignal, HookContext, HookDecision, HookEngine, HookError, HookEvent,
    HookEventKind, SystemMessage,
};

// ── Memory(新暴露)─────────────────────────────────────────────────────────
pub use zhuye_memory::{FileMemoryStore, InMemoryStore, MemoryScope, MemoryStore};

// ── Skills(新暴露)─────────────────────────────────────────────────────────
pub use zhuye_skills::SkillsCatalog;

// ── Agent definitions(新暴露)──────────────────────────────────────────────
pub use zhuye_agent_def::AgentDefinition;

// ── Prompts(按模型匹配的外置提示词库,v1.6)──────────────────────────────
// TUI / GUI 下游经此消费:.zhuye/prompts/*.md 的发现、加载、按模型
// 三级回退(default → 厂商 → 精确模型名)与进程级初始化。
pub use zhuye_prompt::{PromptLibrary, PromptSet, init_library, library, split_spec};

// ── Compact(新暴露)─────────────────────────────────────────────────────────
pub use zhuye_compact::Compactor;

// ── Rollout(新暴露)─────────────────────────────────────────────────────────
pub use zhuye_rollout::JsonlRolloutWriter;
// v1.x 读取侧:SDK 消费者(TUI / GUI)不依赖 zhuye-cli 即可查询历史会话
// —— 列表、按 id 定位、回放、markdown 导出、按模型统计。
pub use zhuye_rollout::index::{
    find_file_by_session_id, find_session_path, list_sessions, list_sessions_in_workspace,
};
pub use zhuye_rollout::{all_model_usage, replay, replay_path, session_stats, to_markdown};

// ── Telemetry(v1.x 新暴露)────────────────────────────────────────────────
// 数据记录 SDK 化:写入侧 `TelemetrySink` 由 `ZhuyeBuilder::with_telemetry`
// 注入(默认按 `[telemetry]` config 自动构造);读取侧 API 让 TUI / GUI
// 直接查询 model-io 全文(含 system prompt / 工具名 / usage)与 span 日志。
pub use zhuye_telemetry::{
    LangfuseConfig, LangfuseExporter, Level, ModelIoRecord, ModelRef, SpanStatus, TelemetrySink,
    TraceEvent, UsageSnapshot, list_model_io_files, list_span_log_files, read_model_io_file,
    read_span_log_file, resolve_traces_dir,
};

// ── Subagent(新暴露)───────────────────────────────────────────────────────
pub use zhuye_subagent::{CallSubAgentTool, DataTransferConfig, SubAgentFactory, SubAgentSpec};

// ── 审计:Activity 时间线(WP12)────────────────────────────────────────
pub use zhuye_telemetry::{
    ActivityEvent, ActivityFilter, ActivityKind, ActivityLevel, ActivityLogger, map_event,
    pump_events,
};

// ── 定时:integration 层的 cron / autopilot(WP11)─────────────────────
pub use zhuye_stream::{
    AutopilotConfig, AutopilotManager, AutopilotRun, AutopilotRunStatus, CronDriverHandle,
    CronJobSpec, CronSchedule, CronScheduler, CronStatus, HISTORY_CAP as AUTOPILOT_HISTORY_CAP,
    QueueCounts, QueueDriverHandle, QueueScheduler, QueueState, QueueStatusSummary, QueueTask,
    QueueTaskStatus,
};

// ── 编排:任务 / 团队 / squad(WP10)───────────────────────────────────
pub use zhuye_task::{
    Actor, ActorKind, ActorType, SquadError, SquadManager, SquadMember, SquadSpec, TaskManager,
    TeamFile, TeamMemberSpec,
};

// ── 讨论(Discussion,M9 首次暴露,v0.2.3 扩展)──────────────────────────────
// 顶层 re-export:核心 facade (M9) + LLM 实战集成所需的高级 API (v0.2.3)。
// `comm_tools` / `runtime` / `orchestrator` 仍走 `zhuye_discussion::*`,
// 避免 facade 臃肿;advanced 用户也可直接 import 完整路径。
pub use zhuye_discussion::{
    // 高级 API (v0.2.3 —— 真实 LLM 集成所需)
    AgentId,
    AgentSection,
    // 核心 facade (M9)
    DiscussionConfig,
    DiscussionId,
    DiscussionMode,
    DiscussionOrchestrator,
    DiscussionResult,
    MessageBus,
    MessageId,
    MessageKind,
    OrchestratorEvent,
    // v0.2.3 LLM wiring helper:把 AgentSection[] 编译成 LlmContext,
    // 返回 prompt_for_closure 喂给 DiscussionOrchestrator::run。
    build_context,
    prompt_for_closure,
};

// ── Builder 外观(Builder facade)──────────────────────────────────────
pub use builder::{BuilderError, ZhuyeBuilder};

/// 便捷别名:用一条链式调用构造 `Zhuye`。`Zhuye` 即
/// `zhuye::Zhuye`,包装一个 `AgentThread`。
pub type Zhuye = builder::Zhuye;
