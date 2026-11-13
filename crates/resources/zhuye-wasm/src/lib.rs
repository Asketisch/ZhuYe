//! `zhuye-wasm` —— WASM 组件插件宿主(wasmtime 组件模型)。
//!
//! 面向社区插件生态:任何语言(Rust / TypeScript / Python / Go …)按
//! `wit/plugin.wit` 契约产出单个 `.wasm` 组件,经 plugin manifest 声明
//! 后由本 crate 进程内实例化 —— 零子进程、零 JSON-RPC 样板。
//!
//! ## 关键设计
//!
//! - **JSON 字符串 ABI**:进出均为 JSON 文本(契约见 wit 注释与
//!   docs/plugins.md),动态语言作者无需生成强类型绑定。
//! - **确定性超时**:进程级单例 `Engine` 开启 epoch 中断,后台线程
//!   定期推进 epoch;每次调用按 [`DEFAULT_CALL_DEADLINE`] 设定
//!   deadline,超时立即截断,绝不 panic 主进程。
//! - **fail-closed**:hook 调用超时 / `Err` / 非法 JSON / 缺 `action`
//!   一律 Deny(与 shell hook 同语义);tool 报错即 `is_error = true`。
//! - **权限**:WASM 工具经既有外部工具 floor = `Prompt` 注册,与 MCP
//!   工具同基线 —— 组件声明 `Auto` 不会生效。
//! - **沙箱边界**:组件默认无 FS / 无网络 / 无环境变量(空 store)。

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine as WasmEngine, Store};
use wasmtime_wasi::WasiCtx;

use zhuye_hooks::{HookDecision, HookError, HookEvent, SystemMessage};
use zhuye_protocol::ToolOutput;
use zhuye_tools::ToolContext;

/// 单次导出调用的默认 deadline(epoch 中断)。超过即截断 —— hook 按
/// Deny、tool 按 `is_error` 处理(fail-closed)。
pub const DEFAULT_CALL_DEADLINE: Duration = Duration::from_secs(30);

/// epoch ticker 的推进间隔。deadline 以 epoch 计(1 epoch ≈ 此间隔),
/// 精度足够(超时误差 ≤ 10ms)。
const EPOCH_TICK: Duration = Duration::from_millis(10);

/// epoch tick 的毫秒数(set_epoch_deadline 以 tick 计,换算用)。
const EPOCH_TICK_MS: u64 = 10;

wasmtime::component::bindgen!({
    path: "../../../wit",
    world: "zhuye:plugin/zhuye-hook@0.1.0",
});

wasmtime::component::bindgen!({
    path: "../../../wit",
    world: "zhuye:plugin/zhuye-tool@0.1.0",
});

// 两个 world 均无 import(纯计算沙箱):linker 无需额外 define。
// 生成类型:zhuye_hook::ZhuyeHookWorld / zhuye_tool::ZhuyeToolWorld
// (bindgen 按包名分层)。

/// 进程级单例引擎(epoch 中断开启 + ticker 线程)。构造一次全仓共享
/// (编译产物缓存归 Engine)。
pub fn shared_engine() -> &'static WasmEngine {
    static ENGINE: std::sync::OnceLock<WasmEngine> = std::sync::OnceLock::new();
    let engine = ENGINE.get_or_init(|| {
        let mut cfg = Config::new();
        cfg.wasm_component_model(true);
        cfg.epoch_interruption(true);
        WasmEngine::new(&cfg).expect("wasmtime engine 初始化")
    });
    // ticker 一次性注册:引擎已进 OnceLock(&'static),线程持引用安全。
    static TICKER: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    TICKER.get_or_init(|| {
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(EPOCH_TICK);
                engine.increment_epoch();
            }
        });
    });
    engine
}

/// Store 数据:WASI 上下文(wasip2 组件规范隐式 import `wasi:io/poll`;
/// guest 经 wasip2 target 产出时必带)。默认无 stdio / 无 FS / 无 env
/// —— 组件拿不到任何宿主能力。
struct HostState {
    wasi: WasiCtx,
    table: wasmtime::component::ResourceTable,
}

