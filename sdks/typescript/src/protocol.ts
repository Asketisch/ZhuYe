//! ZhuYe 协议 v0 —— SDK 与 `zhuye serve` 子命令之间的 wire 格式。
//!
//! 协议冻结,新增变体属 non-breaking addition;SDK 解析未知事件时
//! 退到 `UnknownEventMsg` 不抛错,让 Rust 核心升级不破坏下游。

/**
 * Submission 是 client → serve 的一条命令。`id` 用于把随之产生的事件
 * (`Event.id`)关联回原始 Submission。
 */
export interface Submission {
  id: string;
  op: Op;
  client_user_message_id?: string;
}

/** 会话使用的提示词选择策略。 */
export type PromptSelection =
  | { mode: 'default' }
  | { mode: 'auto' }
  | { mode: 'manual'; profile: string };

/**
 * Op —— 客户端可向 core 提交的操作。`type` 字段是判别符,与 Rust
 * `#[serde(tag = "type", rename_all = "snake_case")]` 一致。
 */
export type Op =
  | { type: 'user_input'; items: UserInputItem[]; thread_settings?: ThreadSettings }
  | { type: 'compact' }
  | { type: 'interrupt'; child_id?: string | null }
  | { type: 'rewind'; to_turn_id?: string | null }
  | { type: 'shutdown' }
  | { type: 'tool_approval'; id: string; decision: ReviewDecision }
  | { type: 'hook_approval'; id: string; decision: ReviewDecision }
  | { type: 'enter_plan_mode'; task: string }
  | { type: 'exit_plan_mode' }
  | { type: 'plan_approval'; id: string; choice: PlanApprovalChoice }
  | { type: 'set_effort'; effort: ReasoningEffort }
  | {
      type: 'ask_user_question_response';
      id: string;
      answers: AskUserAnswer;
    }
  | { type: 'ask_user_input_response'; id: string; text: string }
  /** MCP server 交互请求的回执;`content` 的键名取服务端 schema 字段名。 */
  | {
      type: 'mcp_elicitation_response';
      id: string;
      action: 'accept' | 'decline' | 'cancel';
      content?: Record<string, unknown> | null;
    }
  | { type: 'set_permission_mode'; mode: PermissionMode }
  | { type: 'cycle_permission_mode' }
  | { type: 'enter_goal_mode'; goal: string; verify_command?: string | null; token_budget?: number | null }
  | { type: 'exit_goal_mode' }
  /** v1.3 SDK:注册客户端自定义工具(实现留在客户端进程)。 */
  | { type: 'register_tools'; tools: RemoteToolSpec[] }
  /** v1.3 SDK:对 `EventMsg::ToolExecutionRequest` 的回执。 */
  | { type: 'tool_execution_response'; call_id: string; output: ToolOutput }
  /** v1.6.1:提示词层管理(list 查询 / switch 重扫 `.zhuye/prompts`)。 */
  | {
      type: 'prompt_command';
      action: { type: 'list' } | { type: 'switch'; layer?: string | null }
        | { type: 'select'; selection: PromptSelection | null }
        | { type: 'save_default'; selection: PromptSelection };
    }
  /** v1.x:运行时重扫扩展目录(skills 已接线;commands 位回执恒 false)。 */
  | { type: 'rescan_extensions'; skills: boolean; commands: boolean }
  /** F01:查询 prompt_guard 裁决统计(应答 guard_stats)。 */
  | { type: 'query_guard_stats' }
  /** v1.4 A2:回合中途转向 —— 不打断在飞 turn,下一个安全点注入。 */
  | { type: 'steer'; priority?: 'now' | 'attachment'; items: UserInputItem[] }
  /** v1.4 C1:查询子代理状态快照(应答 subagent_status)。 */
  | { type: 'query_subagents'; child_id?: string }
  /** v1.x:启动 side-channel agent(用户驱动的并发独立 agent)。 */
  | { type: 'start_side_channel'; agent: string; prompt: string }
  /** v1.x:取消 side-channel(未知 / 已终态 id 为 noop,不回错误)。 */
  | { type: 'cancel_side_channel'; id: string }
  /** v1.x:查询 side-channel 快照(应答 side_channel_status)。 */
  | { type: 'query_side_channels'; id?: string };

