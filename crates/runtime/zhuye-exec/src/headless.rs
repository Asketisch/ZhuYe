//! headless 会话 bootstrap —— `zhuye exec` 与 `zhuye serve` 的共享装配层。
//!
//! v1.3 SDK:原 `async_main` 的装配段(config / hooks / registry / 工具 /
//! task / MCP / LSP / plugin / reload)抽取为三个可复用函数,exec 与
//! serve 共用,保证两条入口的行为不再漂移:
//!
//! - [`bootstrap_common`]:config 加载 + tracing + 工具注册表 + 取消令牌
//!   + workspace 解析(两个分支的前半段);
//! - [`bootstrap_normal`]:普通分支后半段(M4/M5 bootstrap、AgentThread、
//!   cron、hooks、MCP/LSP/plugin、reload drainer);
//! - [`bootstrap_resumed`]:resume 分支(回放历史 + 最小依赖)。

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zhuye_config::ConfigWatcher;
use zhuye_core::{AgentConfig, AgentThread};
use zhuye_hooks::builtins::{PlanModeGate, build_read_before_edit};
use zhuye_llm::ModelRegistry;
use zhuye_mcp::McpConnectionManager;
use zhuye_plugin::runtime::{SharedPluginRuntime, bootstrap_plugins};
use zhuye_protocol::{Event, EventMsg, Submission, ThreadId};
use zhuye_task::coordinator::CoordinatorConfig;
use zhuye_tools::{Sanitizer, ToolRegistry, ToolSource};

use crate::bootstrap::{
    self, bootstrap_lsp, bootstrap_m4, bootstrap_m5, bootstrap_m6, bootstrap_resume,
};
use crate::jsonl::JsonlWriter;
use crate::reload::spawn_reload_task;
use crate::runtime_config::{
    apply_coordinator_from_config, build_quota_tracker, build_sanitizer, build_telemetry_sink,
    init_tracing,
};

/// headless bootstrap 的公共参数(exec / serve 各自的 args 投影)。
#[derive(Debug, Clone, Default)]
pub struct HeadlessArgs {
    /// Agent 定义名(默认 `"default"`)。
    pub agent: Option<String>,
    /// 任务 / 团队用内存 store(不落盘)。
    pub ephemeral_tasks: bool,
    pub ephemeral_teams: bool,
    /// 从 cwd 向上探测项目根作为工作区。
    pub auto_root: bool,
    /// 启动即进入 Plan mode(只读)。
    pub plan_mode: bool,
    /// 内置 hook 仅在 `[hooks].enabled` 显式列出时启用(serve 用)。
    /// exec / TUI 保持「未配置 = 全部启用」;serve 是 SDK 嵌入入口,
    /// 默认启用 `verification` 之类 Stop hook 会在宿主 cwd 跑
    /// `cargo test`,对宿主进程既慢又不可预期。
    pub hooks_explicit_only: bool,
    /// 显式插件 id 列表(`serve --plugin <id>` 可多次)。`None` /
    /// 空 = 按 config 的 `[plugins].enabled_plugins` 挂载(默认);
    /// 非空 = **显式覆盖**配置(SDK 嵌入方按需精确加载,不读全局开关)。
    pub plugins: Option<Vec<String>>,
}

/// 解析插件挂载列表:显式 `--plugin` 覆盖优先,否则用 config。
///
/// 注:覆盖只作用于本次启动装配;若运行中 config 的 `[plugins]` 段
/// 发生热重载,同步目标仍是 config 列表(见 `reload.rs`)—— 显式
/// 加载是进程级语义,不回写配置。
pub fn resolve_enabled_plugins(
    args: &HeadlessArgs,
    cfg: &zhuye_config::ReflectConfig,
) -> Vec<String> {
    match args.plugins.as_deref() {
        Some(list) if !list.is_empty() => list.to_vec(),
        _ => cfg.plugins.enabled_plugins.clone(),
    }
}

/// v1.10 功能2:goal + 队列模式的交付插件名。
/// `zhuye plugin install plugins/goal-queue && zhuye plugin enable goal-queue`
/// 后,headless 装配 `goal` / `queue` 工具并启动队列 driver。
pub const GOAL_QUEUE_PLUGIN: &str = "goal-queue";

/// enabled 列表里的条目形如 `goal-queue@inline` / `goal-queue@user`,
/// 取 `@` 前的名字比对。
pub fn goal_queue_plugin_enabled(enabled: &[String]) -> bool {
    enabled
        .iter()
        .any(|id| id.split('@').next() == Some(GOAL_QUEUE_PLUGIN))
}

/// v1.10 功能2:`goal-queue` 插件启用时注册 `queue` / `goal` 工具并启动
/// 队列 driver(事件驱动串行消费,持久化到 `~/.zhuye/queue/<session>.json`)。
/// 未启用时 no-op —— goal 模式的 Op 路径(`Op::EnterGoalMode`)不受影响,
/// 插件只是给 headless / SDK 侧的模型补上工具入口。
#[allow(clippy::too_many_arguments)]
fn wire_goal_queue_plugin(
    enabled: &[String],
    tools: &Arc<ToolRegistry>,
    thread: &Arc<AgentThread>,
    goal_slot: Arc<parking_lot::RwLock<Option<Arc<zhuye_goal::GoalController>>>>,
    thread_id: zhuye_protocol::ThreadId,
) {
    if !goal_queue_plugin_enabled(enabled) {
        return;
    }
    // 队列:完成信号走 per-turn 事件监视(出队时经 `AgentThread::submit`
    // 拿到该 submission 的事件通道,在 TurnComplete / TurnAborted / 通道
    // 关闭时按任务 id 结清)—— Stop hook 覆盖不到绕过 check_stop 的结束
    // 路径(凭证耗尽 / 预算耗尽 / Interrupt),曾致 active 任务永久卡
    // Running。driver 周期 tick 只作兜底。
    let scheduler = {
        let thread_for_queue = Arc::clone(thread);
        let submitter: zhuye_stream::TurnSubmitter = Arc::new(move |sub| {
            let t = Arc::clone(&thread_for_queue);
            Box::pin(async move { Some(t.submit(sub).await.into_receiver()) })
        });
        let mut s = zhuye_stream::QueueScheduler::new(Some(thread.submission_sender()), thread_id)
            .with_turn_submitter(submitter);
        if let Some(base) = zhuye_stream::default_queue_base() {
            s = s.with_store(zhuye_stream::queue_store_path_for(&base, &thread_id));
        }
        Arc::new(s)
    };
    tools.register_with_source(
        ToolSource::Builtin,
        Arc::new(crate::QueueTool::new(scheduler.clone())),
    );
    let queue_driver = (*scheduler).clone().start(2);
    tokio::spawn(async move {
        let _handle = queue_driver;
        std::future::pending::<()>().await;
    });
    // goal:共享 `AgentConfig.goal` 槽读状态;enter/exit 经 submission
    // 通道提交 Op(引擎构造 controller,复用校验/3-strike/预算语义)。
    tools.register_with_source(
        ToolSource::Builtin,
        Arc::new(crate::GoalTool::new(thread.submission_sender(), goal_slot)),
    );
    tracing::info!(session_id = %thread_id, "goal-queue plugin wired (queue + goal tools)");
}

/// [`bootstrap_common`] 的产物:两个分支后续装配都要用的共享句柄。
pub struct HeadlessCommon {
    pub initial_cfg: zhuye_config::ReflectConfig,
    pub config_path: PathBuf,
    pub registry: Arc<ModelRegistry>,
    pub hook_engine: Arc<zhuye_hooks::HookEngine>,
    pub sanitizer: Arc<Sanitizer>,
    /// v1.x:prompt injection guard(`[prompt_guard]` / env opt-in;None = 关闭)。
    pub prompt_guard: Option<Arc<zhuye_sanitize::prompt_guard::PromptGuard>>,
    pub tools: Arc<ToolRegistry>,
    pub task_manager: Arc<zhuye_task::TaskManager>,
    pub cancel: CancellationToken,
    pub workspace: PathBuf,
    /// `"provider/model"` 形式的完整 spec。
    pub model: String,
    pub watcher: ConfigWatcher,
    /// reload / MCP / LSP 生命周期事件的公共通道(消费者由各分支自定)。
    pub reload_tx: mpsc::Sender<Event>,
    /// `reload_tx` 的接收端(normal 分支桥接到 stdout drainer;resumed
    /// 分支 drop 掉,与 exec 历史行为一致)。
    pub(crate) _reload_rx: ReloadRxHandle,
    /// v1.x:`[sandbox]` 段解析出的 OS 沙箱默认开关(env > config > 基线)。
    /// `None` = 配置未表态(每 turn 跟随 env)。
    pub sandbox_default: Option<bool>,
    /// v1.x:`[sandbox].writable_dirs`(workspace 之外额外可写目录)。
    pub sandbox_writable: Vec<PathBuf>,
    /// v1.x:`[sandbox].allow_network`。
    pub sandbox_network: Option<bool>,
}

