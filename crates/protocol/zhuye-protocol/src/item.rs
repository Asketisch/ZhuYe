//! 由 Submission / Op / EventMsg 引用到的子类型。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::question::AskUserAnswer;

/// 围绕 UUID 的 newtype,用于线程标识。在一个会话的整个生命周期内保持稳定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThreadId(pub Uuid);

impl ThreadId {
    /// 生成新的线程标识(UUID v4)。
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// 批次二十四(#9):parse UUID 字符串(来自 session overlay 的 `SessionLine.id`)
    /// 为 `ThreadId`,让 `/session` Enter 能解析选中行恢复目标。
    pub fn parse_str(s: &str) -> Result<Self, uuid::Error> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl Default for ThreadId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ThreadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 围绕 UUID 的 newtype,用于回合标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(pub Uuid);

impl TurnId {
    /// 生成新的回合标识(UUID v4)。
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// 把 UUID 字符串(例如 `Op::Rewind.to_turn_id` 中的)解析为 `TurnId`。
    /// 供 `submission_loop` 中 rewind-truncate 路径使用,把 wire 形态的
    /// `Option<String>` 转换为 recorder 期望的类型化 id。
    pub fn parse_str(s: &str) -> Result<Self, uuid::Error> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl Default for TurnId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TurnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 一段用户输入(文本 / 图片 / 技能激活 / 问题回答 / 文件 mention)。
///
/// v1.1.0 P1 #14 新增 `QuestionAnswer` —— 允许用户通过标准 `Op::UserInput`
/// 流回答 `EventMsg::AskUserQuestion`(与 `Op::AskUserQuestionResponse` 平行,
/// 适合 TUI 在 question modal 之外用 input bar 自由输入答案的场景)。
///
/// v1.x 新增 `File` —— Composer `@` 弹层选中的文件。`path` 为相对当前
/// workspace 的 POSIX 路径(不携带绝对路径到日志/recorder);真实读取由
/// LLM `/read` 工具按需触发(避免无谓 IO 与抽象泄露)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserInputItem {
    /// 纯文本输入,投递给模型时成为用户消息中的文本块。
    Text {
        /// 用户输入的文本内容。
        text: String,
    },
    /// 内联图片输入(多模态),与同批 Text / File 按序合并进同一条用户消息。
    Image {
        /// 图片原始字节(JSON wire 上为 0-255 整数数组,非 base64)。
        data: Vec<u8>,
        /// 图片 MIME 类型,如 `image/png` / `image/jpeg`。
        mime_type: String,
    },
    /// 按路径引用的本地图片(区别于内联字节的 `Image`)。
    /// 注:core 尚未接通该变体,投递给模型时会被跳过。
    LocalImage {
        /// 图片文件在本机上的路径(按引用传递,不内联字节)。
        path: PathBuf,
    },
    /// 显式激活一个技能(skill)。
    /// 注:core 尚未接通该变体,投递给模型时会被跳过。
    Skill {
        /// 技能名,对应技能目录(catalog)中登记的名称。
        name: String,
        /// 可选的激活参数(任意 JSON);wire 上可省略,缺省为 `None`。
        #[serde(default)]
        args: Option<serde_json::Value>,
    },
    /// v1.1.0 P1 #14: 对 LLM 主动询问的结构化回答。`request_id` 与
    /// `EventMsg::AskUserQuestion.request_id` 配对。`None` 表示用户按
    /// Esc 取消(LLM 收到空答案)。
    QuestionAnswer {
        /// 所回答问题的请求 id,与 `AskUserQuestionEvent.request_id` 配对。
        request_id: String,
        /// 逐题答案集合:每题给出选中 option 的下标列表与可选自定义文本。
        answers: AskUserAnswer,
    },
    /// v1.x:在工作区中 @ 一个文件路径。投递到 LLM 时由
    /// `submission_loop::user_input_items_to_messages` 展开为带路径标注的
    /// 文本块(如 `@src/main.rs` 或 `@src/main.rs:L10-L20`),真实读取由
    /// `/read` 工具按需触发。
    File {
        /// 相对工作区的 POSIX 路径。
        path: String,
        /// 可选:被引用行区间(1-indexed,含端点);`None` = 整文件引用。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        range: Option<FileRange>,
    },
}

/// `UserInputItem::File` 的可选行区间。1-indexed,含端点。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRange {
    /// 起始行号(1-indexed,含该行)。
    pub start_line: u32,
    /// 结束行号(1-indexed,含该行);与 `start_line` 一起渲染为
    /// `@<path>:L<start>-L<end>` 标注。
    pub end_line: u32,
}