export interface UserInputItem {
  type: 'text' | 'image';
  text?: string;
  /** 图片字节与 MIME 类型;仅在 type 为 image 时使用。 */
  data?: number[];
  mime_type?: string;
}

export interface ThreadSettings {
  /** @deprecated 服务端不读取此字段;请使用 set_effort Op。 */
  effort?: ReasoningEffort | null;
  /** @deprecated 服务端不读取此字段;请使用 set_permission_mode Op。 */
  permission_mode?: PermissionMode | null;
  model?: string | null;
  approval_policy?: 'auto' | 'prompt' | 'deny' | 'auto_approve' | 'full_access' | null;
  sandbox_policy?: 'workspace_only' | 'os_sandbox' | 'full_access' | null;
  max_tool_concurrency?: number | null;
}

// v1.10 功能4:max / ultra 为新增档位(不同模型族上限不同,由引擎按模型钳制)
export type ReasoningEffort = 'low' | 'medium' | 'high' | 'max' | 'ultra';

export type PermissionMode = 'auto' | 'prompt' | 'plan' | 'deny' | 'bubble';

export type PlanApprovalChoice =
  | 'auto_mode'
  | 'manual_approve'
  | 'revise';

/**
 * `ReviewDecision` 的 wire 形态(serde snake_case)。
 *
 * 注意是**扁平**结构而非 tagged 对象:`Approve` 序列化为裸字符串
 * `"approve"`,`Deny` 序列化为 `{"deny":{"reason":...}}`。旧版误写成
 * `{type:'approve'}`,服务端无法反序列化,审批会永久挂起(v1.3 修复)。
 */
export type ReviewDecision =
  | 'approve'
  | 'approve_for_session'
  | { deny: { reason: string } };

export interface AskUserAnswer {
  answers: Answer[];
}
export interface Answer {
  selected: number[];
  custom?: string | null;
}

/** v1.3 SDK:客户端自定义工具声明。 */
export interface RemoteToolSpec {
  name: string;
  description: string;
  /** JSON Schema 对象。 */
  parameters: Record<string, unknown>;
}

/** v1.3 SDK:远程工具执行结果(回执 wire 格式)。 */
export interface ToolOutput {
  content: ContentBlock[];
  is_error: boolean;
  metadata?: Record<string, unknown>;
  elapsed_ms?: number;
}

export type ContentBlock =
  | { type: 'text'; text: string }
  | { type: 'image'; data: number[]; mime_type: string }
  | { type: 'diff'; unified_diff: string }
  | { type: 'tool_use'; id: string; name: string; args: unknown }
  | { type: 'tool_result'; call_id: string; output: ToolOutput };

/**
 * Event —— core → client 的状态单元。`id` 与 `Submission.id` 对齐;
 * 不绑定 Submission 时(如 `session_configured` / `shutdown_complete`)
 * 取 `EVENT_ID_NONE`(空字符串)。
 */
export interface Event {
  id: string;
  msg: EventMsg;
}

export const EVENT_ID_NONE = '';

/**
 * 已知 EventMsg 变体的联合。未知变体降为 `UnknownEventMsg`(让 Rust
 * 核心新增变体时下游仍能解析)。
 */