/// [`bootstrap_normal`] / [`bootstrap_resumed`] 的产物。
pub struct HeadlessSession {
    pub thread: Arc<AgentThread>,
    pub tools: Arc<ToolRegistry>,
    /// resume 分支回放进 thread 的历史消息数(普通分支为 0)。exec 用它
    /// 拼合成 reminder 文案;serve 忽略。
    pub prior_messages: usize,
    /// 运行时插件状态 —— exec / serve 用它在 submit 前展开
    /// `/plugin:ns:name args` 形式的 slash 命令(`expand_user_input`)。
    /// `HOME` 缺失或 plugins 目录不存在时 `Option` 内为 `None`。
    pub plugin_runtime: SharedPluginRuntime,
}

/// 装配公共前半段:config → tracing → 工具注册表 → task manager →
/// 取消令牌 → workspace。与原 `async_main` 步骤 0-3 逐行对应。
pub async fn bootstrap_common(args: &HeadlessArgs) -> anyhow::Result<HeadlessCommon> {
    // 0. 尽早加载配置,让 `init_tracing` 能按 [analytics] 决定是否启用 OTLP。
    let config_path = zhuye_config::default_config_path()
        .ok_or_else(|| anyhow::anyhow!("HOME unset; cannot locate config directory"))?;
    let initial_cfg = zhuye_config::load_default();
    init_tracing(initial_cfg.analytics.as_ref());

    // v1.x:`[sandbox]` 段启动期解析为 OS 沙箱默认开关(env > config >
    // 基线,见 `OsSandbox::resolve`),经 `AgentConfig.sandbox_default` 注入
    // 每 turn —— 取代此前的 `unsafe set_var` 进程级桥接(Rust 2024 下
    // set_var 是 unsafe 且多线程有竞态;且旧桥接只处理 `Some(true)`,
    // `os_level = Some(false)` 用户显式关闭反而无效)。
    let sandbox_cfg = &initial_cfg.sandbox;
    let sandbox_default = (sandbox_cfg.os_level.is_some()
        || !sandbox_cfg.writable_dirs.is_empty()
        || sandbox_cfg.allow_network)
        .then(|| {
            zhuye_sandbox::OsSandbox::resolve(
                sandbox_cfg.os_level,
                sandbox_cfg.writable_dirs.clone(),
                sandbox_cfg.allow_network,
            )
            .enabled
        });
    let sandbox_writable = sandbox_cfg.writable_dirs.clone();
    let sandbox_network = sandbox_cfg.allow_network.then_some(true);
    if let Some(on) = sandbox_default {
        tracing::info!(
            enabled = on,
            writable_dirs = sandbox_cfg.writable_dirs.len(),
            allow_network = sandbox_cfg.allow_network,
            "[sandbox] resolved → AgentConfig.sandbox_default (env 优先)"
        );
    }

    // `[sanitize]` 段接到 queue 的脱敏 pass;失败 → warn + fallback 默认。
    let sanitizer = build_sanitizer(initial_cfg.sanitize.as_ref());

    // v1.x:`[prompt_guard]` / `ZHUYE_PROMPT_GUARD=1` 接输入侧提示注入
    // 检测。默认关闭(opt-in);开启后工具输出进模型上下文前被扫描。
    let prompt_guard_enabled = initial_cfg
        .prompt_guard
        .as_ref()
        .and_then(|s| s.enabled)
        .unwrap_or(false)
        || std::env::var("ZHUYE_PROMPT_GUARD").ok().as_deref() == Some("1");
    let prompt_guard = prompt_guard_enabled.then(|| {
        tracing::info!("[prompt_guard] enabled: 工具输出将在进入模型上下文前扫描");
        Arc::new(zhuye_sanitize::prompt_guard::PromptGuard::new())
    });

    // 从 `~/.zhuye/config.toml [hooks]` 段构建 HookEngine(含 builtin +
    // 插件 hook),AgentThread 与 TaskManager 共享同一份。
    // serve(hooks_explicit_only)把「未配置 = 全部启用」收紧为「未配置
    // = 不启用任何内置 hook」:嵌入场景不应默认跑 verification 之类
    // 会在宿主 cwd 执行 shell 命令的 Stop hook;显式列出的照常启用。
    let mut hooks_cfg = zhuye_hooks::config::HooksConfig::from_reflect_section(&initial_cfg.hooks);
    if args.hooks_explicit_only && hooks_cfg.enabled.is_none() {
        hooks_cfg.enabled = Some(Vec::new());
    }
    let hook_engine: Arc<zhuye_hooks::HookEngine> = Arc::new(hooks_cfg.build_engine());

    // 1. ModelRegistry(env 兜底;v1.3 起 `ZHUYE_MODEL=mock` 可离线注册
    //    mock provider)+ 热重载 watcher。
    let registry = Arc::new(ModelRegistry::new());
    initial_cfg
        .apply_to_registry(&registry)
        .map_err(|e| anyhow::anyhow!("failed to build provider from config: {e}"))?;
    // 无 provider / 无显式 model 都要 fail-fast 且报因可读 —— 不再回落
    // 内置默认模型名(打向第三方兼容端点只会得到难诊断的远端错误)。
    if initial_cfg.active_provider().is_none() {
        anyhow::bail!(
            "no provider configured: set [active] in {} or OPENAI_API_KEY/ANTHROPIC_API_KEY env, or ZHUYE_MODEL=mock",
            config_path.display()
        );
    }
    // `model = "free"` 这类**运行时才知道**的值(从 provider 清单里挑一个
    // 免费模型)在此解析成真实 id —— 之后 `AgentConfig.model` 拿到的永远是
    // 具体模型,事件流 / tracing 里也不会出现 "free" 这种占位。
    let model = zhuye_config::resolve_model_spec(&initial_cfg, &registry)
        .await
        .map_err(|e| anyhow::anyhow!("failed to resolve model: {e}"))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no model configured for the active provider: set [<provider>].model \
                 or a [[<provider>.credentials]].model entry in {} (or ZHUYE_MODEL env)",
                config_path.display()
            )
        })?;

    // v1.10 功能3:`apply_to_registry` 用静态 spec 灌窗口覆盖表;动态模型
    // (`model = "free"`)解析出的真实 id 可能不同 → 在此按最终 spec 重灌,
    // 让 `[model].context_window` 精确键到最终模型。
    zhuye_llm::set_context_window_overrides(
        &initial_cfg.effective_context_window_overrides(&model),
    );

    // v1.5 实战修复:新用户首次运行(HOME 下无 `~/.zhuye/`)时,notify
    // watcher 要求被监听路径存在,否则 bootstrap 直接失败。先确保父目录
    // 存在(与 login 写配置时的目录语义一致)。
    if let Some(parent) = config_path.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).map_err(|e| {
            anyhow::anyhow!("failed to create config dir {}: {e}", parent.display())
        })?;
    }
    let watcher = ConfigWatcher::spawn(config_path.clone(), initial_cfg.clone()).map_err(|e| {
        anyhow::anyhow!(
            "failed to spawn config watcher at {}: {e}",
            config_path.display()
        )
    })?;
    // `ConfigReloaded` 事件的专用 channel;消费者由各分支接。
    let (reload_tx, reload_rx) = mpsc::channel::<Event>(8);
    // 把 reload_rx 存进 HeadlessCommon 之外单独带回 —— 直接作为字段携带
    // Receiver 会让 HeadlessCommon 无法 Clone;这里包成消费端句柄。
    let reload_rx = ReloadRxHandle(reload_rx);

    // 2. 工具注册表(builtin 全量注册 —— 单一事实源
    // `register_builtin_tools`,与 lib facade 共用,勿在此内联追加)。
    let tools = Arc::new(ToolRegistry::default());
    let ask_uq_max = initial_cfg
        .ask_user_question
        .as_ref()
        .and_then(|s| s.resolved())
        .map(|r| r.max_questions as u32);
    zhuye_tools::register_builtin_tools(&tools, ask_uq_max, Arc::new(zhuye_ast::AstTool::new()));

    // task 系统:默认 FileTaskStore 持久化,`--ephemeral-*` / HOME 缺失
    // 时降级内存后端。
    let task_store: Arc<dyn zhuye_task::TaskStore> = if args.ephemeral_tasks {
        Arc::new(zhuye_task::InMemoryTaskStore::default())
    } else {
        match zhuye_task::FileTaskStore::with_default_home() {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "FileTaskStore 初始化失败,降级到 InMemoryTaskStore"
                );
                Arc::new(zhuye_task::InMemoryTaskStore::default())
            }
        }
    };
    let team_store: Arc<dyn zhuye_task::TeamStore> = if args.ephemeral_teams {
        Arc::new(zhuye_task::InMemoryTeamStore::default())
    } else {
        match zhuye_task::FileTeamStore::with_default_home() {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "FileTeamStore 初始化失败,降级到 InMemoryTeamStore"
                );
                Arc::new(zhuye_task::InMemoryTeamStore::default())
            }
        }
    };
    let task_manager = Arc::new(
        zhuye_task::TaskManager::new(task_store, team_store).with_hook_engine(hook_engine.clone()),
    );
    zhuye_task::register_task_tools(&tools, task_manager.clone());
    bootstrap::TOOLS.with(|t| *t.borrow_mut() = Some(tools.clone()));

    // v1.4 D1:tokenizer feature 开启时注册 tiktoken 全局估算器
    // (set-once;失败仅 warn,回退启发式,不阻塞启动)。
    #[cfg(feature = "tokenizer")]
    match zhuye_compact::global_tiktoken_estimator() {
        Ok(est) => {
            if zhuye_compact::set_global_estimator(est) {
                tracing::info!("tiktoken estimator registered (tokenizer feature)");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "tiktoken estimator init failed; using heuristic");
        }
    }

    // 3. Ctrl-C 取消令牌 + workspace 解析。
    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        });
    }
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let workspace = if args.auto_root {
        zhuye_core::detect_project_root(&workspace)
    } else {
        workspace
    };

    Ok(HeadlessCommon {
        initial_cfg,
        config_path,
        registry,
        hook_engine,
        sanitizer,
        prompt_guard,
        tools,
        task_manager,
        cancel,
        workspace,
        model,
        watcher,
        reload_tx,
        _reload_rx: reload_rx,
        sandbox_default,
        sandbox_writable,
        sandbox_network,
    })
}