/// 工具运行所需的用户信任级别。
///
/// 放在 `zhuye-protocol` 中,用于打破 `zhuye-tools` (需要在 `ToolSpec::required_permission` 里使用)
/// 与 `zhuye-hooks` (需要在 `HookDecision::PermissionOverride` 里使用) 之间的循环依赖。
/// `zhuye-hooks` 重新导出此类型。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// 工具直接运行,无需提示(只读工具的默认值)。
    #[default]
    Auto,
    /// 工具需经用户审批才能运行(有副作用工具的默认值)。
    Prompt,
    /// 工具被禁止运行。
    Deny,
    /// Plan mode(v1.x):用户主动进入只读规划阶段,
    /// 任何不在白名单内的工具(包括 bash/edit/write)都会被
    /// `PlanModeGate` hook blanket-deny。
    Plan,
    /// 自动批准文件编辑类工具(`write` / `edit` / `delete`),其余
    /// `Prompt` 工具仍走 approval modal(编辑类工具自动批准)。
    AcceptEdits,
    /// 非阻塞 bubble 通知 + 自动批准:emit `PermissionBubble` 事件供
    /// TUI 展示,不弹 blocking modal。
    Bubble,
    /// 危险:静默跳过所有工具审批,不弹 modal、不发 bubble。
    /// `ask_user_question` 等主动索取人类输入的工具(那些仍会弹出)。
    Bypass,
}

impl PermissionMode {
    /// 人类可读标签(用于日志和 TUI approval modal)。
    pub fn as_str(&self) -> &'static str {
        match self {
            PermissionMode::Auto => "auto",
            PermissionMode::Prompt => "prompt",
            PermissionMode::Deny => "deny",
            PermissionMode::Plan => "plan",
            PermissionMode::AcceptEdits => "accept_edits",
            PermissionMode::Bubble => "bubble",
            PermissionMode::Bypass => "bypass",
        }
    }

    /// TUI `/mode` / Tab / Shift+Tab 的循环顺序。
    /// `default → accept edits → plan → bypass permissions`。
    /// 循环 = `Auto→AcceptEdits→Plan→Bypass→Auto`。
    /// `Prompt | Deny | Bubble` 不进默认循环:从它们循环会落到 `Auto`
    /// (三者仍经 `/mode <name>` 直达,见 `slash::parse_mode`)。
    pub fn next_in_ui_cycle(self) -> Self {
        match self {
            PermissionMode::Auto => PermissionMode::AcceptEdits,
            PermissionMode::AcceptEdits => PermissionMode::Plan,
            PermissionMode::Plan => PermissionMode::Auto,
            PermissionMode::Bypass => PermissionMode::Auto,
            PermissionMode::Prompt | PermissionMode::Deny | PermissionMode::Bubble => {
                PermissionMode::Auto
            }
        }
    }

    /// 是否应在 `ApprovalGate::ask_tool` 中自动批准(不弹 modal)。
    pub fn auto_approves_tool(&self, tool_name: &str) -> bool {
        match self {
            PermissionMode::Bubble => true,
            PermissionMode::Bypass => false,
            PermissionMode::AcceptEdits => is_edit_tool_name(tool_name),
            _ => false,
        }
    }
}

/// 文件编辑类工具名 —— `AcceptEdits` 模式短路用。
///
/// review 修复:补上实际注册名 `delete_file` / `notebook_edit`
/// (此前只匹配 `delete` / `Delete`,永不命中)。
pub fn is_edit_tool_name(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "write"
            | "edit"
            | "delete"
            | "delete_file"
            | "notebook_edit"
            | "Write"
            | "Edit"
            | "Delete"
            | "NotebookEdit"
    )
}

/// Plan 模式的会话级唯一标识。Plan 由 `EnterPlanModeTool` 进入、
/// `ExitPlanModeTool` 退出,整个生命周期由 `plan_id` 串联。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlanId(pub Uuid);

