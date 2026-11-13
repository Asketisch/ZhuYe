//! 配置数据结构。所有字段 `Option<>` / `Default`，未知 TOML 键不报错。
//!
//! ## 子模块拆分
//!
//! - [`provider`] —— 各 provider 段(`AnthropicSection` / `OpenAISection` / `OllamaSection`) + `CredentialConfig`
//! - [`agent`] —— 路由 / 子代理 / 协调器 / 压缩 / token 预算
//! - [`discovery`] —— 模型清单发现(`[model_discovery]` 段)
//! - [`notifications`] —— 通知 / 遥测 / 分析
//! - [`integrations`] —— MCP / LSP / PostgreSQL / Redis / Bridge / Voice / DAP / ACP / 网络搜索
//! - [`plugins`] —— 插件市场配置 / 插件段
//!
//! 所有子模块类型通过 `pub use` 重导出,外部代码仍走 `crate::schema::XxxSection`。

pub mod agent;
pub mod discovery;
pub mod integrations;
pub mod notifications;
pub mod permission_syntax;
pub mod plugins;
pub mod provider;

pub use integrations::ACP_DEFAULT_BIND;

pub use agent::{
    AnthropicSubagentSection, CompactSection, CoordinatorSection, OllamaSubagentSection,
    OpenAISubagentSection, RoutingSection, SpecSlotConfig, SubagentProvidersSection,
    SubagentSpecConfig, TokenBudgetSection,
};
pub use discovery::ModelDiscoverySection;
pub use integrations::{
    AcpSection, BridgeSection, DapSection, LspFilePattern, LspServerEntry, LspServersSection,
    McpServerEntry, McpServersSection, McpTransport, PostgresSessionSection, SseRedisSection,
    VoiceSection, WebSearchSection,
};
pub use notifications::{
    AnalyticsSection, NotificationChannel, NotificationsSection, TelemetrySection,
    TuiNotificationsSection,
};
pub use permission_syntax::parse_permission_entry;
pub use plugins::{PluginMarketplaceConfig, PluginMarketplaceKind, PluginsSection};
pub use provider::{
    AnthropicSection, CredentialConfig, OllamaSection, OpenAISection, QuotaConfig, QuotaSource,
};

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ── 顶层配置 ──────────────────────────────────────────────────

/// 顶层配置 —— 对应 `~/.zhuye/config.toml` 的根表。
///
/// 注:不再 derive `Eq` —— provider 段的 `temperature: Option<f32>`
/// 不实现 `Eq`;`PartialEq` 已覆盖所有既有用法(断言比较)。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ReflectConfig {
    #[serde(default)]
    pub active: ActiveSection,
    #[serde(default)]
    pub anthropic: Option<AnthropicSection>,
    #[serde(default)]
    pub openai: Option<OpenAISection>,
    #[serde(default)]
    pub ollama: Option<OllamaSection>,
    #[serde(default)]
    pub compact: CompactSection,
    #[serde(default)]
    pub token_budget: Option<TokenBudgetSection>,
    #[serde(default)]
    pub sandbox: SandboxSection,
    #[serde(default)]
    pub hooks: HooksSection,
    #[serde(default)]
    pub mcp_servers: McpServersSection,
    #[serde(default)]
    pub lsp_servers: LspServersSection,
    #[serde(default)]
    pub plugins: PluginsSection,
    #[serde(default)]
    pub routing: Option<RoutingSection>,
    #[serde(default)]
    pub subagent_providers: Option<SubagentProvidersSection>,
    #[serde(default)]
    pub coordinator: Option<CoordinatorSection>,
    #[serde(default)]
    pub sanitize: Option<SanitizeSection>,
    #[serde(default)]
    pub prompt_guard: Option<PromptGuardSection>,
    #[serde(default)]
    pub ask_user_question: Option<AskUserQuestionSection>,
    #[serde(default)]
    pub model: Option<ModelSection>,
    /// 按模型覆盖上下文窗口(TOML:`[context_windows]`)。
    /// 优先于 `context_window_for` 静态回退表 —— 用来支持未被识别的
    /// 私端/新模型。
    /// 例子:
    /// ```toml
    /// [context_windows]        # 段名
    /// "MiniMax-M3" = 1000000   # 模型名 → 上下文 token 数
    /// "qwen36-1m"  = 1000000   # 模型名 → 上下文 token 数
    /// ```
    #[serde(default)]
    pub context_windows: Option<ContextWindowsSection>,
    /// 模型清单发现配置(TOML:`[model_discovery]`)。
    ///
    /// 段缺省时用内置默认(发现能力开启 + 内置免费判定规则)。
    #[serde(default)]
    pub model_discovery: Option<ModelDiscoverySection>,
    #[serde(default)]
    pub goal: Option<GoalSection>,
    #[serde(default)]
    pub telemetry: Option<TelemetrySection>,
    #[serde(default)]
    pub analytics: Option<AnalyticsSection>,
    #[serde(default)]
    pub notifications: Option<NotificationsSection>,
    #[serde(default)]
    pub postgres_session: Option<PostgresSessionSection>,
    #[serde(default)]
    pub sse_redis: Option<SseRedisSection>,
    #[serde(default)]
    pub bridge: Option<BridgeSection>,
    #[serde(default)]
    pub voice: Option<VoiceSection>,
    #[serde(default)]
    pub dap: Option<DapSection>,
    #[serde(default)]
    pub acp: Option<AcpSection>,
    #[serde(default)]
    pub web_search: Option<WebSearchSection>,
    /// 工具权限白名单 / 黑名单规则。加载时合并进 permission store(与
    /// `~/.zhuye/permissions.toml` 并存,config 规则优先)。示例:
    /// ```toml
    /// [[permissions.rule]]        # 权限规则数组
    /// tool = "Bash"                 # 工具名
    /// action = "allow"              # 动作:allow / deny / ask
    /// shell_pattern = "git *"       # 仅 Bash,命令 glob(修复后生效)
    /// ```
    #[serde(default)]
    pub permissions: Option<PermissionsSection>,
    #[serde(default)]
    pub subagents: Vec<SubagentSpecConfig>,
    #[serde(default = "default_config_version")]
    pub config_version: u32,
}

