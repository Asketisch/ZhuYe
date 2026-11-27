//! shim 核心逻辑(纯函数,单测覆盖):定位上游 agent-browser 二进制、
//! 组装子进程命令与环境、降级态 MCP 应答。
//!
//! ## 上游接入契约(对照 v0.45.0 源码确认)
//!
//! - `agent-browser mcp` 子命令**只接受** `--tools` / `--config` 选项;
//!   attach 端点是全局状态,必须走 `AGENT_BROWSER_CDP` env(或全局
//!   `--cdp` flag / 配置文件)。走 env 的原因:MCP server 内部把每个工具
//!   调用委托给本二进制的 `--json` 子进程,env 会被完整继承,而 argv 上
//!   的全局 flag 不会透传给委托调用 —— env 是唯一能把「attach 语义」
//!   贯穿到每个工具调用的通道;
//! - 上游 daemon 对 cdp 端点连接失败**只报错,不回退自启浏览器**
//!   (native/browser.rs `connect_cdp` 直接返回 Err;自启路径只在未配置
//!   cdp 时走)—— 这是「不下载浏览器」原则成立的根基;
//! - 端点值接受端口号或 `ws://` URL,统一给全量 URL 免除歧义。

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use crate::BrowserError;
use crate::chrome;

/// 上游二进制定位的 env 覆盖。
pub const AGENT_BROWSER_ENV: &str = "ZHUYE_AGENT_BROWSER";
/// 上游识别的 attach 端点 env(上游 flags.rs 从此处读 cdp)。
pub const CDP_ENV: &str = "AGENT_BROWSER_CDP";
/// 上游识别的 session env(上游 flags.rs 从此处读 session;daemon socket
/// 按 session 命名,是 daemon 身份的唯一键)。
pub const SESSION_ENV: &str = "AGENT_BROWSER_SESSION";
/// 上游二进制文件名(Windows 下补 `.exe`)。
pub const UPSTREAM_BIN: &str = "agent-browser";
/// 降级态 MCP 协议版本(与宿主 zhuye-mcp 客户端既有协商兼容)。
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// 本 shim 实例的上游 session 名(`zhuye-` 前缀隔离用户自用会话;附 PID
/// 与启动秒级时间戳)。
///
/// 修复原因:上游 daemon 按 session 复用,而 daemon 身份**不含** cdp
/// 端点 —— 复用默认 session 时,上一实例残留的 daemon(上游 idle 1 小时
/// 才退出)会把它记住的旧浏览器(已被本 shim 回收)发给新实例,表现为
/// "CDP connection refused"。独立 session 使每个 shim 实例拿到全新
/// daemon,attach 端点必然与 env 一致。
pub fn upstream_session_name() -> String {
    // OnceLock 固化为进程级单例:一次 shim 生命周期 = 一个 daemon 身份。
    // 直接现算会因秒级时间戳翻转导致同进程前后两次组装出不同 session。
    static SESSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SESSION
        .get_or_init(|| {
            let pid = std::process::id();
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            format!("zhuye-p{pid}-t{secs}")
        })
        .clone()
}

/// 定位上游 agent-browser 可执行文件。
///
/// 顺序:env 覆盖 → 本二进制同目录(插件 `bin/` 随包捆绑,构建脚本从
/// submodule 产出)→ PATH(brew/npm 安装兜底)。找不到返回 None。
pub fn locate_upstream_binary() -> Option<PathBuf> {
    let exe_suffix = if cfg!(windows) { ".exe" } else { "" };
    if let Ok(custom) = std::env::var(AGENT_BROWSER_ENV) {
        let path = PathBuf::from(custom);
        // env 指向的路径不存在 → 视为配置错误,直接放弃探测(与浏览器
        // env 覆盖同一口径:显式配置错了就该报错,不静默换路)。
        return path.exists().then_some(path);
    }
    // 与本 shim 同目录(插件 bin/ 捆绑布局)。current_exe 在三个平台均可靠;
    // 开发期 `cargo run` 时同目录是 target/(只有装进插件目录才捆绑上游),
    // 找不到自然落到 PATH 兜底。
    if let Ok(self_exe) = std::env::current_exe() {
        if let Some(dir) = self_exe.parent() {
            let sibling = dir.join(format!("{UPSTREAM_BIN}{exe_suffix}"));
            if sibling.exists() {
                return Some(sibling);
            }
        }
    }
    // PATH 探测。
    let name = format!("{UPSTREAM_BIN}{exe_suffix}");
    let on_path = std::env::var("PATH").is_ok_and(|paths| {
        paths
            .split(std::path::MAIN_SEPARATOR_STR)
            .any(|dir| !dir.is_empty() && Path::new(dir).join(&name).exists())
    });
    on_path.then_some(PathBuf::from(name))
}