impl PlanId {
    /// 生成新的 plan 标识(UUID v4)。
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for PlanId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for PlanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for PlanId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(PlanId(Uuid::parse_str(s)?))
    }
}

/// 用户对工具 / Hook 审批请求的裁决,经 `Op::ToolApproval` / `Op::HookApproval`
/// 回传给审批门。wire 形态:`"approve"` / `"approve_for_session"` /
/// `{"deny":{"reason":"..."}}`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    /// 仅批准本次调用。
    Approve,
    /// 拒绝本次调用,工具不执行;`reason` 作为错误结果反馈给模型。
    Deny {
        /// 拒绝原因;审批门在取消、中断、超时或命中拒绝规则时也会自行合成。
        reason: String,
    },
    /// 批准并在本会话内放行该工具:后续同名调用免审批(`High` 风险调用除外)。
    ApproveForSession,
}

/// v1.x Plan mode 审批选择(对齐业界通用的三选项体验)。
///
/// `Op::PlanApproval` 携带此枚举,submission_loop 据此决定 plan 批准后的目标
/// `PermissionMode`(而非旧的硬编码 `Prompt`)。与 [`ReviewDecision`] 分开:
/// 后者用于工具/Hook 审批(y/n/a),语义不同(ApproveForSession 在 plan 路径
/// 与 Approve 合并),混用会污染工具审批语义。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanApprovalChoice {
    /// 「Yes, use auto mode」:切到 `AcceptEdits`(自动批准编辑/写入类,
    /// Bash 等仍走白名单/审批)。保守的"自动"——对齐业界通用的 auto mode。
    AutoMode,
    /// 「Yes, manually approve edits」:切到 `Prompt`(逐工具审批,旧行为)。
    ManualApprove,
    /// 「Tell the agent what to change」:留在 plan 模式,用户输入反馈继续 plan。
    /// 不改 permission mode,emit `PlanRejected`(等价于 reject + 回到 plan 编辑)。
    Revise,
}

/// `Op::UserInput.thread_settings` 携带的设置覆盖,在处理该条输入时生效;
/// 各字段 `None` 表示不覆盖、沿用会话配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreadSettingsOverrides {
    /// 本回合使用的模型 spec(如 `openai/gpt-4o`);`None` 跟随会话当前模型
    /// (可随配置热重载切换)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 本回合审批策略覆盖:`Prompt` 强制启用审批门,`Deny` 拒绝所有需审批的
    /// 工具;`Auto` / `None` 跟随会话级审批配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<ApprovalPolicy>,
    /// 本回合 OS 沙箱覆盖:`OsSandbox` 强制启用,`WorkspaceOnly` / `FullAccess`
    /// 关闭 OS 层(文件工具的工作区路径校验不受影响);`None` 回退到配置 / 环境变量。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_policy: Option<SandboxPolicy>,
    /// 工具执行队列的并发上限(0 按 1 处理);调整作用于会话级队列,
    /// 对后续回合持续生效直到再次覆盖。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_concurrency: Option<usize>,
}

/// 审批策略:用于 `ThreadSettingsOverrides` 的回合级覆盖,以及
/// `SessionConfiguredEvent` 的会话快照。
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// 不强制审批,沿用会话级审批配置(默认值)。
    #[default]
    Auto,
    /// 强制启用审批门(即使会话级审批关闭),需审批的工具走审批流程。
    Prompt,
    /// 拒绝所有需审批的工具调用:不弹审批、直接以拒绝结果返回模型。
    Deny,
}

/// 沙箱策略:用于 `ThreadSettingsOverrides` 的回合级 OS 沙箱覆盖,以及
/// `SessionConfiguredEvent` 的会话快照。
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxPolicy {
    /// 仅工作区路径约束:文件类工具拒绝越界路径,不启用 OS 级沙箱(默认值)。
    #[default]
    WorkspaceOnly,
    /// M3+ (requires landlock/mac-sandbox).
    OsSandbox,
    /// M3+ (no restrictions).
    FullAccess,
}