/// `HeadlessCommon` 内部携带的 reload 接收端句柄。
///
/// `bootstrap_normal` / `bootstrap_resumed` 消费它(桥接到 stdout);
/// `HeadlessCommon` 被 drop 时接收端一并释放,reload task 的发送端
/// send 失败后自然退出,不会泄漏。
pub(crate) struct ReloadRxHandle(pub(crate) mpsc::Receiver<Event>);

/// 装配权限层 + YOLO 分类器(normal 与 resume 分支共用)。
///
/// 优先级(链式 first-match-wins,更具体的作用域在前):
/// config.toml `[permissions]`(会话) > project
/// (`<workspace>/.zhuye/permissions.toml`) > 用户级
/// (`$ZHUYE_HOME/permissions.toml`,默认 `~/.zhuye/permissions.toml`)。
/// `add` / `remove` 委托链尾的用户级
/// file store,运行时写入照旧落盘到用户文件;project 文件手工维护,
/// 随工作区签入。
///
/// v1.5 修复:此前只有 normal 分支装配权限,resume 分支完全绕过
/// v1.x:side-channel thread 工厂 —— 按 agent 名构造**独立** AgentThread。
///
/// 与主会话共用 bootstrap(agent 定义 / 工具 / memory store / policy),
/// 差异两点:
/// - **recorder = None** —— side-channel 不写主会话 rollout(「事件不进
///   主历史」的承诺在这里落地);
/// - **取消令牌由 registry 生成**(参数传入),不是主 token 的子级。
struct SideChannelSecurity {
    permission_resolver: Option<Arc<dyn zhuye_permissions::PermissionResolver>>,
    yolo_classifier: Option<Arc<dyn zhuye_permissions::YoloClassifier>>,
    yolo_threshold: f32,
    parent_mode: Arc<parking_lot::RwLock<zhuye_protocol::PermissionMode>>,
    approvals: bool,
    sandbox_default: Option<bool>,
    sandbox_writable: Vec<std::path::PathBuf>,
    sandbox_network: Option<bool>,
}

fn build_side_channel_factory(
    registry: zhuye_llm::SharedModelRegistry,
    tools: Arc<ToolRegistry>,
    cfg: zhuye_config::ReflectConfig,
    telemetry: Option<Arc<zhuye_telemetry::TelemetrySink>>,
    security: SideChannelSecurity,
) -> zhuye_core::side_channel::SideContextThreadFactory {
    Arc::new(
        move |agent_name: &str,
              cancel: CancellationToken,
              context: zhuye_core::side_channel::SideChannelContext| {
            let workspace = context.workspace;
            let model = context.model;
            // 独立注册表避免侧通道的 session 工具覆盖主会话工具。
            let side_tools = Arc::new(ToolRegistry::new());
            for (name, source) in tools.list_with_source() {
                if let Some(tool) = tools.get(&name) {
                    side_tools.register_with_source(source, tool);
                }
            }
            let mut m4 = bootstrap_m4(
                &workspace,
                &side_tools,
                agent_name,
                &model,
                &registry,
                ThreadId::new(),
                None,
                &cfg,
                telemetry.clone(),
            )
            .ok_or_else(|| {
                zhuye_core::SideChannelStartError::Factory(format!(
                    "side-channel: agent `{agent_name}` bootstrap failed"
                ))
            })?;
            // 不写主历史:摘掉 recorder。
            m4.recorder = None;
            let mut thread_cfg = AgentConfig::new(model.clone(), workspace)
                .with_m4(m4)
                .with_policy(Arc::new(cfg.routing_policy_with(&model)))
                .with_sandbox_default(security.sandbox_default)
                .with_sandbox_writable(security.sandbox_writable.clone())
                .with_sandbox_network(security.sandbox_network)
                .with_approvals(security.approvals)
                // v1.10 功能3/4:side-channel 代理与主会话同口径 ——
                // 合并窗口覆盖表 + 初始思考强度。
                .with_context_window_overrides(cfg.effective_context_window_overrides(&model))
                .with_initial_effort(cfg.resolve_reasoning_effort())
                .with_cancel(cancel);
            thread_cfg.inherited_approval_mode = Some(security.parent_mode.clone());
            thread_cfg.yolo_classifier = security.yolo_classifier.clone();
            thread_cfg.yolo_threshold = security.yolo_threshold;
            if let Some(resolver) = &security.permission_resolver {
                thread_cfg = thread_cfg.with_permission_resolver(resolver.clone());
            }
            let thread = AgentThread::new(
                thread_cfg,
                registry.clone(),
                side_tools,
                Some(build_sanitizer(cfg.sanitize.as_ref())),
                None,
            );
            thread.register_hook(zhuye_core::side_channel::InheritedPlanGate::new(
                security.parent_mode.clone(),
            ));
            Ok(thread)
        },
    )
}

/// 权限规则与 YOLO 分类器。
async fn apply_permission_layers(
    mut cfg: AgentConfig,
    initial_cfg: &zhuye_config::ReflectConfig,
    workspace: &std::path::Path,
) -> AgentConfig {
    let file_store: Arc<dyn zhuye_permissions::PermissionStore> =
        match zhuye_permissions::FilePermissionStore::with_default_home() {
            Ok(store) => Arc::new(store),
            Err(error) => {
                tracing::warn!(%error, "无法确定用户权限文件路径，仍应用会话与项目规则");
                Arc::new(zhuye_permissions::InMemoryPermissionStore::new())
            }
        };
    let cfg_store: Arc<dyn zhuye_permissions::PermissionStore> =
        Arc::new(zhuye_permissions::InMemoryPermissionStore::new());
    if let Some(sec) = &initial_cfg.permissions {
        let rules = sec.expanded_rules();
        for rule in &rules {
            if let Err(e) = cfg_store.add(rule.clone()).await {
                tracing::warn!(error = %e, "permission rule from config.toml 落库失败,跳过");
            }
        }
        tracing::info!(
            rules = rules.len(),
            "loaded permissions rules from config.toml [permissions]"
        );
    }
    // v1.5:project 级权限 —— `<workspace>/.zhuye/permissions.toml`。
    let project_path = workspace.join(".zhuye").join("permissions.toml");
    let project_store: Arc<dyn zhuye_permissions::PermissionStore> = Arc::new(
        zhuye_permissions::FilePermissionStore::with_path(project_path.clone()),
    );
    if let Ok(rules) = project_store.list().await {
        if !rules.is_empty() {
            tracing::info!(
                rules = rules.len(),
                path = %project_path.display(),
                "loaded project permissions from .zhuye/permissions.toml"
            );
        }
    }
    let chained: Arc<dyn zhuye_permissions::PermissionStore> =
        Arc::new(zhuye_permissions::ChainedPermissionStore::new(vec![
            cfg_store,
            project_store,
            file_store,
        ]));
    let resolver: Arc<dyn zhuye_permissions::PermissionResolver> =
        Arc::new(zhuye_permissions::StorePermissionResolver::new(chained));
    cfg = cfg.with_permission_resolver(resolver);
    cfg.yolo_classifier = Some(Arc::new(zhuye_permissions::HeuristicYoloClassifier));
    cfg
}