pub const CURRENT_CONFIG_VERSION: u32 = 1;

fn default_config_version() -> u32 {
    1
}

impl ReflectConfig {
    /// v1.10 功能3:`[context_windows]` 表的原始条目(未合并
    /// `[model].context_window`)。供 `apply_to_registry` 同步进
    /// `zhuye_llm` 进程级全局覆盖表。
    pub fn context_window_override_entries(&self) -> HashMap<String, u32> {
        self.context_windows
            .as_ref()
            .map_or_else(|| Default::default(), |c| c.entries.clone())
    }

    /// v1.10 功能3:合并后的上下文窗口覆盖表 —— `[context_windows]`
    /// per-model 条目 + `[model].context_window`(全局值作用于**当前
    /// 活跃模型**,per-model 条目优先),**所有键统一归一化**(去 provider
    /// 前缀 / ollama tag、小写),消费方(`AgentConfig` 侧查询 /
    /// `zhuye_llm` 全局表)用同一口径即可命中。exec bootstrap / serve 用
    /// 它装配 `AgentConfig::context_window_overrides`,让 `[model]` 段的
    /// 窗口真正进引擎(此前该字段解析后无消费方)。
    pub fn effective_context_window_overrides(&self, active_model: &str) -> HashMap<String, u32> {
        // 先把原始表条目按键归一化重排(同键冲突时保留后者,与 lookup
        // 的「首个命中」语义可能略有出入 —— 配置里给同一模型写两行本身
        // 就是错误状态)。
        let mut map: HashMap<String, u32> = self
            .context_window_override_entries()
            .into_iter()
            .map(|(k, v)| (normalize_model_key(&k), v))
            .filter(|(k, _)| !k.is_empty())
            .collect();
        if let Some(w) = self.model.as_ref().and_then(|m| m.context_window) {
            let key = normalize_model_key(active_model);
            if !key.is_empty() {
                // per-model 条目已「覆盖」该模型就不插入 —— 覆盖判定必须
                // 与消费方的查找规则同口径(精确 → **单向前缀**,见
                // `ContextWindowsSection::lookup`):查询名以配置键为
                // 前缀才算命中;表里写 "Mock" 而活跃模型是 "mock/mock-1"
                // 这种命中会让 per-model 条目(更具体)保持优先。
                let covered = map.keys().any(|k| *k == key || key.starts_with(k.as_str()));
                if !covered {
                    map.insert(key, w);
                }
            }
        }
        map
    }