/// 工具调用的审批风险等级(按声明序可比较,`Low < Medium < High`),
/// 随 `ApprovalRequestEvent` / `PermissionBubbleEvent` 下发供 UI 提示。
/// bash 由命令分类器动态定级,其余工具经执行队列时默认 `Medium`。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// 低风险:只读 / 无副作用(如分类为 Safe 的 bash 命令);默认值。
    #[default]
    Low,
    /// 中风险:有副作用但可控(如分类为 Risky 的 bash 命令)。
    Medium,
    /// 高风险:破坏性 / 系统级操作;审批门不会以会话内放行或
    /// `AcceptEdits` / `Bubble` 模式的自动批准跳过它。
    High,
}

/// v1.x S4:`/effort` slash 命令的协议镜像枚举。
///
/// 之所以**不**直接复用 `zhuye_llm::ReasoningEffort`,是因为
/// `zhuye-protocol` 不能反向依赖 `zhuye-llm`(避免循环依赖 + 协议层
/// 与实现层解耦)。本枚举与 `zhuye_llm::ReasoningEffort` 字段一一对应,
/// 由 `zhuye-core::submission_loop` 在收到 `Op::SetEffort` 后调
/// `From<ReasoningEffortMirror> for ReasoningEffort` 桥接到 LLM 路径。
///
/// `Default = Low` 与 Anthropic / OpenAI 默认 reasoning 强度一致;`/effort`
/// 不带参数时也按 Low 处理,避免 "未设置 = 不思考" 的歧义。
///
/// v1.10 功能4:新增 `Max` / `Ultra` 两档(变体级 additive,旧端不识别时
/// 按 serde 未知变体降级)。不同模型族支持的最高档不同(qwen 系到 High、
/// deepseek 到 Max、OpenAI 到 Ultra),由 `zhuye_llm` 的能力表在**发 wire
/// 前**钳制 —— 本枚举表达「用户想要的强度」,不代表目标模型一定支持。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffortMirror {
    /// 低强度思考(默认档)。
    #[default]
    Low,
    /// 中等强度思考。
    Medium,
    /// 高强度思考;qwen 系及未知模型族可用的最高档。
    High,
    /// 极高强度思考(v1.10 新增);deepseek 系的最高档,超出目标模型上限时向下钳制。
    Max,
    /// 最高强度思考(v1.10 新增);仅 OpenAI 推理系(`gpt-5*` / `gpt-6*` / o 系)
    /// 支持,其余模型向下钳制到各自上限。
    Ultra,
}

impl ReasoningEffortMirror {
    /// 档位 wire 字符串(`low` / `medium` / `high` / `max` / `ultra`),
    /// 与 serde snake_case 序列化保持同一口径,供配置解析 / CLI 校验复用。
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffortMirror::Low => "low",
            ReasoningEffortMirror::Medium => "medium",
            ReasoningEffortMirror::High => "high",
            ReasoningEffortMirror::Max => "max",
            ReasoningEffortMirror::Ultra => "ultra",
        }
    }
}

impl std::str::FromStr for ReasoningEffortMirror {
    type Err = String;

    /// 大小写不敏感解析(`"high"` / `"High"` / `"HIGH"` 均可);
    /// 未知字符串返回 `Err`(带合法档位清单,便于直接拼进错误提示)。
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lowered = s.trim().to_ascii_lowercase();
        match lowered.as_str() {
            "low" => Ok(ReasoningEffortMirror::Low),
            "medium" => Ok(ReasoningEffortMirror::Medium),
            "high" => Ok(ReasoningEffortMirror::High),
            "max" => Ok(ReasoningEffortMirror::Max),
            "ultra" => Ok(ReasoningEffortMirror::Ultra),
            _ => Err(format!(
                "unknown reasoning effort '{s}' (expected one of low|medium|high|max|ultra)"
            )),
        }
    }
}

/// v1.4 A2:转向消息优先级的协议层镜像(与实现层
/// `zhuye_core::steering_queue::SteeringPriority` 字段一一对应,由
/// `submission_loop` 在收到 `Op::Steer` 后桥接)。`Attachment` 为默认值:
/// 未显式声明优先级的转向按「参考资料」处理,保守不冒充直接指令。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SteeringPriorityMirror {
    /// 参考资料:以 `<system-reminder>` 包裹注入,下一安全点生效。
    #[default]
    Attachment,
    /// 立即指示:作为用户中途说话直入上下文,打断当前规划方向。
    Now,
}