/// 读取 cron driver tick 间隔;缺省 30 秒,允许测试和运维调小。
fn cron_tick_secs() -> u64 {
    const DEFAULT: u64 = 30;
    std::env::var("ZHUYE_CRON_TICK_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map_or(DEFAULT, |secs: u64| secs.clamp(1, 3600))
}

/// 读取 autopilot driver tick 间隔;缺省 30 秒,允许测试和运维调小
/// (与 `cron_tick_secs` 同款 env 口径)。
fn autopilot_tick_secs() -> u64 {
    const DEFAULT: u64 = 30;
    std::env::var("ZHUYE_AUTOPILOT_TICK_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map_or(DEFAULT, |secs: u64| secs.clamp(1, 3600))
}

/// v1.x:Autopilot 真实调度 driver(`~/.zhuye/autopilot/config.json`
/// opt-in)。注入走 `fire_if_due` + `submission_sender`(与 cron 同款
/// orphan 转发路径,事件同时到 stdout 与终态 watcher);终态回填用
/// 会话订阅里 fire 之后的第一个 turn 终态 / error 事件。
///
/// 简化语义(文档化):autopilot 面向默认会话的无人值守执行;若客户端
/// 回合与注入回合并发,回填可能取到先到者的终态 —— max_concurrent
/// 占位仍在,不会双发。首 tick 延后一个间隔:interval 的首次 tick 在
/// spawn 后立即触发,而 serve 的会话订阅此时可能尚未建立(实测竞态),
/// 且与「配置保存后从下一个 schedule 点开始」语义一致。
fn spawn_autopilot_driver(thread: &Arc<AgentThread>) {
    let manager = zhuye_stream::autopilot::shared_manager(zhuye_config::data_path_or_relative(&[
        "autopilot",
    ]));
    let tx = thread.submission_sender();
    let mut session_rx = thread.subscribe_session();
    let tick = autopilot_tick_secs();
    // fire 与终态回填之间的在途计数:fire 成功注入即 +1;watcher 收到
    // 第一个 turn 终态且计数 >0 时 -1 并回填对应运行(单会话语义下
    // 先到先得,见函数文档的简化说明)。
    let pending: std::sync::Arc<std::sync::Mutex<usize>> =
        std::sync::Arc::new(std::sync::Mutex::new(0));
    let fire_manager = manager.clone();
    let fire_pending = pending.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(tick)).await;
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(tick));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let fired =
                zhuye_stream::autopilot::fire_if_due(&fire_manager, &Some(tx.clone())).await;
            if let Ok(Some(run)) = fired {
                if run.status == zhuye_stream::autopilot::AutopilotRunStatus::InProgress {
                    *fire_pending.lock().expect("pending lock poisoned") += 1;
                }
            }
        }
    });
    // 终态回填:在途计数 >0 时,会话事件流里第一个 turn 终态归本次注入
    // (Error 事件在无终态前出现则记为失败证据,不消耗计数)。
    let watcher_manager = manager;
    let watcher_pending = pending;
    tokio::spawn(async move {
        let mut failure: Option<String> = None;
        while let Some(ev) = session_rx.recv().await {
            match &ev.msg {
                EventMsg::TurnComplete(_) => {
                    if *watcher_pending.lock().expect("pending lock poisoned") == 0 {
                        continue;
                    }
                    *watcher_pending.lock().expect("pending lock poisoned") -= 1;
                    let _ = watcher_manager.finalize_run_by_status(
                        zhuye_stream::autopilot::AutopilotRunStatus::InProgress,
                        if failure.is_none() {
                            zhuye_stream::autopilot::AutopilotRunStatus::Success
                        } else {
                            zhuye_stream::autopilot::AutopilotRunStatus::Failed
                        },
                        failure.take(),
                    );
                    failure = None;
                }
                EventMsg::TurnAborted(_) => {
                    if *watcher_pending.lock().expect("pending lock poisoned") == 0 {
                        continue;
                    }
                    *watcher_pending.lock().expect("pending lock poisoned") -= 1;
                    let _ = watcher_manager.finalize_run_by_status(
                        zhuye_stream::autopilot::AutopilotRunStatus::InProgress,
                        zhuye_stream::autopilot::AutopilotRunStatus::Failed,
                        Some("autopilot turn aborted".into()),
                    );
                    failure = None;
                }
                EventMsg::Error(e) => {
                    failure = Some(e.message.clone());
                }
                _ => {}
            }
        }
    });
    tracing::info!(tick_secs = tick, "autopilot driver started");
}