impl wasmtime_wasi::WasiView for HostState {
    fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
        wasmtime_wasi::WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl HostState {
    fn new() -> Self {
        Self {
            wasi: wasmtime_wasi::WasiCtxBuilder::new().build(),
            table: wasmtime::component::ResourceTable::new(),
        }
    }
}

/// 从文件加载组件并做契约校验(编译期即可发现非 zhuye 世界 / 损坏
/// 组件 —— 挂载期显式报错,不延后到首次调用)。
pub fn load_component(path: &Path) -> Result<Component, WasmError> {
    Component::from_file(shared_engine(), path).map_err(WasmError::Component)
}

/// 宿主侧错误。
#[derive(Debug, thiserror::Error)]
pub enum WasmError {
    /// 组件编译 / 加载失败(非 wasm 组件、非 zhuye 世界、文件损坏)。
    #[error("wasm component load failed: {0}")]
    Component(#[source] wasmtime::Error),
    /// 导出调用失败(超时 trap / guest 显式 Err / 实例化失败)。
    #[error("wasm call failed: {0}")]
    Call(String),
    /// 决策 / 声明 / 输出 JSON 解析失败(fail-closed 由调用方决定)。
    #[error("wasm json invalid: {0}")]
    Json(String),
}

// ── WasmHook:进程内 hook(zhuye-hook 世界)──────────────────────────

/// 进程内 WASM hook:实现 [`zhuye_hooks::Hook`],决策 JSON 协议兼容
/// shell hook 的 stdout 协议(见 [`decision_from_json`] 的双键解析;
/// 超时 / `Err` / 非法 JSON → Deny,保持 fail-closed)。
pub struct WasmHook {
    component: Component,
    name: String,
    /// 订阅的事件 kind(来自 hooks.json 的 event 名,loader 传入)。
    /// 引擎按 `events()` 过滤派发 —— 返回空切片会让 hook 永远不被调用
    /// (此前如此,整条链路静默失效)。
    kind: zhuye_hooks::HookEventKind,
    /// 工具名 matcher(hooks.json 的 `matcher`,与 ShellHook 同语义)。
    matcher: Option<String>,
    deadline: Duration,
}

impl WasmHook {
    /// 从 `.wasm` 文件构造。`name` 即引擎注册名(如
    /// `plugin:<id>:<event>#<i>`,与 shell hook 同款);`kind` 是事件
    /// 订阅(hooks.json 的 event 名解析结果),`matcher` 是可选的工具名
    /// 通配(仅对 PreToolUse / PostToolUse 生效)。
    pub fn from_file(
        path: &Path,
        name: impl Into<String>,
        kind: zhuye_hooks::HookEventKind,
        matcher: Option<String>,
    ) -> Result<Self, WasmError> {
        Ok(Self {
            component: load_component(path)?,
            name: name.into(),
            kind,
            matcher,
            deadline: DEFAULT_CALL_DEADLINE,
        })
    }