/// `EventMsg::SessionConfigured` 载荷:会话就绪握手事件(全局事件,`id` 为空),
/// 报告会话 id 与当前模型配置;热重载切换模型后会重新下发。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfiguredEvent {
    /// 本会话的线程标识,与 rollout 文件名及 `SessionMeta.session_id` 一致。
    pub session_id: ThreadId,
    /// 当前生效的模型 spec(如 `openai/gpt-4o`)。
    pub model: String,
    /// 模型提供方名称,取自模型 spec 的 `/` 前缀;无前缀时为 `"unknown"`。
    pub provider: String,
    /// 会话审批策略。注:引擎目前恒填默认值 `Auto`,并不反映实际审批配置。
    pub approval_policy: ApprovalPolicy,
    /// 会话沙箱策略。注:引擎目前恒填默认值 `WorkspaceOnly`,并不反映实际沙箱状态。
    pub sandbox_policy: SandboxPolicy,
    /// 引擎报告的模型上下文窗口大小(token),供 TUI 上下文用量条做分母。
    /// 缺省 `None`(未知模型 / 旧 payload),TUI 此时优雅省略上下文条。
    /// 经 `zhuye_llm::context_window_for` 回退表补全,避免与 LLM 层漂移。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_size: Option<u32>,
}

/// 工具产生的输出内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    /// 输出内容块,随工具结果回传给模型(`Text` / `Image` 原样传递,
    /// 其余变体序列化为 JSON 文本)。
    pub content: Vec<ContentBlock>,
    /// 是否为失败结果(执行出错、被拒、超时等),随工具结果一并告知模型。
    pub is_error: bool,
    /// 结构化附加信息(如 `{"approval":"denied"}`、`{"timeout_ms":...}`);
    /// 不随工具结果发给模型,供客户端、hooks 与引擎内部逻辑读取。
    pub metadata: serde_json::Value,
    /// 工具执行耗时(毫秒);未实际执行即返回(如审批被拒、hook 拦截)时为 0。
    pub elapsed_ms: u64,
}

/// 工具执行错误。放在 protocol(而非 `zhuye-tools`)中,以便
/// `zhuye-hooks` 能在 `HookEvent::PostToolUseFailure` 中携带它,
/// 避免出现 `tools ↔ hooks` 循环依赖。
#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolError {
    /// 参数非法:缺失必填项、类型错误或取值越界。
    #[error("invalid arguments: {message}")]
    InvalidArgs {
        /// 错误说明(指出哪个参数有何问题),经 Display 拼入错误文本反馈给模型。
        message: String,
    },
    /// 工具执行失败的通用错误,内含失败描述。
    #[error("execution failed: {0}")]
    Execution(String),
    /// 因权限不足被拒绝执行。
    #[error("permission denied: {reason}")]
    PermissionDenied {
        /// 拒绝原因,经 Display 拼入错误文本反馈给模型。
        reason: String,
    },
    /// 执行超时(执行队列的统一超时,或工具内部的命令 / 网络请求超时)。
    #[error("timeout after {elapsed_ms}ms")]
    Timeout {
        /// 超时时的耗时(毫秒);部分工具直接取所配置的超时阈值。
        elapsed_ms: u64,
    },
    /// 执行被取消(取消令牌触发,如用户中断或会话关闭)。
    #[error("cancelled")]
    Cancelled,
    /// 路径越界:目标路径解析后不在工作区内。
    #[error("path escaped sandbox: {path}")]
    PathEscape {
        /// 被拒绝的越界路径(原始参数或解析后的绝对路径,视工具而定)。
        path: std::path::PathBuf,
    },
    /// I/O 错误(文件读写、网络请求等),内含错误描述;`std::io::Error` 可经
    /// `From` 自动转换为此变体。
    #[error("io: {0}")]
    Io(String),
    /// 被 hook 否决执行。
    #[error("hook denied: {reason}")]
    HookDenied {
        /// hook 给出的否决原因,经 Display 拼入错误文本反馈给模型。
        reason: String,
    },
    /// v1.3:OS 沙箱不可用且安全基线要求严格模式,fail-closed。
    /// BashTool 不会回退到裸 `sh -c`;直接返回此错误。
    #[error("sandbox unavailable: {reason}")]
    SandboxUnavailable {
        /// 不可用的具体原因(后端缺失 / 初始化失败,或不可信命令被拒绝裸执行)。
        reason: String,
    },
}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        ToolError::Io(e.to_string())
    }
}