export type EventMsg =
  | SessionConfigured
  | TurnStarted
  | TurnComplete
  | TurnAborted
  | TurnRewound
  | ShutdownComplete
  | SubmissionClosed
  | AgentMessage
  | AgentMessageDelta
  | AgentMessageReset
  | ThinkingDelta
  | TokenCount
  | ToolCallBegin
  | ToolCallArgsDelta
  | ToolCallOutputDelta
  | ToolCallEnd
  | ToolExecutionRequest
  | ApprovalRequest
  | AskUserQuestion
  | AskUserInput
  | PermissionBubble
  | ContextCompacted
  | ErrorEvent
  | StreamError
  | ConfigReloaded
  | PromptList
  | PromptSwitched
  | ExtensionsRescanned
  | GuardStats
  | CacheWarming
  | StreamRecovery
  | CollabStarted
  | CollabMessage
  | CollabFinished
  | McpServerStarted
  | McpServerFailed
  | McpToolInvoked
  | McpElicitation
  | LspServerStarted
  | LspServerFailed
  | PlanRequest
  | PlanReady
  | PlanApproved
  | PlanRejected
  | PermissionModeChanged
  | PlanDraftUpdated
  | PlanStep
  | PluginLoaded
  | QuotaExhausted
  | Routing
  | UnknownEventMsg;

export interface SessionConfigured {
  type: 'session_configured';
  session_id: string;
  model: string;
  provider: string;
  approval_policy: 'auto' | 'prompt';
  sandbox_policy: 'workspace_only' | 'full_access';
  context_window_size?: number | null;
}

export interface TurnStarted {
  type: 'turn_started';
  turn_id: string;
  user_message_id: string;
}

export interface TurnComplete {
  type: 'turn_complete';
  turn_id: string;
  status: 'success' | 'max_iterations' | 'stopped' | 'token_budget_exceeded';
  usage: TokenUsage;
}

export interface TurnAborted {
  type: 'turn_aborted';
  turn_id: string;
  reason: AbortReason;
}

export type AbortReason =
  | { type: 'user_interrupt' }
  | { type: 'shutdown' }
  | { type: 'error'; code: string; message: string };

export interface TurnRewound {
  type: 'turn_rewound';
  to_turn_id: string | null;
  truncated_after: number;
}

export interface ShutdownComplete {
  type: 'shutdown_complete';
}

/**
 * v1.3 SDK:某条 submission 的 per-turn 通道已排空(该 submission 在
 * core 处理完毕,不会再有任何事件)。`Event.id` 携带对应 submission id。
 * serve 在 drain task 结束时发出;非 turn 操作(compact / rewind / 权限
 * 模式切换 / goal 等)没有 `turn_complete` 之类的终态事件,`submit()`
 * 迭代器靠本事件收尾,否则会永久挂起。
 */
export interface SubmissionClosed {
  type: 'submission_closed';
}

export interface AgentMessage {
  type: 'agent_message';
  text: string;
}

export interface AgentMessageDelta {
  type: 'agent_message_delta';
  delta: string;
}

/**
 * v1.10:本 turn 流式 assistant 输出被重置 —— 模型调用失败将自动重试,
 * 客户端应清空该 turn 正在流式生成的消息缓冲(文本 / thinking / 未
 * 完成的工具调用),等待重试的全新输出,防止两次输出拼接。
 */
export interface AgentMessageReset {
  type: 'agent_message_reset';
  reason_code: string;
  retry_in_ms: number;
}

export interface ThinkingDelta {
  type: 'thinking_delta';
  delta: string;
}

export interface TokenCount {
  type: 'token_count';
  input_tokens: number;
  output_tokens: number;
  cached_tokens?: number;
  cache_write_tokens?: number;
  total_tokens?: number;
  provider?: string;
  credential_label?: string;
  /**
   * True = 本回合累计(引擎回合收尾补发);缺省 = 单次调用。
   * 两者不可一并累计,否则会话总量与费用翻倍。
   */
  turn_total?: boolean;
}

export interface TokenUsage {
  input_tokens: number;
  output_tokens: number;
  cached_tokens?: number;
  cache_write_tokens?: number;
}

export interface ToolCallBegin {
  type: 'tool_call_begin';
  call_id: string;
  tool_name: string;
  args: Record<string, unknown>;
  child_id?: string | null;
}

/**
 * v1.5:模型生成工具参数期间的原始 JSON 片段(逐段转发)。
 *
 * 与 `tool_call_begin` 的 `call_id` 对应同一通调用;客户端把各片段按
 * 到达顺序拼接即得完整参数 JSON。additive 变体 —— 老客户端按未知
 * `type` 忽略即可。
 */