/// 装配普通分支后半段(与原 `async_main` 普通分支逐行对应),返回
/// 可供 exec / serve 使用的 session。
pub async fn bootstrap_normal(
    common: HeadlessCommon,
    args: &HeadlessArgs,
) -> anyhow::Result<HeadlessSession> {
    let HeadlessCommon {
        initial_cfg,
        config_path,
        registry,
        hook_engine,
        sanitizer,
        prompt_guard,
        tools,
        task_manager,
        cancel,
        workspace,
        model,
        watcher,
        reload_tx,
        _reload_rx: ReloadRxHandle(reload_rx),
        sandbox_default,
        sandbox_writable,
        sandbox_network,
    } = common;

    // M4 bootstrap:agent 定义、skills、压缩器、记忆、prompt 构建器。
    let thread_id = ThreadId::new();
    let telemetry_sink = build_telemetry_sink(&initial_cfg);
    let m4 = bootstrap_m4(
        &workspace,
        &tools,
        args.agent.as_deref().unwrap_or("default"),
        &model,
        &registry,
        thread_id,
        initial_cfg.compact.trigger_tokens,
        &initial_cfg,
        telemetry_sink.clone(),
    )
    .expect("bootstrap_m4 must succeed for the normal (non-resume) path");
    let policy = Arc::new(initial_cfg.routing_policy_with(&model));

    // M5 bootstrap:subagent 工具(call_<role>)。
    let parent_recorder: Arc<dyn zhuye_protocol::RolloutRecorder> = m4
        .recorder
        .clone()
        .unwrap_or_else(|| Arc::new(zhuye_protocol::NullRecorder));
    let factory = bootstrap_m5(
        &workspace,
        args.agent.as_deref().unwrap_or("default"),
        &model,
        &registry,
        thread_id,
        parent_recorder.clone(),
        &m4,
        &initial_cfg,
    );
    factory.set_telemetry(telemetry_sink.clone());
    // v1.4 A1:子代理运行注册表 + 主会话令牌注入 —— 同一 `Arc` 双侧共享
    // (主 cfg 的 `Op::Interrupt { child_id }` 路由侧 / 工厂的 spawn 登记
    // 侧);同时把主会话 Ctrl-C 令牌覆盖进 factory,让子代理的 child_token
    // 挂在会话令牌之下,Shutdown / Ctrl-C 能级联取消在飞子代理。
    let subagent_runtime = Arc::new(zhuye_core::SubagentRuntimeRegistry::new());
    factory.set_runtime_registry(subagent_runtime.clone());
    factory.set_cancel(cancel.clone());

    // Coordinator 模式整合:team spec 注入 factory + 统一开关。
    let coord_enabled = CoordinatorConfig::from_env_or_config(
        initial_cfg
            .coordinator
            .as_ref()
            .unwrap_or(&zhuye_config::CoordinatorSection::default()),
    )
    .enabled;
    if coord_enabled {
        if let Err(e) = task_manager.sync_team_specs(&factory).await {
            tracing::warn!(error = %e, "coordinator: sync_team_specs 失败");
        }
    }
    apply_coordinator_from_config(&initial_cfg, &workspace, Some(&m4), &factory, &tools);

    let skills_for_plugins = m4.skills.clone();
    let token_budget = zhuye_core::config::token_budget_from_env(
        initial_cfg
            .token_budget
            .as_ref()
            .and_then(|s| s.session_total_tokens),
    );
    let max_iterations =
        zhuye_core::config::max_iterations_from_env(initial_cfg.active.max_iterations);
    let quota_tracker = build_quota_tracker(&initial_cfg);
    // v1.5 R4:恢复上次的配额窗口(重启不静默重置;文件缺失 no-op)。
    if let Some(t) = &quota_tracker {
        t.load_state();
    }
    // web_search 工具级 env:`[web_search].api_key` → BRAVE_API_KEY。
    let mut tool_env = std::collections::HashMap::new();
    if let Some(ws) = &initial_cfg.web_search {
        if let Some(key) = &ws.api_key {
            if !key.is_empty() {
                tool_env.insert("BRAVE_API_KEY".to_string(), key.clone());
            }
        }
    }

    let mut cfg = AgentConfig::new(model.clone(), workspace.clone())
        .with_sandbox_default(sandbox_default)
        .with_sandbox_writable(sandbox_writable.clone())
        .with_sandbox_network(sandbox_network)
        .with_m4(m4)
        .with_approval_context(factory.approval_context())
        .with_policy(policy)
        .with_token_budget(token_budget)
        .with_max_iterations(max_iterations)
        .with_quota_tracker(quota_tracker)
        .with_tool_env(tool_env)
        .with_telemetry(telemetry_sink.clone())
        .with_cancel(cancel.clone())
        .with_session_id(thread_id)
        .with_subagent_runtime(subagent_runtime.clone())
        // v1.10 功能3:`[context_windows]` per-model 表 + `[model].context_window`
        // (作用于当前活跃模型)合并后的覆盖表 —— 此前 `[model]` 段的窗口
        // 解析后无消费方,是死配置。
        .with_context_window_overrides(initial_cfg.effective_context_window_overrides(&model))
        // v1.10 功能4:`[model].reasoning_effort` 作为会话初始思考强度
        // (运行时仍可经 `Op::SetEffort` 覆盖;发 wire 前按模型族钳制)。
        .with_initial_effort(initial_cfg.resolve_reasoning_effort())
        .with_initial_permission_mode(if args.plan_mode {
            zhuye_protocol::PermissionMode::Plan
        } else {
            zhuye_protocol::PermissionMode::Auto
        });
    cfg = apply_permission_layers(cfg, &initial_cfg, &workspace).await;
    // v1.x:prompt injection guard 注入(None = 关闭;子代理 factory
    // 自行读同一 config,这里只接主会话与 resume 路径)。
    cfg.prompt_guard = prompt_guard.clone();
    // 侧通道复用主会话策略,避免独立执行绕过权限规则。
    let side_channel_driver = zhuye_core::side_channel::SideChannelDriver::new_with_context(
        zhuye_core::side_channel::SideChannelRegistry::new(),
        cfg.workspace.clone(),
        cfg.model.clone(),
        build_side_channel_factory(
            registry.clone(),
            tools.clone(),
            initial_cfg.clone(),
            telemetry_sink.clone(),
            SideChannelSecurity {
                permission_resolver: cfg.permission_resolver.clone(),
                yolo_classifier: cfg.yolo_classifier.clone(),
                yolo_threshold: cfg.yolo_threshold,
                parent_mode: cfg.permission_mode.clone(),
                approvals: cfg.approvals,
                sandbox_default,
                sandbox_writable: sandbox_writable.clone(),
                sandbox_network,
            },
        ),
    );
    cfg = cfg.with_side_channel_driver(side_channel_driver);
    factory.set_parent_config(cfg.clone());
    let cfg_for_reload = cfg.clone(); // 共享 model Arc<RwLock>
    // v1.10 功能2:goal 槽的共享 Arc 先取出(构造 GoalTool 用,避免
    // 工具持有 AgentThread 造成引用环)。
    let goal_slot = cfg.goal.clone();
    let thread = Arc::new(AgentThread::new(
        cfg,
        registry.clone(),
        tools.clone(),
        Some(sanitizer.clone()),
        Some(hook_engine.clone()),
    ));
    // v1.x:Activity 审计(WP12)—— 会话事件沉淀到
    // `~/.zhuye/activity/activity.jsonl`。独立 receiver,与 stdout
    // drainer 并行互不阻塞;`[telemetry].activity=false` 关闭。
    if initial_cfg
        .telemetry
        .as_ref()
        .is_none_or(|t| t.activity_enabled())
    {
        let logger = zhuye_telemetry::ActivityLogger::with_default_home();
        let rx = thread.subscribe_session();
        tokio::spawn(zhuye_telemetry::pump_events(logger, rx));
    }

    // Cron 真实调度:到期把 job.prompt 作为 user_input 注入。
    // v1.x:启用持久化(`~/.zhuye/cron/<session>.json`)—— resume 同一
    // thread id 时 job 恢复,进程退出不再丢。坏文件不覆盖、起步为空。
    let cron_scheduler = {
        let mut scheduler =
            zhuye_stream::CronScheduler::new(Some(thread.submission_sender()), thread_id);
        if let Some(base) = zhuye_stream::default_cron_base() {
            scheduler = scheduler.with_store(zhuye_stream::store_path_for(&base, &thread_id));
        }
        Arc::new(scheduler)
    };
    tools.register_with_source(
        ToolSource::Builtin,
        Arc::new(crate::CronTool::new(cron_scheduler.clone())),
    );
    let cron_driver = (*cron_scheduler).clone().start(cron_tick_secs());
    tokio::spawn(async move {
        let _handle = cron_driver;
        std::future::pending::<()>().await;
    });
    tracing::info!(session_id = %thread_id, tick_secs = cron_tick_secs(), "cron scheduler started");

    // v1.x:Autopilot 真实调度(config.json opt-in;到期注入真实 turn)。
    spawn_autopilot_driver(&thread);

    // Plan mode hooks:PlanModeGate + read_before_edit。
    thread.register_hook(PlanModeGate::default_mode());
    let rbe_section = initial_cfg.hooks.read_before_edit.as_ref();
    let (_rbe_state, rbe_hook) = build_read_before_edit(
        rbe_section.and_then(|s| s.enabled),
        rbe_section.and_then(|s| s.mtime_drift_tolerance_ms),
    );
    thread.register_hook(rbe_hook);
    tracing::info!(
        "registered read_before_edit hook (enabled={:?}, drift_ms={:?})",
        rbe_section.and_then(|s| s.enabled),
        rbe_section.and_then(|s| s.mtime_drift_tolerance_ms),
    );

    if args.plan_mode {
        tracing::info!("exec started with --plan-mode; only read-only tools are usable");
    }

    // MCP / LSP bootstrap(失败仅 warn,不阻塞主流程)。
    let mcp_manager = bootstrap_m6(&initial_cfg, reload_tx.clone()).await;
    let _lsp_manager = bootstrap_lsp(&initial_cfg, reload_tx.clone(), tools.clone()).await;

    // 插件 runtime —— 挂载 enabled_plugins 能力。
    let mcp_for_plugins = mcp_manager.clone().unwrap_or_else(|| {
        let (tx, _rx) = tokio::sync::mpsc::channel::<zhuye_mcp::McpLifecycleEvent>(16);
        Arc::new(McpConnectionManager::new(tx))
    });
    tools.register(Arc::new(zhuye_mcp::ListMcpResourcesTool::new(
        mcp_for_plugins.clone(),
    )));
    tools.register(Arc::new(zhuye_mcp::ReadMcpResourceTool::new(
        mcp_for_plugins.clone(),
    )));
    // v1.6:MCP Prompts 原语(与 Resources 对齐的第三类原语)。
    tools.register(Arc::new(zhuye_mcp::ListMcpPromptsTool::new(
        mcp_for_plugins.clone(),
    )));
    tools.register(Arc::new(zhuye_mcp::GetMcpPromptTool::new(
        mcp_for_plugins.clone(),
    )));

    let enabled_plugins = resolve_enabled_plugins(args, &initial_cfg);
    let plugin_runtime = bootstrap_plugins(
        tools.clone(),
        thread.hook_engine(),
        mcp_for_plugins,
        skills_for_plugins,
        factory.clone(),
        &enabled_plugins,
        Some(reload_tx.clone()),
    )
    .await;
    // B4:用户级 slash 命令目录(workspace + 用户级),与 plugin 命令
    // 同表共存(裸名 vs `<plugin>:` 前缀名)。
    let user_cmd_dirs = vec![
        thread
            .config()
            .current_workspace()
            .join(".zhuye")
            .join("commands"),
        zhuye_config::data_path(&[]).map_or_default(|d| d.join("commands")),
    ];
    // SharedPluginRuntime 是锁包装:快照内部句柄尽快释放(展开范式同款)。
    let user_cmds = {
        let guard = plugin_runtime.lock().await;
        match guard.as_ref() {
            Some(rt) => rt.register_user_commands(&user_cmd_dirs),
            None => 0,
        }
    };
    if user_cmds > 0 {
        tracing::info!(count = user_cmds, "user slash commands mounted");
    }

    // v1.10 功能2:goal + 队列模式(goal-queue 插件门控;未启用 no-op)。
    wire_goal_queue_plugin(
        &enabled_plugins,
        &tools.clone(),
        &thread,
        goal_slot,
        thread_id,
    );

    // reload task + `ConfigReloaded` drainer(转发到 stdout JSONL)。
    spawn_reload_task(
        registry.clone(),
        watcher,
        reload_tx,
        config_path.clone(),
        initial_cfg,
        cfg_for_reload,
        Some(factory),
        Some(parent_recorder),
        mcp_manager,
        tools.clone(),
        Some(plugin_runtime.clone()),
        Some(hook_engine.clone()),
    );
    let (sync_tx, sync_rx) = std::sync::mpsc::channel::<Event>();
    std::thread::spawn(move || {
        // writer 必须逐事件构造:`stdout().lock()` 的锁随 writer 存活,
        // 若在此线程持久持有,serve 主 writer task 对 stdout 锁的竞争
        // 会永久阻塞 —— serve stdout 从此零输出(实测回归)。本线程
        // 低频(config-reload 事件),逐条拿锁开销可接受。
        for event in sync_rx {
            let mut w = JsonlWriter::new(std::io::stdout().lock());
            if let Err(e) = w.write_event(&event) {
                tracing::warn!(error = %e, "config-reload jsonl write failed; dropping");
                break;
            }
        }
        tracing::debug!("config-reload drainer thread exited");
    });
    tokio::spawn(async move {
        let mut rx = reload_rx;
        while let Some(event) = rx.recv().await {
            if sync_tx.send(event).is_err() {
                break;
            }
        }
    });

    Ok(HeadlessSession {
        thread,
        tools,
        prior_messages: 0,
        plugin_runtime,
    })
}