/// 内容块:工具输出(`ToolOutput.content`)、回合内容历史(模型文本 / 工具调用 /
/// 工具结果)与 rollout 持久化共用的基本单元。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// 纯文本块。
    Text {
        /// 文本正文(模型输出或工具输出)。
        text: String,
    },
    /// 图片块(多模态),以原始字节内联。
    Image {
        /// 图片原始字节(JSON wire 上为 0-255 整数数组,非 base64)。
        data: Vec<u8>,
        /// 图片 MIME 类型,如 `image/png`。
        mime_type: String,
    },
    /// 文件改动 diff(由 `edit` / `write` / `ast` 等文件改写类工具产出)。
    Diff {
        /// unified diff 格式的改动文本。
        unified_diff: String,
    },
    /// LLM 请求的工具调用(M2+)。在模型调用后通过 `latest_content`
    /// 暴露,供 graph 决定是否 dispatch。
    ToolUse {
        /// 模型分配的工具调用 id,与对应 `ToolResult.call_id` 配对。
        id: String,
        /// 被调用的工具名。
        name: String,
        /// 模型给出的调用参数(已解析的 JSON)。
        args: serde_json::Value,
    },
    /// 工具执行结果(M2+)。在队列运行后追加到 `latest_content`,
    /// 使下一次模型调用能看到工具输出。
    ToolResult {
        /// 所对应工具调用的 id(即 `ToolUse.id`)。
        call_id: String,
        /// 工具执行输出(内容块、错误标记、元数据与耗时)。
        output: ToolOutput,
    },
}