export interface ToolCallArgsDelta {
  type: 'tool_call_args_delta';
  call_id: string;
  delta: string;
}

/** 工具执行期间的输出预览,最终结果以 tool_call_end 为准。 */
export interface ToolCallOutputDelta {
  type: 'tool_call_output_delta';
  call_id: string;
  delta: string;
  is_stderr: boolean;
}

export interface ToolCallEnd {
  type: 'tool_call_end';
  call_id: string;
  output: ToolOutput;
  is_error: boolean;
  elapsed_ms: number;
  child_id?: string | null;
}

/** v1.3 SDK:请求客户端执行其注册的远程工具。 */
export interface ToolExecutionRequest {
  type: 'tool_execution_request';
  call_id: string;
  tool: string;
  args: Record<string, unknown>;
}

export interface ApprovalRequest {
  type: 'approval_request';
  request_id: string;
  kind: ApprovalKind;
  risk: 'low' | 'medium' | 'high';
}

export type ApprovalKind =
  | { type: 'tool'; tool_name: string; args: Record<string, unknown> }
  | { type: 'hook'; hook_name: string; decision_preview: string };

export interface AskUserQuestion {
  type: 'ask_user_question';
  request_id: string;
  questions: Question[];
}

export interface Question {
  header: string;
  question: string;
  options: QuestionOption[];
  multi_select?: boolean;
}

export interface QuestionOption {
  label: string;
  description: string;
  preview?: string;
}

export interface AskUserInput {
  type: 'ask_user';
  request_id: string;
  prompt: string;
  secret?: boolean;
  placeholder?: string | null;
}

export interface PermissionBubble {
  type: 'permission_bubble';
  tool_name: string;
  args_preview?: string | null;
  risk: 'low' | 'medium' | 'high';
}

export interface ContextCompacted {
  type: 'context_compacted';
  strategy: 'truncate' | 'summarize';
  before_tokens?: number;
  after_tokens?: number;
}

export interface ErrorEvent {
  type: 'error';
  code: string;
  message: string;
  details?: Record<string, unknown>;
}

export interface StreamError {
  type: 'stream_error';
  code: string;
  message: string;
  retry_in_ms: number;
  provider?: string;
  credential_label?: string;
}

export interface ConfigReloaded {
  type: 'config_reloaded';
  sections_changed?: string[];
}

export interface ExtensionsRescanned {
  type: 'extensions_rescanned';
  skills?: number;
  commands?: boolean;
}

/** F01:`query_guard_stats` 的应答 —— prompt_guard 裁决统计快照。 */
export interface GuardStats {
  type: 'guard_stats';
  enabled: boolean;
  definitions_blocked: number;
  definitions_reviewed: number;
  outputs_blocked: number;
  outputs_reviewed: number;
}

/** v1.x:prompt cache 保温(cache warming)状态事件 —— 引擎主动推,无对应
 * Op(镜像 context_composition 形态)。默认关闭([cache_warming] enabled),
 * 开启后可用 warm_cached_tokens 与后续 token_count.cached_tokens 对照验证
 * 保温收益。reason ∈ no_economics / no_cache_hits / below_threshold /
 * window_exhausted / stale / replay_failed 等。 */
export interface CacheWarming {
  type: 'cache_warming';
  action: 'scheduled' | 'executed' | 'skipped' | 'cancelled';
  phase: 'turn' | 'idle';
  reason?: string;
  prompt_tokens?: number;
  warm_cached_tokens?: number;
  warm_cost_usd?: number;
  expected_savings_usd?: number;
  model?: string;
  /** UNIX 秒。 */
  at: number;
}

/** v1.x:流锚点恢复事件族(四个变体共享载荷,type 区分)——
 * started / anchor_selected / tail_discarded / retry_started。
 * 可观察诊断事件,不改变 stream_error / agent_message_reset 语义。 */