/// 装配 resume 分支:回放历史消息到全新 AgentThread,并与普通分支共用
/// 完整 M4/M5 装配 + 插件挂载。
///
/// 历史 resume 分支为了最小依赖用了测试辅助 `default_m4_deps`(空壳
/// skills catalog + "You are a test agent." system prompt)且不带
/// factory / plugin,导致续跑会话丢失 skills / 子代理 / 插件能力。
/// 现在 resume 与普通分支同权:M4 真实装配(recorder 仍续写原
/// session)、SubAgentFactory 在位、`enabled_plugins` 挂载且
/// `[plugins]` 段热重载生效。
pub async fn bootstrap_resumed(
    common: HeadlessCommon,
    args: &HeadlessArgs,
    thread_id_str: &str,
) -> anyhow::Result<HeadlessSession> {
    let HeadlessCommon {
        initial_cfg,
        config_path,
        registry,
        hook_engine,
        sanitizer,
        prompt_guard,
        tools,
        task_manager: _task_manager,
        cancel,
        workspace,
        model,
        watcher,
        reload_tx,
        _reload_rx: ReloadRxHandle(reload_rx),
        sandbox_default,
        sandbox_writable,
        sandbox_network,
    } = common;

    let bundle = bootstrap_resume(thread_id_str).await?;
    let thread_id = bundle.thread_id;
    let telemetry_sink = build_telemetry_sink(&initial_cfg);
    // 与普通分支相同的 M4 装配:真实 skills 扫描 + 记忆 + 压缩器 + note
    // 落盘。`bootstrap_m4` 内部按 thread_id 续写同一 JSONL,与旧 resume
    // 的 `resume_recorder` 等价。
    let m4 = bootstrap_m4(
        &workspace,
        &tools,
        args.agent.as_deref().unwrap_or("default"),
        &model,
        &registry,
        thread_id,
        initial_cfg.compact.trigger_tokens,
        &initial_cfg,
        telemetry_sink.clone(),
    )
    .expect("bootstrap_m4 must succeed for the resume path");
    let resume_recorder: Arc<dyn zhuye_protocol::RolloutRecorder> = m4
        .recorder
        .clone()
        .unwrap_or_else(|| Arc::new(zhuye_protocol::NullRecorder));
    let skills_for_plugins = m4.skills.clone();
    // M5:子代理工厂 —— 插件 agents 能力与内置 subagent spec 都落在这里。
    let factory = bootstrap_m5(
        &workspace,
        args.agent.as_deref().unwrap_or("default"),
        &model,
        &registry,
        thread_id,
        resume_recorder.clone(),
        &m4,
        &initial_cfg,
    );
    factory.set_telemetry(telemetry_sink.clone());
    factory.set_cancel(cancel.clone());
    let cfg = AgentConfig::new(model.clone(), workspace.clone())
        .with_sandbox_default(sandbox_default)
        .with_sandbox_writable(sandbox_writable.clone())
        .with_sandbox_network(sandbox_network)
        .with_m4(m4)
        .with_approval_context(factory.approval_context())
        .with_policy(Arc::new(initial_cfg.routing_policy_with(&model)))
        .with_telemetry(telemetry_sink.clone())
        .with_cancel(cancel.clone())
        // 回填 session_id:submission_loop 每轮 `rec.replay(cfg.session_id)`
        // 重建历史 —— 未设置时是随机新 uuid,永远命不中原文件,恢复的
        // 历史进不了模型上下文(跨轮失忆)。三处 id(文件名 /
        // session_meta.session_id / 此处)对齐后,历史稳定进入每轮 base。
        .with_session_id(thread_id)
        // v1.10 功能3/4:与 normal 分支同口径 —— 合并后的窗口覆盖表 +
        // 初始思考强度。
        .with_context_window_overrides(initial_cfg.effective_context_window_overrides(&model))
        .with_initial_effort(initial_cfg.resolve_reasoning_effort())
        .with_preload_messages(bundle.initial_messages.clone());
    // 与 normal 分支同权:权限三层装配(config > project > user)+ YOLO
    // 分类器。此前 resume 分支漏装,resume 会话会绕过权限规则。
    let mut cfg = apply_permission_layers(cfg, &initial_cfg, &workspace).await;
    // v1.x:prompt injection guard 注入(resume 路径与新会话一致)。
    cfg.prompt_guard = prompt_guard.clone();
    factory.set_parent_config(cfg.clone());
    let side_channel_driver = zhuye_core::side_channel::SideChannelDriver::new_with_context(
        zhuye_core::side_channel::SideChannelRegistry::new(),
        cfg.workspace.clone(),
        cfg.model.clone(),
        build_side_channel_factory(
            registry.clone(),
            tools.clone(),
            initial_cfg.clone(),
            telemetry_sink.clone(),
            SideChannelSecurity {
                permission_resolver: cfg.permission_resolver.clone(),
                yolo_classifier: cfg.yolo_classifier.clone(),
                yolo_threshold: cfg.yolo_threshold,
                parent_mode: cfg.permission_mode.clone(),
                approvals: cfg.approvals,
                sandbox_default,
                sandbox_writable: sandbox_writable.clone(),
                sandbox_network,
            },
        ),
    );
    let cfg = cfg.with_side_channel_driver(side_channel_driver);
    // 0.5:resume 分支此前完全忽略 `--plan-mode` —— 既不设
    // `PermissionMode::Plan` 也不注册 `PlanModeGate`,于是
    // `zhuye exec --plan-mode --resume <id>` 直接是 Auto 模式 + 无
    // Plan gate,写工具全开。始终传 `--plan-mode` 的脚本调用方只要同时
    // resume 就静默拿到完整写权限。与 normal 分支对齐。
    let cfg = if args.plan_mode {
        cfg.with_initial_permission_mode(zhuye_protocol::PermissionMode::Plan)
    } else {
        cfg
    };
    let cfg_for_reload = cfg.clone();
    // v1.10 功能2:goal 槽共享 Arc(同 normal 分支)。
    let goal_slot = cfg.goal.clone();
    let thread = Arc::new(AgentThread::new(
        cfg,
        registry.clone(),
        tools.clone(),
        Some(sanitizer.clone()),
        Some(hook_engine.clone()),
    ));
    // v1.x:Activity 审计(resume 分支与 normal 同权;开关同 [telemetry].activity)。
    if initial_cfg
        .telemetry
        .as_ref()
        .is_none_or(|t| t.activity_enabled())
    {
        let logger = zhuye_telemetry::ActivityLogger::with_default_home();
        let rx = thread.subscribe_session();
        tokio::spawn(zhuye_telemetry::pump_events(logger, rx));
    }
    // v1.x:resume 分支补齐 CronTool + 持久化(此前只在 normal 分支装配,
    // resume 会话里 cron 工具不存在 → "tool not found";持久化 store 也
    // 没接,job 跨重启恢复不了)。store 以 thread_id 命名,与 normal 分支
    // 同一文件 → 同一 session 的 job 跨重启可见。
    {
        let mut scheduler =
            zhuye_stream::CronScheduler::new(Some(thread.submission_sender()), thread_id);
        if let Some(base) = zhuye_stream::default_cron_base() {
            scheduler = scheduler.with_store(zhuye_stream::store_path_for(&base, &thread_id));
        }
        tools.register_with_source(
            ToolSource::Builtin,
            Arc::new(crate::CronTool::new(Arc::new(scheduler.clone()))),
        );
        let cron_driver = scheduler.start(cron_tick_secs());
        tokio::spawn(async move {
            let _handle = cron_driver;
            std::future::pending::<()>().await;
        });
        tracing::info!(session_id = %thread_id, tick_secs = cron_tick_secs(), "cron scheduler started (resume)");

        // v1.x:resume 路径同样接 Autopilot(与 cron 对齐)。
        spawn_autopilot_driver(&thread);
    }
    // 0.5:Plan mode 的执行机制是 hook,不是 permission mode。normal 分支
    // 注册的 PlanModeGate / read_before_edit 在 resume 分支也要有。
    if args.plan_mode {
        thread.register_hook(PlanModeGate::default_mode());
        let rbe_section = initial_cfg.hooks.read_before_edit.as_ref();
        let (_rbe_state, rbe_hook) = build_read_before_edit(
            rbe_section.and_then(|s| s.enabled),
            rbe_section.and_then(|s| s.mtime_drift_tolerance_ms),
        );
        thread.register_hook(rbe_hook);
        tracing::info!("resume path: plan_mode=true, registered PlanModeGate + read_before_edit");
    }
    // resume 也跑 MCP bootstrap,方便继续使用之前挂的 MCP server。
    let mcp_manager = bootstrap_m6(&initial_cfg, reload_tx.clone()).await;
    let _lsp_manager = bootstrap_lsp(&initial_cfg, reload_tx.clone(), tools.clone()).await;
    // 插件 runtime —— resume 与普通分支同权:挂载 enabled_plugins。
    // resume 分支历史上没有 reload drainer(事件静默丢弃),插件
    // `PluginLoaded` 事件同样无消费者,传 `None` 静默。
    let mcp_for_plugins = mcp_manager.clone().unwrap_or_else(|| {
        let (tx, _rx) = tokio::sync::mpsc::channel::<zhuye_mcp::McpLifecycleEvent>(16);
        Arc::new(McpConnectionManager::new(tx))
    });
    let plugin_runtime = bootstrap_plugins(
        tools.clone(),
        hook_engine.clone(),
        mcp_for_plugins,
        skills_for_plugins,
        factory.clone(),
        &resolve_enabled_plugins(args, &initial_cfg),
        None,
    )
    .await;
    // B4:resume 路径同样挂用户级命令目录(见 bootstrap_normal 注释)。
    let user_cmd_dirs = vec![
        thread
            .config()
            .current_workspace()
            .join(".zhuye")
            .join("commands"),
        zhuye_config::data_path(&[]).map_or_default(|d| d.join("commands")),
    ];
    let user_cmds = {
        let guard = plugin_runtime.lock().await;
        match guard.as_ref() {
            Some(rt) => rt.register_user_commands(&user_cmd_dirs),
            None => 0,
        }
    };
    if user_cmds > 0 {
        tracing::info!(count = user_cmds, "user slash commands mounted (resume)");
    }

    // v1.10 功能2:resume 分支与 normal 同权(goal-queue 插件门控)。
    wire_goal_queue_plugin(
        &resolve_enabled_plugins(args, &initial_cfg),
        &tools.clone(),
        &thread,
        goal_slot,
        thread_id,
    );
    spawn_reload_task(
        registry.clone(),
        watcher,
        reload_tx,
        config_path.clone(),
        initial_cfg,
        cfg_for_reload,
        Some(factory),
        Some(resume_recorder),
        mcp_manager,
        tools.clone(),
        Some(plugin_runtime.clone()),
        Some(thread.hook_engine()),
    );
    // resume 分支历史上没有 reload drainer(事件静默丢弃),保持原行为:
    // 这里同样不桥接 reload_rx,drop 即让 reload task 的发送端自然退出。
    drop(reload_rx);

    Ok(HeadlessSession {
        thread,
        tools,
        prior_messages: bundle.initial_messages.len(),
        plugin_runtime,
    })
}