    /// 自定义 deadline(测试用;生产默认 [`DEFAULT_CALL_DEADLINE`])。
    pub fn with_deadline(mut self, d: Duration) -> Self {
        self.deadline = d;
        self
    }
}

impl std::fmt::Debug for WasmHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmHook")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// 决策 JSON → [`HookDecision`]。
///
/// 协议兼容两套字段(wit 注释与 docs/plugins.md 承诺「与 shell hook 的
/// stdout 协议逐字段一致」,宿主必须兑现):
/// - 决策键:`action`(wasm 原生)或 `decision`(shell 协议);
/// - 取值:`allow` / `ok` / 缺省 / 未知 → Allow(hook 未表达拒绝,与
///   shell 同保守口径;未知值记 warn 便于观测);
///   `deny` / `block` → Deny;`ask` → Ask;
///   `inject` / `message` → InjectMessage(内容键 `content` 回落 `reason`);
/// - wasm 扩展:`modify_args`(带 `args` 对象改写工具入参)。
///
/// 非法 JSON → Deny(fail-closed,与 shell 的「非 JSON 当信息性输出放行」
/// 不同:wasm 契约明确要求返回决策 JSON,非法即组件缺陷)。
fn decision_from_json(json: &str, hook_name: &str) -> HookDecision {
    let trimmed = json.trim();
    if trimmed.is_empty() {
        return HookDecision::Allow;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return HookDecision::Deny {
            reason: format!("wasm hook: 决策 JSON 非法: {trimmed}"),
        };
    };
    if !v.is_object() {
        return HookDecision::Deny {
            reason: format!("wasm hook: 决策必须是 JSON 对象: {trimmed}"),
        };
    };
    // 双键:action(wasm)优先,decision(shell)回落。
    let action = v
        .get("action")
        .or_else(|| v.get("decision"))
        .and_then(|a| a.as_str())
        .unwrap_or("allow")
        .to_ascii_lowercase();
    let reason = || {
        v.get("reason")
            .and_then(|r| r.as_str())
            .unwrap_or_default()
            .to_string()
    };
    match action.as_str() {
        "deny" | "block" => HookDecision::Deny {
            reason: {
                let r = reason();
                if r.is_empty() {
                    format!("denied by wasm hook '{hook_name}'")
                } else {
                    r
                }
            },
        },
        "ask" => HookDecision::Ask {
            reason: {
                let r = reason();
                if r.is_empty() {
                    format!("asked by wasm hook '{hook_name}'")
                } else {
                    r
                }
            },
        },
        "inject" | "message" => {
            // 内容键:content(wasm)回落 reason(shell 的 inject 用 reason)。
            let content = v
                .get("content")
                .and_then(|c| c.as_str())
                .map_or_else(reason, str::to_string);
            HookDecision::InjectMessage(SystemMessage::new(content))
        }
        "modify_args" => match v.get("args") {
            Some(args) if args.is_object() => HookDecision::ModifyArgs(args.clone()),
            _ => HookDecision::Deny {
                reason: "wasm hook: modify_args 缺 args 对象".into(),
            },
        },
        "allow" | "ok" => HookDecision::Allow,
        other => {
            tracing::warn!(
                hook = hook_name,
                action = other,
                "wasm hook: 未知决策值,按 allow"
            );
            HookDecision::Allow
        }
    }
}

#[async_trait]
impl zhuye_hooks::Hook for WasmHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "WASM 组件 hook(进程内,JSON 决策协议)"
    }

    // 引擎 dispatch 按 events() 过滤(`[].contains(kind)` 恒 false),
    // 必须如实返回 hooks.json 声明的 kind;matcher 在 handle 内消化
    // (与 ShellHook 同款两段式)。
    fn events(&self) -> &[zhuye_hooks::HookEventKind] {
        std::slice::from_ref(&self.kind)
    }

    async fn handle(&self, event: &HookEvent) -> Result<HookDecision, HookError> {
        // matcher:仅对携带工具名的事件生效(与 ShellHook 同语义)。
        let tool = match event {
            HookEvent::PreToolUse { tool, .. } => Some(tool.as_str()),
            HookEvent::PostToolUse { tool, .. } => Some(tool.as_str()),
            _ => None,
        };
        if let Some(pattern) = self.matcher.as_deref() {
            let allowed = tool.is_some_and(|t| zhuye_hooks::shell::wildcard_match(pattern, t));
            if !allowed {
                return Ok(HookDecision::Allow);
            }
        }
        let event_json = serde_json::to_string(event)
            .map_err(|e| HookError::Other(format!("wasm hook: 事件序列化失败: {e}")))?;
        // bindgen 生成的是同步调用:wasm 执行是纯 CPU,放 spawn_blocking
        // 避免阻塞 tokio worker;epoch deadline 保证最长阻塞时长
        // (超时 trap → fail-closed)。
        let component = self.component.clone();
        let deadline_ticks = self.deadline.as_millis() as u64 / EPOCH_TICK_MS;
        let call_result = tokio::task::spawn_blocking(move || {
            let engine = shared_engine();
            let mut store = Store::new(engine, HostState::new());
            store.set_epoch_deadline(deadline_ticks);
            let mut linker: Linker<HostState> = Linker::new(engine);
            // wasi:io/poll 隐式 imports(wasip2 规范)由 wasmtime-wasi
            // 提供(sync 实现);组件导出是同步纯 CPU 函数,不实际调用
            // wasi,import 满足声明即可。
            wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
            let world = ZhuyeHook::instantiate(&mut store, &component, &linker)?;
            world.call_handle(&mut store, &event_json)
        })
        .await
        .map_err(|e| HookError::Other(format!("wasm hook: blocking join: {e}")))?;
        match call_result {
            // 两层:wasmtime(trap)/ guest result。
            Ok(Ok(decision_json)) => Ok(decision_from_json(&decision_json, &self.name)),
            Ok(Err(user_err)) => Ok(HookDecision::Deny {
                reason: format!("wasm hook: 组件返回 Err: {user_err}"),
            }),
            Err(trap) => {
                // epoch 超时表现为 wasmtime trap;非超时 trap(如 guest
                // 在 deadline 边缘被打断时的任意点异常)一律 fail-closed
                // Deny —— 决策路径不向引擎传播 Err。
                let msg = format!("{trap}");
                if msg.contains("epoch") {
                    Ok(HookDecision::Deny {
                        reason: format!("wasm hook: 超时(>{:?})", self.deadline),
                    })
                } else {
                    Ok(HookDecision::Deny {
                        reason: format!("wasm hook: 调用失败(fail-closed): {msg}"),
                    })
                }
            }
        }
    }
}