export interface StreamRecovery {
  type:
    | 'stream_recovery_started'
    | 'stream_recovery_anchor_selected'
    | 'stream_recovery_tail_discarded'
    | 'stream_recovery_retry_started';
  attempt: number;
  reason_code: string;
  transient?: string;
  anchor?: string;
  discarded_text_chars?: number;
  discarded_thinking_chars?: number;
  retry_in_ms?: number;
}

export interface CollabStarted {
  type: 'collab_started';
  discussion_id: string;
  topic: string;
}

export interface CollabMessage {
  type: 'collab_message';
  discussion_id: string;
  participant: string;
  text: string;
  token_usage?: TokenUsage;
}

export interface CollabFinished {
  type: 'collab_finished';
  discussion_id: string;
  result?: string | null;
}

export interface McpServerStarted {
  type: 'mcp_server_started';
  server: string;
  tool_count: number;
  tool_names?: string[];
  transport?: string;
}

export interface McpServerFailed {
  type: 'mcp_server_failed';
  server: string;
  reason: string;
  will_retry: boolean;
}

export interface McpToolInvoked {
  type: 'mcp_tool_invoked';
  server: string;
  tool: string;
  elapsed_ms: number;
  is_error: boolean;
}

export type McpElicitationFieldKind =
  | { type: 'text'; secret: boolean }
  | { type: 'number'; integer: boolean }
  | { type: 'boolean' }
  | { type: 'enum'; options: string[]; multi: boolean };

export interface McpElicitationField {
  /** 回执 `content` 里使用的键名(服务端 schema 属性名)。 */
  name: string;
  title?: string;
  description?: string;
  required: boolean;
  kind: McpElicitationFieldKind;
}

export type McpElicitationKind =
  | { type: 'form'; fields: McpElicitationField[] }
  | { type: 'link'; url: string };

/** MCP server 向宿主请求用户输入;用 `mcp_elicitation_response` 回执。 */
export interface McpElicitation {
  type: 'mcp_elicitation';
  request_id: string;
  server: string;
  message: string;
  kind: McpElicitationKind;
}

export interface LspServerStarted {
  type: 'lsp_server_started';
  server: string;
  methods: string[];
  language_ids: string[];
}

export interface LspServerFailed {
  type: 'lsp_server_failed';
  server: string;
  reason: string;
}

export interface PlanRequest {
  type: 'plan_request';
  task: string;
}

export interface PlanReady {
  type: 'plan_ready';
  plan_id: string;
  markdown: string;
}

export interface PlanApproved {
  type: 'plan_approved';
  plan_id: string;
  choice: PlanApprovalChoice;
}

export interface PlanRejected {
  type: 'plan_rejected';
  plan_id: string;
  reason?: string | null;
}

export interface PermissionModeChanged {
  type: 'permission_mode_changed';
  from: PermissionMode;
  to: PermissionMode;
}

export interface PlanDraftUpdated {
  type: 'plan_draft_updated';
  draft_id: string;
  markdown: string;
}

export interface PlanStep {
  type: 'plan_step';
  plan_id: string;
  index: number;
  total: number;
  status: 'pending' | 'in_progress' | 'completed' | 'failed';
  step: string;
}

export interface PluginLoaded {
  type: 'plugin_loaded';
  plugin: string;
  skill_count: number;
  command_count: number;
}

export interface QuotaExhausted {
  type: 'quota_exhausted';
  credential: string;
  cooldown_until?: string | null;
}

export interface Routing {
  type: 'routing';
  role: string;
  kind: string;
  detail?: string;
}

export interface UnknownEventMsg {
  type: string;
  [k: string]: unknown;
}

/** 提示词选择查询结果。 */
export interface PromptList {
  type: 'prompt_list';
  available: string[];
  current_spec: string;
  active_layers: string[];
  selection?: PromptSelection;
  profile?: string;
  fallback_reason?: string;
}

/** 提示词重扫或选择结果。 */
export interface PromptSwitched {
  type: 'prompt_switched';
  action: string;
  active_layers: string[];
  selection?: PromptSelection;
  profile?: string;
  fallback_reason?: string;
}