    /// v1.10 功能3:查某模型**被用户显式配置**的上下文窗口(不含
    /// `context_window_for` 静态回退表):`[context_windows]` 表 lookup
    /// 优先;`[model].context_window` 只作用于调用方传入的(活跃)模型。
    /// `None` = 未配置。消费方:`zhuye-exec::bootstrap_m4` 的窗口感知
    /// 压缩阈值(`compactor_config_with_window`)。
    pub fn configured_context_window_for(&self, model_spec: &str) -> Option<u32> {
        if let Some(v) = self
            .context_windows
            .as_ref()
            .and_then(|cw| cw.lookup(model_spec))
        {
            return Some(v);
        }
        // `[model].context_window` 是「活跃模型」的字段;调用方传的就是
        // 当前模型 spec,直接生效。
        self.model.as_ref().and_then(|m| m.context_window)
    }

    /// v1.10 功能4:`[model].reasoning_effort` 的解析结果(大小写不敏感;
    /// `None` = 未配置或值非法)。返回协议层镜像枚举,由 exec bootstrap
    /// 写入 `AgentConfig::effort` 作为会话初始档位;运行时仍可用
    /// `Op::SetEffort` 覆盖。非法值在这里吞掉 —— 配置加载是宽松口径,
    /// 坏档位不该让整份 config 失效(调用方负责 warn)。
    pub fn resolve_reasoning_effort(&self) -> Option<zhuye_protocol::ReasoningEffortMirror> {
        let raw = self.model.as_ref()?.reasoning_effort.as_deref()?;
        raw.parse().ok()
    }
}

impl Default for ReflectConfig {
    fn default() -> Self {
        Self {
            active: ActiveSection::default(),
            anthropic: None,
            openai: None,
            ollama: None,
            compact: CompactSection::default(),
            token_budget: None,
            sandbox: SandboxSection::default(),
            hooks: HooksSection::default(),
            mcp_servers: McpServersSection::default(),
            lsp_servers: LspServersSection::default(),
            plugins: PluginsSection::default(),
            routing: None,
            context_windows: None,
            model_discovery: None,
            subagent_providers: None,
            coordinator: None,
            sanitize: None,
            prompt_guard: None,
            ask_user_question: None,
            model: None,
            goal: None,
            telemetry: None,
            analytics: None,
            notifications: None,
            postgres_session: None,
            sse_redis: None,
            bridge: None,
            voice: None,
            dap: None,
            acp: None,
            web_search: None,
            permissions: None,
            subagents: Vec::new(),
            config_version: CURRENT_CONFIG_VERSION,
        }
    }
}

// ── Active / 通用 Section ──────────────────────────────────────

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ActiveSection {
    pub provider: Option<String>,
    /// 钉住该 provider 凭证池中的某个条目(`[[<provider>.credentials]]` 的
    /// `label`)。`Some` 且命中池内条目时,该条目健康则始终优先派位,其余
    /// 条目仅在其 cooldown 时作为 failover;`None` = 不钉住,全池加权
    /// round-robin。顶层 `[<provider>].api_key` 隐式条目的 label 固定为
    /// `"default"`,钉它就显式写 `credential = "default"`。
    #[serde(default)]
    pub credential: Option<String>,
    #[serde(default)]
    pub max_iterations: Option<u32>,
    /// 主 agent 工具白名单(`[active] tools`)。非空时 LLM 可见
    /// 工具集与该名单**求交** —— 与 `.zhuye/agents/*.md` frontmatter 的
    /// `tools` 语义一致,只是作用在配置层,让 serve / SDK 接入方无需
    /// 写 agent 文件即可裁掉 16 个内置工具(bash / write / web_search
    /// 等)。空 = 不过滤(向后兼容)。客户端注册的远程工具同样受此
    /// 名单约束(想保留就要显式列出)。
    #[serde(default)]
    pub tools: Vec<String>,
    /// 主 agent 工具黑名单(`[active] disallowed_tools`)。与
    /// agent 定义层的同名列表**取并集**后过滤。空 = 无额外排除。
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    /// 内置 skill 包开关(`[active] bundled_skills`)。**默认 false(关闭)**:
    /// 框架层不为会话注入 15 个编码向内置 skill(code-review / commit-helper
    /// 等),ephemeral 块因此不再每 turn 携带 ~1.4KB 的目录死文本 —— 本仓库
    /// 定位纯框架层,内置技能属应用层偏好,交由使用者显式开启。写
    /// `[active] bundled_skills = true` 恢复合入;workspace / home 的文件
    /// skill(`.zhuye/skills/`)与插件 skill 不受此开关影响,同名时仍以
    /// 文件目录扫描结果优先。
    #[serde(default)]
    pub bundled_skills: bool,
}