/// 组装上游 MCP 子进程命令:attach 端点与 session 都走 env,argv 只带
/// `mcp`。env 会被上游对每个工具调用的委托子进程完整继承(argv 上的
/// 全局 flag 不会),是唯一能把 attach / session 语义贯穿始终的通道。
pub fn upstream_command(binary: &Path, cdp_url: &str) -> Command {
    let mut cmd = Command::new(binary);
    cmd.arg("mcp")
        .env(CDP_ENV, cdp_url)
        .env(SESSION_ENV, upstream_session_name());
    cmd
}

/// 确保本机浏览器可用,返回浏览器级 ws 端点与进程句柄。
///
/// 先 attach:`DevToolsActivePort` 可读且端口存活(桌面 browser_backend
/// 或上一会话先启动的场景);否则探测并启动 headless 新实例(隔离
/// profile)。失败给结构化错误(文案可操作,给 LLM / 用户)。
pub fn ensure_browser() -> Result<(String, chrome::BrowserProcess), BrowserError> {
    let dir = chrome::profile_dir();
    if let Ok((port, path)) = chrome::read_active_port(&dir) {
        if chrome::instance_alive(port) {
            return Ok((
                chrome::browser_ws_url(port, &path),
                chrome::BrowserProcess::attached(),
            ));
        }
        eprintln!("[browser-server] DevToolsActivePort 残留但端口 {port} 已死,启动新实例");
    }
    let executable = chrome::discover_executable().ok_or_else(|| {
        BrowserError::unavailable(
            "browser",
            format!(
                "本机浏览器未找到:请安装 Chrome/Edge/Chromium,或用 {}/路径覆盖指定可执行文件",
                chrome::BROWSER_EXECUTABLE_ENV
            ),
        )
    })?;
    let process = chrome::launch(&executable, &dir, true)?;
    let (port, path) = process.devtools_port()?;
    Ok((chrome::browser_ws_url(port, &path), process))
}

/// 降级原因(决定 isError 文案的可操作提示方向)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DegradeReason {
    /// 本机浏览器不可用。
    BrowserMissing(String),
    /// 上游 agent-browser 二进制未找到。
    UpstreamMissing,
}

impl DegradeReason {
    /// isError 文案(给 LLM / 用户,含全部获取路径)。
    pub fn message(&self) -> String {
        match self {
            DegradeReason::BrowserMissing(detail) => format!(
                "browser 不可用:{detail}。修复路径:安装本机 Chrome/Edge/Chromium;\
                 或设置 {env}=<浏览器可执行文件路径>。",
                env = chrome::BROWSER_EXECUTABLE_ENV,
            ),
            // 修复原因:三种获取路径全部列出,失败提示必须可操作
            // (插件 bin/ 捆绑是默认期望,PATH 与 env 是显式兜底)。
            DegradeReason::UpstreamMissing => format!(
                "browser 不可用:agent-browser 二进制未找到。修复路径(任选其一):\
                 ① 构建捆绑版 —— git submodule update --init --depth 1 \
                 third_party/agent-browser 后运行 scripts/build_browser_plugin.sh;\
                 ② PATH 安装(brew install agent-browser 或 npm i -g agent-browser);\
                 ③ 设置 {env}=<agent-browser 可执行文件路径>。",
                env = AGENT_BROWSER_ENV,
            ),
        }
    }
}

/// 降级态静态工具表(与上游核心面同名,失败反馈可操作)。
///
/// 修复原因:降级时仍暴露工具面而非空表 —— LLM 看到能力存在 + 调用即得
/// 修复指引,好过静默无工具让模型反复尝试别的路径。
pub fn degraded_tools_json() -> Value {
    let schema = |props: Value| json!({"type": "object", "properties": props});
    json!({
        "tools": [
            {
                "name": "agent_browser_open",
                "description": "导航到 URL(等页面加载完成;之后重新 snapshot)。",
                "inputSchema": schema(json!({"url": {"type": "string"}})),
            },
            {
                "name": "agent_browser_snapshot",
                "description": "拍页面可及性快照,返回带 [ref=eN] 元素引用的树形文本。",
                "inputSchema": schema(json!({})),
            },
            {
                "name": "agent_browser_click",
                "description": "点击快照中的元素([ref=eN] 引用)。",
                "inputSchema": schema(json!({"selector": {"type": "string", "description": "[ref=eN] 元素引用"}})),
            },
            {
                "name": "agent_browser_fill",
                "description": "向输入框填写文本。",
                "inputSchema": schema(json!({"selector": {"type": "string"}, "text": {"type": "string"}})),
            },
            {
                "name": "agent_browser_press",
                "description": "按组合键,如 \"enter\" / \"ctrl+a\"。",
                "inputSchema": schema(json!({"keys": {"type": "string"}})),
            },
            {
                "name": "agent_browser_scroll",
                "description": "滚轮滚动(正 dy = 下)。",
                "inputSchema": schema(json!({"dx": {"type": "integer"}, "dy": {"type": "integer"}})),
            },
            {
                "name": "agent_browser_screenshot",
                "description": "截取当前页面,返回图片。",
                "inputSchema": schema(json!({})),
            },
            {
                "name": "agent_browser_close",
                "description": "关闭受控标签页。",
                "inputSchema": schema(json!({})),
            }
        ]
    })
}