/// 供 exec / serve 复用的 resume 三选一解析(`--resume` / `-c` / `-r`)。
pub fn resolve_resume_thread_id(
    resume: Option<&str>,
    continue_last: bool,
    resume_by: Option<usize>,
) -> anyhow::Result<Option<String>> {
    if let Some(s) = resume {
        return Ok(Some(s.to_string()));
    }
    if continue_last || resume_by.is_some() {
        let base = zhuye_rollout::path::default_base();
        let tid = zhuye_rollout::index::resolve_session_index(&base, continue_last, resume_by)?;
        return Ok(Some(tid.to_string()));
    }
    Ok(None)
}

/// 内置 `/prompt` slash 分流(v1.6.1,提示词层管理)。
///
/// - `/prompt` 或 `/prompt list` → `Op::PromptCommand { List }`,引擎回
///   `EventMsg::PromptList`(可用层 + 生效层);
/// - `/prompt reload`(或 `/prompt rescan`)→ `Switch { layer: None }`,
///   引擎按启动目录序重扫提示词库,回 `EventMsg::PromptSwitched`;
/// - `/prompt <其他词>` → 同 Switch,词作为 action 名透传(预留聚焦切换);
/// - 不以 `/prompt` 开头(含 `/prompts` 等相近词)→ 返回 `None`,调用方
///   继续走插件展开 / 普通 prompt。
///
/// 与插件命令的关系:本命令是引擎内置(协议级),优先于插件展开;
/// TUI/GUI 也可不经 slash 直接构造 `Op::PromptCommand` 提交。
pub fn expand_prompt_slash(prompt: &str) -> Option<Submission> {
    let rest = prompt.trim().strip_prefix("/prompt")?;
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return None;
    }
    let arg = rest.split_whitespace().next().unwrap_or("list");
    let action = match arg {
        "list" | "" => zhuye_protocol::PromptAction::List,
        "reload" | "rescan" => zhuye_protocol::PromptAction::Switch { layer: None },
        other => zhuye_protocol::PromptAction::Switch {
            layer: Some(other.to_string()),
        },
    };
    Some(
        Submission::with_id(
            uuid::Uuid::new_v4().to_string(),
            zhuye_protocol::Op::PromptCommand { action },
        )
        .with_source_command("prompt"),
    )
}

/// 给 `bootstrap_resumed` 用的合成 prompt(与 exec 历史行为一致)。
pub fn resumed_prompt(thread_id: &str, prior_messages: usize) -> String {
    format!(
        "<system-reminder>Resumed session {thread_id} ({prior_messages} prior messages)</system-reminder>\n\nContinue."
    )
}

/// 展开用户输入中的插件 slash 命令(`/plugin:ns:name args`)并构造
/// Submission。
///
/// - 输入不以 `/` 开头或未命中任何已挂载命令 → 原样构造
///   `Submission::user_input`(直通,不影响普通 prompt);
/// - 命中 → 以命令 md 正文(剥 frontmatter + `$ARGUMENTS`/`$1`..`$9`
///   替换)替换输入,并在 `Submission::source_command` 标注命令名;
/// - 命中但命令文件读取失败 → 返回 `Err`(exec 直接报错退出;serve
///   侧调用方选择降级为原样转发)。
pub async fn expand_plugin_command(
    plugin_runtime: &SharedPluginRuntime,
    prompt: String,
) -> anyhow::Result<Submission> {
    // 先快照命令注册表并尽快释放 runtime 锁 —— 展开期间的文件 IO 不占锁。
    let registry = {
        let guard = plugin_runtime.lock().await;
        match guard.as_ref() {
            Some(rt) => rt.commands(),
            None => return Ok(Submission::user_input(prompt)),
        }
    };
    match zhuye_plugin::expand_user_input(&prompt, &registry) {
        None => Ok(Submission::user_input(prompt)),
        Some(Ok(expanded)) => {
            tracing::info!(command = %expanded.name, "plugin command expanded");
            Ok(Submission::user_input(expanded.body).with_source_command(expanded.name))
        }
        Some(Err(e)) => {
            // split_whitespace 自带按首尾空白裁切,无需先 trim。
            let name = prompt
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_start_matches('/');
            Err(anyhow::anyhow!("插件命令 `{name}` 展开失败: {e}"))
        }
    }
}

// ── v1.7:CLI 图片输入(`--image`)────────────────────────────────