// ── 沙箱 / 提问 / 模型 / 特性开关 / 目标 ────

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SandboxSection {
    /// `[sandbox].os_level` —— OS 级沙箱(Seatbelt / Landlock)开关。
    ///
    /// **`Option<bool>` 而不是 `bool`** —— 解析后必须能区分「用户没写」
    /// 与「用户写了 false」:
    /// - `None` = 用户**未表态** → 走调用方的基线(核心 CLI 的基线是
    ///   「默认要求 OS sandbox」,`OsSandbox::default()` = enabled + strict);
    /// - `Some(false)` = 用户**显式关闭** → 尊重表态。
    ///
    /// 此前是 `bool` + `#[serde(default)]`,两者解析后不可区分 —— 想表达
    /// 「未表态但要按基线走」的一方只能去**重新解析原始 TOML** 找 key
    /// 是否存在(桌面端 `app-core/src/sandbox.rs` 此前就是这么绕的)。
    /// 改成 `Option<bool>` 后,「未表态」的语义由 schema 直接承载。
    #[serde(default)]
    pub os_level: Option<bool>,
    #[serde(default)]
    pub writable_dirs: Vec<PathBuf>,
    #[serde(default)]
    pub allow_network: bool,
}

/// `[permissions]` 段 —— 工具权限规则,直接复用 `zhuye_permissions::PermissionRule`
///(`~/.zhuye/permissions.toml` 的同一数据结构)。加载时合并进 permission store,
/// config 规则优先于 file 规则(first-match-wins)。
///
/// 支持两种写法,可并存:
///
/// 1. 显式 `[[permissions.rule]]`(与 `permissions.toml` 一致,字段最全):
///    ```toml
///    [[permissions.rule]]       # 权限规则数组
///    tool = "Bash"               # 工具名
///    action = "allow"            # 动作:allow / deny / ask
///    shell_pattern = "git *"     # 命令 glob 模式
///    ```
///
/// 2. Claude Code 式紧凑数组([`allow`] / [`deny`]),适合批量放行/拒绝:
///    ```toml
///    [permissions]               # 权限段
///    allow = ["Bash", "Edit", "Write", "Read", "Bash(git:*)", "Bash(npm:*)"]   # 放行列表
///    deny  = ["Bash(curl:*)", "Bash(wget:*)"]                                  # 拒绝列表
///    ```
///    `Bash(git:*)` 会解析为 `shell_pattern = "git*"`(见
///    [`permission_syntax::parse_permission_entry`])。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PermissionsSection {
    /// 权限规则列表。`rename = "rule"` 与 `permissions.toml` 的 `[[rule]]` 对齐。
    #[serde(default, rename = "rule")]
    pub rules: Vec<zhuye_permissions::PermissionRule>,
    /// Claude Code 式放行列表。加载时经 [`Self::expanded_rules`] 展平为
    /// `PermissionRule`(action = `Allow`),与 `rules` 合并。
    #[serde(default)]
    pub allow: Vec<String>,
    /// Claude Code 式拒绝列表。加载时展平为 `PermissionRule`(action = `Deny`)。
    /// matcher 对 deny 做全局短路,故展开时置于 allow 之前以确保拒绝优先。
    #[serde(default)]
    pub deny: Vec<String>,
}