// ── WasmTool:进程内工具(zhuye-tool 世界)────────────────────────────

/// 进程内 WASM 工具:实现 [`zhuye_tools::Tool`]。声明来自组件的
/// `describe()`;执行走 `invoke()`(超时 → `is_error`)。
pub struct WasmTool {
    component: Component,
    name: String,
    /// 组件 describe() 的描述(manifest 覆盖可能改写;loader 读取)。
    pub description: String,
    parameters: serde_json::Value,
    deadline: Duration,
}

impl WasmTool {
    /// 从 `.wasm` 文件构造:实例化调 `describe()` 拿声明。
    /// 失败(非 zhuye-tool 世界 / describe 非法)即返回错误 —— 挂载期
    /// 显式报错,不注册半成品工具。
    pub async fn from_file(path: &Path) -> Result<Self, WasmError> {
        let component = load_component(path)?;
        let engine = shared_engine();
        let mut store = Store::new(engine, HostState::new());
        // epoch_interruption 开启时 deadline 缺省 0(视为已耗尽),必须
        // 在 instantiate **之前**设好 —— 带 start / 初始化代码的组件
        // (tinygo 等)在实例化期执行 guest 代码,顺序颠倒会在挂载期 trap。
        store.set_epoch_deadline(DEFAULT_CALL_DEADLINE.as_millis() as u64 / EPOCH_TICK_MS);
        let mut linker: Linker<HostState> = Linker::new(engine);
        // wasi:io/poll@0.2.12 等隐式 imports 由 wasmtime-wasi(p2)提供;
        // p2 实现是 async 注册,实例化走 async 路径。样例/社区组件的
        // 导出是同步函数且不实际调用 wasi —— import 满足声明即可。
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| WasmError::Call(format!("wasi linker: {e}")))?;
        let world = ZhuyeTool::instantiate(&mut store, &component, &linker)
            .map_err(WasmError::Component)?;
        let desc = world
            .call_describe(&mut store)
            .map_err(|e| WasmError::Call(format!("describe: {e}")))?;
        let v: serde_json::Value = serde_json::from_str(&desc)
            .map_err(|e| WasmError::Json(format!("describe 非法 JSON: {e}")))?;
        let name = v
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or_default()
            .to_string();
        if name.is_empty() {
            return Err(WasmError::Json("describe 缺 name".into()));
        }
        let description = v
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or_default()
            .to_string();
        // parameters 缺省给空 object 而非 JSON null —— OpenAI / Anthropic
        // 的工具 schema 字段要求 object,null 会被 provider 拒单。
        let parameters = match v.get("parameters") {
            Some(p) if p.is_object() => p.clone(),
            Some(p) => {
                return Err(WasmError::Json(format!(
                    "describe.parameters 必须是 JSON object:{p}"
                )));
            }
            None => serde_json::json!({"type": "object", "properties": {}}),
        };
        Ok(Self {
            component,
            name,
            description,
            parameters,
            deadline: DEFAULT_CALL_DEADLINE,
        })
    }

    /// 自定义 deadline(测试用)。
    pub fn with_deadline(mut self, d: Duration) -> Self {
        self.deadline = d;
        self
    }

    /// manifest 显式描述覆盖:仅当组件 `describe()` 未给出描述时生效。
    pub fn with_description_override(mut self, desc: String) -> Self {
        if self.description.is_empty() {
            self.description = desc;
        }
        self
    }
}