/// 降级态请求处理(与真代理态同一 wire 协议;line-delimited JSON-RPC)。
pub fn degraded_response(method: &str, params: &Value, reason: &DegradeReason) -> Value {
    match method {
        "initialize" => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "serverInfo": {
                "name": "zhuye-browser",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {"tools": {"listChanged": false}},
        }),
        "tools/list" => degraded_tools_json(),
        // 降级态所有工具调用统一 isError:错误文案带请求的工具名 + 可操作修复路径。
        "tools/call" => {
            let tool = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            json!({
                "content": [{"type": "text", "text": format!("[{tool}] {}", reason.message())}],
                "isError": true,
            })
        }
        "ping" => json!({}),
        other => json!({
            "content": [{"type": "text", "text": format!("method not implemented: {other}")}],
            "isError": true,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_command_puts_cdp_and_session_in_env_not_argv() {
        let cmd = upstream_command(
            Path::new("/usr/local/bin/agent-browser"),
            "ws://127.0.0.1:9222/x",
        );
        let program = cmd.get_program().to_string_lossy().into_owned();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let envs: Vec<(String, String)> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                v.map(|v| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect();
        assert_eq!(program, "/usr/local/bin/agent-browser");
        assert_eq!(
            args,
            vec!["mcp".to_string()],
            "argv 只应带 mcp,端点/session 必须走 env"
        );
        assert_eq!(
            envs.iter()
                .find(|(k, _)| k == CDP_ENV)
                .map(|(_, v)| v.as_str()),
            Some("ws://127.0.0.1:9222/x"),
        );
        let session = envs
            .iter()
            .find(|(k, _)| k == SESSION_ENV)
            .map(|(_, v)| v.clone())
            .expect("必须设置独立 session");
        // session 命名规则:zhuye- 前缀 + 合法字符(上游只收 alnum/-/_)。
        assert!(session.starts_with("zhuye-"), "{session}");
        assert!(
            session
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_'),
            "session 含上游不允许的字符: {session}"
        );
        // 同进程内两次组装 session 稳定(本进程只应产生一个 daemon 身份)。
        let cmd2 = upstream_command(Path::new("/x/agent-browser"), "ws://127.0.0.1:1/y");
        let session2 = cmd2
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new(SESSION_ENV))
            .and_then(|(_, v)| v)
            .and_then(|v| v.to_str())
            .unwrap();
        assert_eq!(session, session2, "同进程 session 应稳定");
    }

    #[test]
    fn degraded_tools_list_matches_upstream_core_surface() {
        let tools = degraded_tools_json()["tools"].as_array().unwrap().clone();
        assert_eq!(tools.len(), 8);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in [
            "agent_browser_open",
            "agent_browser_snapshot",
            "agent_browser_click",
            "agent_browser_fill",
            "agent_browser_press",
            "agent_browser_scroll",
            "agent_browser_screenshot",
            "agent_browser_close",
        ] {
            assert!(names.contains(&expected), "缺 {expected}");
        }
        // 每个工具都必须带 inputSchema(MCP 宿主注册的硬要求)。
        for tool in &tools {
            assert!(
                tool["inputSchema"].is_object(),
                "{} 缺 inputSchema",
                tool["name"]
            );
        }
    }

    #[test]
    fn degraded_tools_call_is_error_with_actionable_hint() {
        let response = degraded_response(
            "tools/call",
            &json!({"name": "agent_browser_snapshot", "arguments": {}}),
            &DegradeReason::UpstreamMissing,
        );
        assert_eq!(response["isError"], true, "{response}");
        let text = response["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains(AGENT_BROWSER_ENV),
            "提示应含 env 覆盖: {text}"
        );
        assert!(
            text.contains("build_browser_plugin.sh"),
            "提示应含构建路径: {text}"
        );
        assert!(
            text.contains("third_party/agent-browser"),
            "提示应含 submodule 初始化: {text}"
        );
    }

    #[test]
    fn degraded_browser_missing_hint_mentions_executable_env() {
        let reason = DegradeReason::BrowserMissing("本机浏览器未找到".into());
        let text = reason.message();
        assert!(text.contains(chrome::BROWSER_EXECUTABLE_ENV), "{text}");
        assert!(text.contains("Chrome"), "{text}");
    }

    #[test]
    fn degraded_initialize_keeps_protocol_shape() {
        let response = degraded_response(
            "initialize",
            &json!({"protocolVersion": "2025-06-18"}),
            &DegradeReason::UpstreamMissing,
        );
        assert_eq!(response["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(response["serverInfo"]["name"], "zhuye-browser");
        assert!(response["capabilities"]["tools"].is_object());
    }

    #[test]
    fn degraded_unknown_method_reports_not_implemented() {
        let response = degraded_response(
            "resources/list",
            &json!({}),
            &DegradeReason::UpstreamMissing,
        );
        assert_eq!(response["isError"], true);
        assert!(
            response["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("resources/list")
        );
    }
}