impl PermissionsSection {
    /// 把 `deny` + `allow` 紧凑字符串与显式 `rules` 合并成统一的规则列表。
    ///
    /// 展开顺序:**deny → allow → 显式 rules**。
    /// - deny 置于最前:matcher 的「deny 全局短路」(`evaluate_with_context`
    ///   见 `zhuye-permissions/src/matcher.rs`)确保拒绝优先,不受后续 allow 覆盖。
    /// - 显式 `rules` 置于最后:它字段最全(含 `tool_glob` / `shell_pattern`),
    ///   放后面能被 allow/deny 的同工具规则先行短路,但 deny 已在前兜底,
    ///   实际表现为「deny 最强,其余 first-match-wins」。
    ///
    /// 无 `allow` / `deny` 时(向后兼容),返回值等价于原 `rules` 的克隆。
    pub fn expanded_rules(&self) -> Vec<zhuye_permissions::PermissionRule> {
        use zhuye_permissions::PermissionAction;
        let mut out = Vec::with_capacity(self.deny.len() + self.allow.len() + self.rules.len());
        for s in &self.deny {
            if let Some(r) = permission_syntax::parse_permission_entry(s, PermissionAction::Deny) {
                out.push(r);
            }
        }
        for s in &self.allow {
            if let Some(r) = permission_syntax::parse_permission_entry(s, PermissionAction::Allow) {
                out.push(r);
            }
        }
        out.extend(self.rules.iter().cloned());
        out
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AskUserQuestionSection {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub max_questions: Option<u8>,
    #[serde(default)]
    pub max_options: Option<u8>,
    #[serde(default)]
    pub default_timeout_secs: Option<u64>,
}

impl AskUserQuestionSection {
    pub fn resolved(&self) -> Option<ResolvedAskUserQuestion> {
        if !self.enabled.unwrap_or(true) {
            return None;
        }
        Some(ResolvedAskUserQuestion {
            max_questions: self.max_questions.unwrap_or(4).clamp(1, 4),
            max_options: self.max_options.unwrap_or(4).clamp(1, 4),
            default_timeout_secs: self.default_timeout_secs.unwrap_or(900),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedAskUserQuestion {
    pub max_questions: u8,
    pub max_options: u8,
    pub default_timeout_secs: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ModelSection {
    /// 活跃模型的上下文窗口(token)。作用等同于在 `[context_windows]`
    /// 表里给当前模型写一行 —— v1.10 功能3起由 exec bootstrap 合并进
    /// 引擎覆盖表(此前解析后无消费方,是死配置)。
    #[serde(default)]
    pub context_window: Option<u32>,
    /// v1.10 功能4:思考强度默认档(`low` / `medium` / `high` / `max` /
    /// `ultra`)。存原始字符串、加载期不校验(坏值仅 warn 后忽略,不让
    /// 整份配置反序列化失败);消费方(exec bootstrap → `AgentConfig
    /// .set_effort`)用 `ReasoningEffortMirror::from_str` 大小写不敏感解析。
    /// 实际发 wire 前由 `zhuye_llm::reasoning_effort` 按模型族钳制
    /// (qwen→high / deepseek→max / OpenAI→ultra / 未知→high)。
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub input_price_micro_usd_per_mtok: Option<u64>,
    #[serde(default)]
    pub output_price_micro_usd_per_mtok: Option<u64>,
}

impl ModelSection {
    pub fn input_price_usd_per_mtok(&self) -> Option<f64> {
        self.input_price_micro_usd_per_mtok
            .map(|v| v as f64 / 1_000_000.0)
    }
    pub fn output_price_usd_per_mtok(&self) -> Option<f64> {
        self.output_price_micro_usd_per_mtok
            .map(|v| v as f64 / 1_000_000.0)
    }
}

/// `[context_windows]` 段:per-model 上下文窗口覆盖表。
///
/// 优先级:该表 > 内置 `context_window_for` 静态回退表 > `None`。
/// key 不区分大小写(TOML 解析后 `ReflectConfig::context_window_overrides` 会 lowercase)。
///
/// 支持两种 TOML 写法:
/// 1. 扁平 key/value(推荐):
///    ```toml
///    [context_windows]        # 段名
///    "MiniMax-M3" = 1000000   # 模型名 → 上下文 token 数
///    "qwen36-1m" = 1000000    # 模型名 → 上下文 token 数
///    ```
/// 2. 嵌套 `entries` 子段:
///    ```toml
///    [context_windows.entries]   # 嵌套子段
///    "MiniMax-M3" = 1000000   # 模型名 → 上下文 token 数
///    ```
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct ContextWindowsSection {
    pub entries: HashMap<String, u32>,
}

impl<'de> Deserialize<'de> for ContextWindowsSection {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // 先反序列化为 raw HashMap;允许多种形态,最后归并到 `entries`。
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Flat(HashMap<String, u32>),
            Nested {
                #[serde(default)]
                entries: HashMap<String, u32>,
            },
        }
        let raw = Either::deserialize(deserializer)?;
        let entries = match raw {
            Either::Flat(m) => m,
            Either::Nested { entries } => entries,
        };
        Ok(ContextWindowsSection { entries })
    }
}

impl ContextWindowsSection {
    /// 取归一化(去 provider/ollama-tag,小写化)后的窗口大小。
    /// key 完全匹配(归一化后优先)→ **单向前缀**匹配(查询名以配置键
    /// 为前缀;多候选取最长配置键,不随 HashMap 迭代序漂移)→ 首个命中。
    ///
    /// 反方向(配置键比查询名更具体)不匹配:否则查询 `gpt-4` 会命中
    /// 配置 `gpt-4o` 的窗口,把长型号的窗口配给短名模型,自适应输出
    /// 预算按错误窗口放大即超窗。与 `zhuye_llm::context_window_override_for`
    /// 同口径。
    pub fn lookup(&self, model_spec: &str) -> Option<u32> {
        let key = normalize_model_key(model_spec);
        if key.is_empty() {
            return None;
        }
        // 精确匹配(归一化后)。
        if let Some(v) = self.entries.get(&key) {
            return Some(*v);
        }
        // 单向前缀匹配,多候选取最长键(确定性)。
        self.entries
            .iter()
            .filter(|(k, _)| key.starts_with(k.as_str()))
            .max_by_key(|(k, _)| k.len())
            .map(|(_, v)| *v)
    }
}

/// 模型名归一化(与 `zhuye_llm::context_window_for` 对齐):
/// 去 provider 前缀(`anthropic/claude-...` → `claude-...`)、去 ollama tag(`:7b`),
/// 小写化 trim。
fn normalize_model_key(model_spec: &str) -> String {
    let no_provider = model_spec.rsplit_once('/').map_or(model_spec, |(_, m)| m);
    let no_tag = no_provider.split(':').next().unwrap_or(no_provider);
    no_tag.trim().to_ascii_lowercase()
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct GoalSection {
    pub max_turns: Option<u32>,
    pub verification_model: Option<String>,
    pub verify_timeout_seconds: Option<u64>,
}

// ── 脱敏 / 钩子 ──────────────────────────────────────────

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SanitizeSection {
    pub enabled: Option<bool>,
    pub marker: Option<String>,
    pub disable_default_patterns: Option<bool>,
    pub extra_patterns: Option<Vec<String>>,
}

/// v1.x:输入侧提示注入检测(`[prompt_guard]`)。默认关闭(opt-in);
/// 开启后工具输出进入模型上下文前被扫描:`Block` 裁决拦截输出,
/// `Review` 在 metadata 标注。env `ZHUYE_PROMPT_GUARD=1` 可压开。
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PromptGuardSection {
    pub enabled: Option<bool>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct HooksSection {
    pub enabled: Option<Vec<String>>,
    /// v1.x:显式禁用的 hook 名(持久化 `HookEngine::disable` 状态)。
    /// 名称即注册名:内置 hook 用短名(`search_budget` 等),插件 hook 用
    /// `plugin:<插件id>:<事件>#<序号>`。注册后统一 apply,先注册后禁用
    /// 的顺序由 `build_engine` 保证。
    #[serde(default)]
    pub disabled: Option<Vec<String>>,
    #[serde(default)]
    pub search_budget: Option<SearchBudgetSection>,
    #[serde(default)]
    pub test_runner: Option<TestRunnerSection>,
    #[serde(default)]
    pub plan_completion: Option<PlanCompletionSection>,
    #[serde(default)]
    pub verification: Option<VerificationSection>,
    #[serde(default)]
    pub langfuse_tracker: Option<LangfuseSection>,
    #[serde(default)]
    pub read_before_edit: Option<ReadBeforeEditSection>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SearchBudgetSection {
    pub max_calls: Option<u32>,
    pub search_tools: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TestRunnerSection {
    pub enabled: Option<bool>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PlanCompletionSection {
    pub strict: Option<bool>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct VerificationSection {
    pub run_on_stop: Option<bool>,
    pub test_command: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LangfuseSection {
    pub enabled: Option<bool>,
    pub endpoint: Option<String>,
    pub public_key: Option<String>,
    pub secret_key: Option<String>,
    pub export_mode: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReadBeforeEditSection {
    pub enabled: Option<bool>,
    pub mtime_drift_tolerance_ms: Option<u64>,
}

// ── 测试 ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