/// 单张图片读取上限(10 MiB),与 `image_view` 工具的限额一致 ——
/// 防止超大图片撑爆请求体 / token 预算。
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// 按扩展名推断图片 MIME(仅支持常见栅格格式)。返回 `None` 表示
/// 不支持的格式。
pub fn image_mime_for_path(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

/// 读取本地图片文件为 `UserInputItem::Image`(数据内联,core 已接线到
/// LLM Image block)。扩展名不支持 / 文件不可读 / 超过
/// [`MAX_IMAGE_BYTES`] → `Err`(exec 报错退出)。
pub fn load_image_item(path: &str) -> anyhow::Result<zhuye_protocol::UserInputItem> {
    let mime = image_mime_for_path(path)
        .ok_or_else(|| anyhow::anyhow!("不支持的图片格式(仅 png/jpeg/gif/webp/bmp): {path}"))?;
    let data = std::fs::read(path).map_err(|e| anyhow::anyhow!("读取图片失败 {path}: {e}"))?;
    if data.len() > MAX_IMAGE_BYTES {
        anyhow::bail!(
            "图片超过大小上限({} bytes > {}): {path}",
            data.len(),
            MAX_IMAGE_BYTES
        );
    }
    Ok(zhuye_protocol::UserInputItem::Image {
        data,
        mime_type: mime.to_string(),
    })
}

#[cfg(test)]
mod side_channel_security_tests {
    use super::*;

    #[tokio::test]
    async fn side_channel_inherits_classifier_and_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let classifier: Arc<dyn zhuye_permissions::YoloClassifier> =
            Arc::new(zhuye_permissions::HeuristicYoloClassifier);
        let workspace = Arc::new(parking_lot::RwLock::new(dir.path().to_path_buf()));
        let factory = build_side_channel_factory(
            Arc::new(ModelRegistry::new()),
            Arc::new(ToolRegistry::new()),
            zhuye_config::ReflectConfig::default(),
            None,
            SideChannelSecurity {
                permission_resolver: None,
                yolo_classifier: Some(classifier.clone()),
                yolo_threshold: 0.93,
                parent_mode: Arc::new(parking_lot::RwLock::new(
                    zhuye_protocol::PermissionMode::Auto,
                )),
                approvals: true,
                sandbox_default: None,
                sandbox_writable: Vec::new(),
                sandbox_network: None,
            },
        );
        let thread = factory(
            "default",
            CancellationToken::new(),
            zhuye_core::side_channel::SideChannelContext {
                workspace: workspace.read().clone(),
                model: "mock".into(),
            },
        )
        .unwrap();
        assert!(Arc::ptr_eq(
            thread.config().yolo_classifier.as_ref().unwrap(),
            &classifier
        ));
        assert_eq!(thread.config().yolo_threshold, 0.93);
        assert!(thread.config().approvals);
        let switched = dir.path().join("switched");
        std::fs::create_dir(&switched).unwrap();
        *workspace.write() = switched.clone();
        let side = factory(
            "default",
            CancellationToken::new(),
            zhuye_core::side_channel::SideChannelContext {
                workspace: workspace.read().clone(),
                model: "mock/new".into(),
            },
        )
        .unwrap();
        assert_eq!(side.config().current_workspace(), switched);
        assert_eq!(side.config().current_model(), "mock/new");
        *workspace.write() = dir.path().to_path_buf();
        assert_eq!(side.config().current_workspace(), switched);
    }
}

#[cfg(test)]
mod image_tests {
    use super::*;

    /// 1×1 透明 PNG(最小合法文件,89 字节)。
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn mime_detection_by_extension() {
        assert_eq!(image_mime_for_path("a.png"), Some("image/png"));
        assert_eq!(image_mime_for_path("B.PNG"), Some("image/png"));
        assert_eq!(image_mime_for_path("c.jpg"), Some("image/jpeg"));
        assert_eq!(image_mime_for_path("d.jpeg"), Some("image/jpeg"));
        assert_eq!(image_mime_for_path("e.gif"), Some("image/gif"));
        assert_eq!(image_mime_for_path("f.webp"), Some("image/webp"));
        assert_eq!(image_mime_for_path("g.bmp"), Some("image/bmp"));
        assert_eq!(image_mime_for_path("h.txt"), None);
        assert_eq!(image_mime_for_path("noext"), None);
        assert_eq!(image_mime_for_path("i.svg"), None, "svg 不在内联白名单");
    }

    #[test]
    fn loads_png_into_image_item() {
        let dir = std::env::temp_dir().join(format!("zhuye-exec-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.png");
        std::fs::write(&p, TINY_PNG).unwrap();
        let item = load_image_item(p.to_str().unwrap()).unwrap();
        match item {
            zhuye_protocol::UserInputItem::Image { data, mime_type } => {
                assert_eq!(data, TINY_PNG);
                assert_eq!(mime_type, "image/png");
            }
            other => panic!("应为 Image item,得到 {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_unsupported_and_missing() {
        assert!(load_image_item("/nonexistent/x.txt").is_err());
        let dir = std::env::temp_dir().join(format!("zhuye-exec-img2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.txt");
        std::fs::write(&p, b"hello").unwrap();
        assert!(load_image_item(p.to_str().unwrap()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_oversized_image() {
        // 构造一个"扩展名合法但超限"的场景:用 png 扩展名写 11MiB 数据。
        let dir = std::env::temp_dir().join(format!("zhuye-exec-img3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("big.png");
        std::fs::write(&p, vec![0u8; MAX_IMAGE_BYTES + 1]).unwrap();
        let err = load_image_item(p.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("超过大小上限"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_input_items_submission_carries_images() {
        let sub = zhuye_protocol::Submission::user_input_items(vec![
            zhuye_protocol::UserInputItem::Image {
                data: TINY_PNG.to_vec(),
                mime_type: "image/png".into(),
            },
            zhuye_protocol::UserInputItem::Text {
                text: "看这张图".into(),
            },
        ]);
        match &sub.op {
            zhuye_protocol::Op::UserInput { items, .. } => {
                assert_eq!(items.len(), 2);
                assert!(matches!(
                    items[0],
                    zhuye_protocol::UserInputItem::Image { .. }
                ));
            }
            other => panic!("应为 UserInput op,得到 {other:?}"),
        }
    }
}

#[cfg(test)]
mod prompt_slash_tests {
    use super::*;

    /// `/prompt` 分流:List / reload / 透传词 / 非 /prompt 前缀不命中。
    #[test]
    fn expand_prompt_slash_parses_actions() {
        // 裸 `/prompt` 与 `/prompt list` → List。
        for input in ["/prompt", "/prompt  ", "/prompt list"] {
            let sub = expand_prompt_slash(input).expect("should match");
            assert_eq!(sub.source_command.as_deref(), Some("prompt"));
            match sub.op {
                zhuye_protocol::Op::PromptCommand {
                    action: zhuye_protocol::PromptAction::List,
                } => {}
                other => panic!("expected List, got {other:?}"),
            }
        }
        // `/prompt reload` → Switch { layer: None }。
        let sub = expand_prompt_slash("/prompt reload").expect("should match");
        match sub.op {
            zhuye_protocol::Op::PromptCommand {
                action: zhuye_protocol::PromptAction::Switch { layer: None },
            } => {}
            other => panic!("expected Switch(None), got {other:?}"),
        }
        // `/prompt <其他词>` → Switch 透传词(预留聚焦切换)。
        let sub = expand_prompt_slash("/prompt my-layer").expect("should match");
        match sub.op {
            zhuye_protocol::Op::PromptCommand {
                action: zhuye_protocol::PromptAction::Switch { layer: Some(l) },
            } => assert_eq!(l, "my-layer"),
            other => panic!("expected Switch(Some), got {other:?}"),
        }
        // 相近前缀不命中(留给插件 / 普通 prompt)。
        for input in ["/prompts", "/promptx", "plain text", "/plan only"] {
            assert!(expand_prompt_slash(input).is_none(), "{input} 不应命中");
        }
    }
}

// ── v1.10 功能2:goal-queue 插件门控单测 ────────────────────────────
#[cfg(test)]
mod goal_queue_tests {
    use super::{GOAL_QUEUE_PLUGIN, goal_queue_plugin_enabled};

    #[test]
    fn 门控按插件名匹配任意scope后缀() {
        assert!(goal_queue_plugin_enabled(
            &["goal-queue@inline".to_string()]
        ));
        assert!(goal_queue_plugin_enabled(&[
            "demo@inline".to_string(),
            "goal-queue@user".to_string(),
        ]));
        assert!(!goal_queue_plugin_enabled(&["demo@inline".to_string()]));
        assert!(!goal_queue_plugin_enabled(&[]));
        // 前缀相似但不相等,不命中。
        assert!(!goal_queue_plugin_enabled(&[
            "goal-queue2@inline".to_string()
        ]));
        assert_eq!(GOAL_QUEUE_PLUGIN, "goal-queue");
    }
}