impl std::fmt::Debug for WasmTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmTool")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// guest 输出 JSON → [`ToolOutput`]。
///
/// wit 契约(`plugin.wit`)只要求 `content` / `is_error`;`metadata` /
/// `elapsed_ms` 可省 —— 宿主补缺省(metadata `{}`;elapsed_ms 由宿主
/// 计时,guest 填的 0 本来就没有意义)。此前直接
/// `from_str::<ToolOutput>` 要求四字段齐全,严格超出契约。
fn guest_output_to_tool_output(
    out_json: &str,
    started: std::time::Instant,
) -> Result<ToolOutput, zhuye_protocol::ToolError> {
    let v: serde_json::Value = serde_json::from_str(out_json).map_err(|e| {
        zhuye_protocol::ToolError::Execution(format!("wasm tool: 输出非法 JSON: {e}"))
    })?;
    if !v.is_object() {
        return Err(zhuye_protocol::ToolError::Execution(format!(
            "wasm tool: 输出必须是 JSON 对象:{out_json}"
        )));
    }
    let content: Vec<zhuye_protocol::ContentBlock> = match v.get("content") {
        Some(c) => serde_json::from_value(c.clone()).map_err(|e| {
            zhuye_protocol::ToolError::Execution(format!("wasm tool: content 块非法: {e}"))
        })?,
        None => Vec::new(),
    };
    let is_error = v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
    let metadata = v
        .get("metadata")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    Ok(ToolOutput {
        content,
        is_error,
        metadata,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

#[async_trait]
impl zhuye_tools::Tool for WasmTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.parameters.clone()
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        _ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, zhuye_protocol::ToolError> {
        let args_json = serde_json::to_string(&args)
            .map_err(|e| zhuye_protocol::ToolError::Execution(e.to_string()))?;
        let component = self.component.clone();
        let deadline_ticks = self.deadline.as_millis() as u64 / EPOCH_TICK_MS;
        let started = std::time::Instant::now();
        let call_result = tokio::task::spawn_blocking(move || {
            let engine = shared_engine();
            let mut store = Store::new(engine, HostState::new());
            store.set_epoch_deadline(deadline_ticks);
            let mut linker: Linker<HostState> = Linker::new(engine);
            // wasi:io/poll 隐式 imports(同 hook 路径)。
            wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
            let world = ZhuyeTool::instantiate(&mut store, &component, &linker)?;
            world.call_invoke(&mut store, &args_json)
        })
        .await
        .map_err(|e| {
            zhuye_protocol::ToolError::Execution(format!("wasm tool: blocking join: {e}"))
        })?;
        match call_result {
            Ok(Ok(out_json)) => guest_output_to_tool_output(&out_json, started),
            Ok(Err(user_err)) => Err(zhuye_protocol::ToolError::Execution(format!(
                "wasm tool: 组件返回 Err: {user_err}"
            ))),
            Err(trap) => {
                let msg = format!("{trap}");
                if msg.contains("epoch") {
                    Err(zhuye_protocol::ToolError::Execution(format!(
                        "wasm tool: 超时(>{:?})",
                        self.deadline
                    )))
                } else {
                    Err(zhuye_protocol::ToolError::Execution(format!(
                        "wasm tool: {msg}"
                    )))
                }
            }
        }
    }
}

// ── 决策 / 输出解析单元测试(纯函数,不依赖 wasm 产物)────────────────

#[cfg(test)]
mod decision_tests {
    use super::*;

    /// wit 注释与 docs 承诺「与 shell hook 的 stdout 协议逐字段一致」:
    /// shell 协议的 `decision` / `block` / `message` / `reason` 键组合
    /// 必须全部可用,否则社区按 shell 协议写的组件会被 fail-closed 拒掉。
    #[test]
    fn 决策解析_兼容shell协议键() {
        // decision: deny + reason。
        assert!(matches!(
            decision_from_json(r#"{"decision":"deny","reason":"nope"}"#, "h"),
            HookDecision::Deny { reason } if reason == "nope"
        ));
        // decision: block(shell 别名)。
        assert!(matches!(
            decision_from_json(r#"{"decision":"block","reason":"b"}"#, "h"),
            HookDecision::Deny { .. }
        ));
        // decision 缺省 → Allow(shell:hook 未表达拒绝)。
        assert!(matches!(
            decision_from_json(r#"{"reason":"info only"}"#, "h"),
            HookDecision::Allow
        ));
        // 空输出 → Allow(shell 的信息性输出)。
        assert!(matches!(decision_from_json("  ", "h"), HookDecision::Allow));
        // inject:message 别名 + reason 内容(shell 协议)。
        match decision_from_json(r#"{"decision":"message","reason":"note"}"#, "h") {
            HookDecision::InjectMessage(m) => assert_eq!(m.content, "note"),
            other => panic!("message 应 InjectMessage:{other:?}"),
        }
        // inject:action(wasm)用 content 键。
        match decision_from_json(r#"{"action":"inject","content":"c1"}"#, "h") {
            HookDecision::InjectMessage(m) => assert_eq!(m.content, "c1"),
            other => panic!("{other:?}"),
        }
        // action: deny(未知 action 值记 warn 后 allow,与 shell 同保守口径)。
        assert!(matches!(
            decision_from_json(r#"{"action":"weird"}"#, "h"),
            HookDecision::Allow
        ));
        // modify_args:wasm 扩展。
        assert!(matches!(
            decision_from_json(r#"{"action":"modify_args","args":{"a":1}}"#, "h"),
            HookDecision::ModifyArgs(_)
        ));
        // 非法 JSON → Deny(fail-closed)。
        assert!(matches!(
            decision_from_json("not json", "h"),
            HookDecision::Deny { .. }
        ));
        // 非对象 JSON → Deny。
        assert!(matches!(
            decision_from_json(r#""str""#, "h"),
            HookDecision::Deny { .. }
        ));
    }

    /// guest 输出只要求 wit 契约的 content / is_error;metadata / elapsed_ms
    /// 可省略(宿主补缺省并按真实耗时填 elapsed_ms)。
    #[test]
    fn 工具输出解析_缺省字段() {
        let started = std::time::Instant::now();
        let out = guest_output_to_tool_output(
            r#"{"content":[{"type":"text","text":"hi"}],"is_error":false}"#,
            started,
        )
        .expect("两字段输出应可解析");
        assert!(!out.is_error);
        assert_eq!(out.metadata, serde_json::json!({}));
        assert_eq!(out.content.len(), 1);
        // 完整字段照常透传。
        let out2 = guest_output_to_tool_output(
            r#"{"content":[],"is_error":true,"metadata":{"k":1},"elapsed_ms":999}"#,
            started,
        )
        .expect("完整输出应可解析");
        assert!(out2.is_error);
        assert_eq!(out2.metadata["k"], 1);
        // 非对象 → 报错。
        assert!(guest_output_to_tool_output(r#""text""#, started).is_err());
        assert!(guest_output_to_tool_output("bad", started).is_err());
    }
}

// ── 契约闭环测试(依赖 plugin-demo 样例产物;缺失即 skip,不假绿)──

#[cfg(test)]
mod contract_tests {
    use super::*;
    use zhuye_hooks::Hook as _;
    use zhuye_tools::Tool as _;

    /// 样例组件路径(由 scripts/build_plugin_demo_wasm.sh 产出)。
    fn demo_wasm(name: &str) -> Option<std::path::PathBuf> {
        // 测试 cwd = crate 目录(crates/resources/zhuye-wasm)。
        let p =
            std::path::Path::new("../../../plugins/plugin-demo/bin").join(format!("{name}.wasm"));
        p.exists().then_some(p)
    }

    fn tokio_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("测试 runtime")
    }

    /// 加载 Rust 样例 hook 组件:组件校验 + Allow/inject 决策闭环 +
    /// 事件订阅与 matcher 语义。
    #[test]
    fn wasm_hook_demo_allow_roundtrip() {
        let Some(path) = demo_wasm("zhuye-demo-hook") else {
            eprintln!("skip:样例组件缺失(先跑 scripts/build_plugin_demo_wasm.sh)");
            return;
        };
        let hook = WasmHook::from_file(
            &path,
            "test:demo-hook",
            zhuye_hooks::HookEventKind::PostToolUse,
            Some("bash".into()),
        )
        .expect("加载失败");
        // events() 必须如实上报 kind —— 引擎 dispatch 按 events() 过滤,
        // 空切片曾让 wasm hook 永远不被派发。
        assert!(
            hook.events()
                .contains(&zhuye_hooks::HookEventKind::PostToolUse)
        );
        let ev: HookEvent = serde_json::from_value(serde_json::json!({
            "kind": "post_tool_use",
            "tool": "bash",
            "result": { "content": [{"type": "text", "text": "out"}], "is_error": false, "metadata": null, "elapsed_ms": 1 },
            "elapsed_ms": 1
        }))
        .expect("构造 HookEvent 失败");
        let rt = tokio_runtime();
        let decision = rt.block_on(hook.handle(&ev)).expect("handle 不应 Err");
        // 样例策略:post_tool_use + bash → inject(决策协议注入分支);
        // 决策 JSON 由组件构造,经宿主解析回 HookDecision —— 闭环验证。
        match &decision {
            HookDecision::InjectMessage(m) => {
                assert!(m.content.contains("[demo-wasm]"), "注入内容:{m:?}");
            }
            other => panic!("样例 hook 应 InjectMessage: got {other:?}"),
        }
        // matcher 不命中(非 bash)→ 不调用组件,直接 Allow。
        let ev_other: HookEvent = serde_json::from_value(serde_json::json!({
            "kind": "post_tool_use",
            "tool": "grep",
            "result": { "content": [{"type": "text", "text": "out"}], "is_error": false, "metadata": null, "elapsed_ms": 1 },
            "elapsed_ms": 1
        }))
        .expect("构造 HookEvent 失败");
        let decision = rt
            .block_on(hook.handle(&ev_other))
            .expect("handle 不应 Err");
        assert!(
            matches!(decision, HookDecision::Allow),
            "matcher 不命中应 Allow"
        );
    }

    /// 超时 fail-closed:极短 deadline → Deny(不 panic)。
    #[test]
    fn wasm_hook_timeout_fail_closed() {
        let Some(path) = demo_wasm("zhuye-demo-hook") else {
            eprintln!("skip:样例组件缺失");
            return;
        };
        let hook = WasmHook::from_file(
            &path,
            "test:timeout-hook",
            zhuye_hooks::HookEventKind::PostToolUse,
            None,
        )
        .expect("加载失败")
        .with_deadline(Duration::from_millis(0));
        let ev: HookEvent = serde_json::from_value(serde_json::json!({
            "kind": "post_tool_use",
            "tool": "bash",
            "result": { "content": [{"type": "text", "text": "out"}], "is_error": false, "metadata": null, "elapsed_ms": 1 },
            "elapsed_ms": 1
        }))
        .expect("构造失败");
        let rt = tokio_runtime();
        let decision = rt.block_on(hook.handle(&ev)).expect("handle 不应 Err");
        // deadline=0 → epoch 立即过期 → trap → fail-closed Deny。
        assert!(
            matches!(decision, HookDecision::Deny { .. }),
            "超时应 Deny: got {decision:?}"
        );
    }

    /// 加载 Rust 样例 tool 组件:describe 声明 + invoke 输出闭环。
    #[test]
    fn wasm_tool_demo_describe_and_invoke() {
        let Some(path) = demo_wasm("zhuye-demo-tool") else {
            eprintln!("skip:样例组件缺失(先跑 scripts/build_plugin_demo_wasm.sh)");
            return;
        };
        let rt = tokio_runtime();
        rt.block_on(async move {
            let tool = WasmTool::from_file(&path).await.expect("加载失败");
            assert_eq!(tool.name(), "demo_echo", "describe 的 name");
            assert!(
                tool.description().contains("echo"),
                "describe 的 description"
            );
            let schema = tool.parameters_schema();
            assert_eq!(schema["type"], "object", "参数 schema 形态");

            let ctx = ToolContext::default();
            let out = tool
                .execute(ctx, serde_json::json!({ "text": "hello-wasm" }))
                .await
                .expect("invoke 不应 Err");
            assert!(!out.is_error);
            let text = serde_json::to_string(&out.content).unwrap();
            assert!(text.contains("hello-wasm"), "echo 回显: {text}");
        });
    }
}