impl ContentBlock {
    /// 最常见变体的便捷构造函数。
    pub fn text(s: impl Into<String>) -> Self {
        ContentBlock::Text { text: s.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_id_is_unique() {
        let a = ThreadId::new();
        let b = ThreadId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn user_input_item_text_serde() {
        let item = UserInputItem::Text { text: "hi".into() };
        let j = serde_json::to_string(&item).unwrap();
        assert!(j.contains(r#""type":"text""#), "got: {j}");
    }

    #[test]
    fn user_input_item_question_answer_roundtrip() {
        use crate::question::{Answer, AskUserAnswer};
        let item = UserInputItem::QuestionAnswer {
            request_id: "q-1".into(),
            answers: AskUserAnswer {
                answers: vec![Answer::single(1).with_custom("alt: skip")],
            },
        };
        let j = serde_json::to_string(&item).unwrap();
        assert!(j.contains(r#""type":"question_answer""#), "got: {j}");
        assert!(j.contains(r#""request_id":"q-1""#), "got: {j}");
        let back: UserInputItem = serde_json::from_str(&j).unwrap();
        if let UserInputItem::QuestionAnswer {
            request_id,
            answers,
        } = back
        {
            assert_eq!(request_id, "q-1");
            assert_eq!(answers.answers.len(), 1);
            assert_eq!(answers.answers[0].selected, vec![1]);
            assert_eq!(answers.answers[0].custom.as_deref(), Some("alt: skip"));
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn approval_policy_default_is_auto() {
        assert_eq!(ApprovalPolicy::default(), ApprovalPolicy::Auto);
    }

    #[test]
    fn permission_mode_default_is_auto() {
        assert_eq!(PermissionMode::default(), PermissionMode::Auto);
    }

    #[test]
    fn permission_mode_serde_uses_snake_case() {
        let s = serde_json::to_string(&PermissionMode::Prompt).unwrap();
        assert_eq!(s, "\"prompt\"");
        let back: PermissionMode = serde_json::from_str(&s).unwrap();
        assert_eq!(back, PermissionMode::Prompt);
    }

    #[test]
    fn permission_mode_plan_serde_roundtrip() {
        // v1.x: Plan 变体必须能被 serde 序列化与反序列化,且 wire 格式为 `"plan"`。
        let s = serde_json::to_string(&PermissionMode::Plan).unwrap();
        assert_eq!(s, "\"plan\"", "Plan 应序列化为小写字符串");
        let back: PermissionMode = serde_json::from_str(&s).unwrap();
        assert_eq!(back, PermissionMode::Plan);
        assert_eq!(PermissionMode::Plan.as_str(), "plan");
    }

    #[test]
    fn permission_mode_accept_edits_and_bubble_serde_roundtrip() {
        for mode in [PermissionMode::AcceptEdits, PermissionMode::Bubble] {
            let s = serde_json::to_string(&mode).unwrap();
            let back: PermissionMode = serde_json::from_str(&s).unwrap();
            assert_eq!(back, mode);
        }
        assert_eq!(PermissionMode::AcceptEdits.as_str(), "accept_edits");
        assert_eq!(PermissionMode::Bubble.as_str(), "bubble");
    }

    #[test]
    fn permission_mode_bypass_serde_roundtrip() {
        // Bypass 必须能被 serde 序列化与反序列化,且 wire 格式为 `"bypass"`。
        let s = serde_json::to_string(&PermissionMode::Bypass).unwrap();
        assert_eq!(s, "\"bypass\"", "Bypass 应序列化为小写字符串");
        let back: PermissionMode = serde_json::from_str(&s).unwrap();
        assert_eq!(back, PermissionMode::Bypass);
        assert_eq!(PermissionMode::Bypass.as_str(), "bypass");
    }

    #[test]
    fn permission_mode_ui_cycle_is_three_step() {
        // v1.4: Auto → AcceptEdits → Plan → Auto (Bypass 已移除)
        assert_eq!(
            PermissionMode::Auto.next_in_ui_cycle(),
            PermissionMode::AcceptEdits
        );
        assert_eq!(
            PermissionMode::AcceptEdits.next_in_ui_cycle(),
            PermissionMode::Plan
        );
        assert_eq!(
            PermissionMode::Plan.next_in_ui_cycle(),
            PermissionMode::Auto
        );
        // Bypass 不在循环内，从 Bypass 会回到 Auto
        assert_eq!(
            PermissionMode::Bypass.next_in_ui_cycle(),
            PermissionMode::Auto
        );
    }

    #[test]
    fn permission_mode_ui_cycle_non_default_falls_to_auto() {
        // Prompt / Deny / Bubble 不进默认循环,从它们循环会落到 Auto。
        for m in [
            PermissionMode::Prompt,
            PermissionMode::Deny,
            PermissionMode::Bubble,
        ] {
            assert_eq!(m.next_in_ui_cycle(), PermissionMode::Auto, "from {m:?}");
        }
    }

    #[test]
    fn bypass_does_not_auto_approve() {
        // v1.4: Bypass 不再静默放行所有工具，仅保留反序列化兼容。
        for tool in ["bash", "write", "Edit", "delete", "read", "custom"] {
            assert!(
                !PermissionMode::Bypass.auto_approves_tool(tool),
                "Bypass should NOT auto-approve {tool}"
            );
        }
    }

    #[test]
    fn session_configured_event_missing_context_window_defaults_none() {
        // 旧 wire payload(无 context_window_size 字段)反序列化时该字段应为 None。
        let j = r#"{
            "session_id": "00000000-0000-0000-0000-000000000000",
            "model": "claude-sonnet-4-latest",
            "provider": "anthropic",
            "approval_policy": "auto",
            "sandbox_policy": "workspace_only"
        }"#;
        let sc: SessionConfiguredEvent = serde_json::from_str(j).unwrap();
        assert_eq!(sc.context_window_size, None);
    }

    #[test]
    fn is_edit_tool_name_matches_write_edit_delete() {
        assert!(is_edit_tool_name("write"));
        assert!(is_edit_tool_name("Edit"));
        assert!(!is_edit_tool_name("bash"));
    }

    #[test]
    fn plan_id_is_unique_and_roundtrips() {
        // PlanId 必须是会话级唯一;透明 newtype 直接序列化底层 UUID。
        let a = PlanId::new();
        let b = PlanId::new();
        assert_ne!(a, b);
        let j = serde_json::to_string(&a).unwrap();
        // `#[serde(transparent)]` 直接输出 UUID 字符串,无额外包装。
        let back: PlanId = serde_json::from_str(&j).unwrap();
        assert_eq!(back, a);
    }
}
