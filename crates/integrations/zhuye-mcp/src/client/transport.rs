//! MCP transport 构造:stdio、streamable-http 与 legacy SSE。
//!
//! ## 安全约束
//!
//! - stdio 子进程用 `env_clear()` 清空父进程 env,避免把
//!   `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` 等敏感凭据泄露到
//!   第三方 MCP server 子进程。然后只注入:
//!   - `HOME`(很多 MCP server 找配置文件需要)
//!   - `PATH`(找子命令需要)
//!   - 用户在 `[mcp_servers.x].env` 显式声明的变量
//! - `kill_on_drop(true)`:Zhuye 主进程 panic 时,子进程一起退出,
//!   避免 zombie。

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

#[allow(deprecated)]
use rmcp::ClientHandler;
#[allow(deprecated)]
use rmcp::model::{
    CreateMessageRequestParams, CreateMessageResult, ElicitRequestParams, ElicitResult,
    ElicitationAction, ElicitationSchema, ErrorData as RmcpError, Implementation,
};
#[allow(deprecated)]
use rmcp::transport::{
    TokioChildProcess,
    streamable_http_client::{StreamableHttpClientTransport, StreamableHttpClientTransportConfig},
};

use rmcp::service::{RequestContext, RoleClient};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use zhuye_protocol::{
    McpElicitationEvent, McpElicitationField, McpElicitationFieldKind, McpElicitationKind,
    McpElicitationResponse,
};

use crate::McpError;
use crate::config::McpTransport;
use crate::manager::McpLifecycleEvent;
use crate::tool::{McpToolDescriptor, tool_descriptor_from_rmcp};

/// 把 rmcp 的 server→client 请求桥到宿主事件通道。
///
/// 只有 elicitation 落地:sampling(`create_message`)需要宿主模型调用与人工授权,
/// 尚未实现,因此 `get_info` **不声明** sampling 能力,合规服务端不会发该请求。
struct HostClientHandler {
    server_name: String,
    lifecycle_tx: mpsc::Sender<McpLifecycleEvent>,
    /// 各 server 当前生效的工具快照(与 manager 共享):`(版本号, 描述符)`。
    /// 运行期刷新时据此算出「哪些工具消失了」。
    tool_snapshots: Arc<tokio::sync::RwLock<HashMap<String, crate::manager::ToolSnapshot>>>,
}

#[allow(deprecated)]
impl ClientHandler for HostClientHandler {
    fn get_info(&self) -> rmcp::model::ClientConfig {
        #[allow(deprecated)]
        let capabilities = rmcp::model::ClientCapabilities::builder()
            .enable_elicitation()
            .build();
        rmcp::model::ClientConfig::new(
            capabilities,
            Implementation::new("zhuye", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, RmcpError> {
        let (message, kind, require_content) = match request {
            ElicitRequestParams::FormElicitationParams {
                message,
                requested_schema,
                ..
            } => (message, elicitation_form(&requested_schema), true),
            ElicitRequestParams::UrlElicitationParams { message, url, .. } => {
                (message, McpElicitationKind::Link { url }, false)
            }
            // 未识别的服务端形态(协议后续新增):不猜语义,按 decline 让
            // 调用它的 tools/call 继续收尾。
            _ => return Ok(ElicitResult::new(ElicitationAction::Decline)),
        };
        self.ask(message, kind, require_content).await
    }

    /// 服务端通知工具清单变化 → 重新拉取并把新清单交给宿主。
    ///
    /// rmcp 在收到通知时会失效自己的工具缓存,这里再 `list_all_tools()`
    /// 即拿到服务端最新清单。变化以 `McpLifecycleEvent::ToolsChanged`
    /// 上报(新清单 + 消失的工具全名),由调用方更新工具注册表。
    async fn on_tool_list_changed(&self, context: rmcp::service::NotificationContext<RoleClient>) {
        let peer = context.peer.clone();
        let listed = peer.list_all_tools().await;
        let tools = match listed {
            Ok(tools) => tools,
            Err(error) => {
                tracing::warn!(
                    server = %self.server_name,
                    %error,
                    "工具列表刷新失败,保留当前注册"
                );
                return;
            }
        };
        let descriptors: Vec<McpToolDescriptor> = tools
            .iter()
            .map(|tool| tool_descriptor_from_rmcp(&self.server_name, tool))
            .collect();
        // 写快照:版本号单调递增,让 manager 能识别「刷新早于启动收尾」的
        // 竞态(收尾时按版本取最新清单)。
        {
            let mut snapshots = self.tool_snapshots.write().await;
            let entry = snapshots
                .entry(self.server_name.clone())
                .or_insert((0, Vec::new()));
            entry.0 += 1;
            entry.1 = descriptors.clone();
        }
        let _ = self
            .lifecycle_tx
            .send(McpLifecycleEvent::ToolsChanged {
                server: self.server_name.clone(),
                tools: descriptors,
            })
            .await;
    }

    async fn create_message(
        &self,
        _params: CreateMessageRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<CreateMessageResult, RmcpError> {
        // 兜底拒绝:能力协商已省略 sampling,这里防止不合规服务端把请求挂死。
        Err(RmcpError::method_not_found::<
            rmcp::model::CreateMessageRequestMethod,
        >())
    }
}

impl HostClientHandler {
    /// 发一条 `EventMsg::McpElicitation` 并等宿主回执。
    ///
    /// waiter 登记与 TTL 兜底在 `zhuye-exec` 的桥接任务里做(它同时握着协议
    /// 事件通道与会话级等待表);这里只管「发出去 / 拿到裁决」。
    async fn ask(
        &self,
        message: String,
        kind: McpElicitationKind,
        require_content: bool,
    ) -> Result<ElicitResult, RmcpError> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel::<McpElicitationResponse>();
        let event = McpElicitationEvent {
            request_id: request_id.clone(),
            server: self.server_name.clone(),
            message,
            kind,
        };
        if self
            .lifecycle_tx
            .send(McpLifecycleEvent::Elicitation { event, respond: tx })
            .await
            .is_err()
        {
            // 桥接任务已退出(会话收尾):按 fail-closed 拒绝,不等回执。
            return Ok(ElicitResult::new(ElicitationAction::Decline));
        }
        let response = rx.await.unwrap_or_else(|_| McpElicitationResponse {
            id: request_id.clone(),
            action: "cancel".into(),
            content: None,
        });
        match response.action.as_str() {
            "accept" => Ok(match response.content {
                Some(content) => ElicitResult::new(ElicitationAction::Accept).with_content(content),
                // form 形态必须有承载输入的对象的;宿主漏给时补空对象,
                // 让服务端按 schema 报错而不是收到结构异常的响应。
                None if require_content => {
                    ElicitResult::new(ElicitationAction::Accept).with_content(serde_json::json!({}))
                }
                None => ElicitResult::new(ElicitationAction::Accept),
            }),
            "decline" => Ok(ElicitResult::new(ElicitationAction::Decline)),
            // 其它值(含 TTL / 通道断开的内部 cancel)统一按协议 cancel 上送。
            _ => Ok(ElicitResult::new(ElicitationAction::Cancel)),
        }
    }
}

/// 服务端 elicitation schema(扁平对象 + 原始类型属性)→ 宿主表单字段。
fn elicitation_form(schema: &ElicitationSchema) -> McpElicitationKind {
    let required = schema.required.clone().unwrap_or_default();
    // `property_order` 保留服务端声明顺序;由 BTreeMap 构造时为空,退回键序。
    let names: Vec<String> = match &schema.property_order {
        Some(order) if !order.is_empty() => order.clone(),
        _ => schema.properties.keys().cloned().collect(),
    };
    let fields = names
        .iter()
        .filter_map(|name| {
            let property = schema.properties.get(name)?;
            let shape = serde_json::to_value(property).unwrap_or(serde_json::Value::Null);
            let lowered = name.to_ascii_lowercase();
            let secret = ["password", "secret", "token"]
                .iter()
                .any(|needle| lowered.contains(needle));
            Some(McpElicitationField {
                name: name.clone(),
                title: string_field(&shape, "title"),
                description: string_field(&shape, "description"),
                required: required.iter().any(|r| r == name),
                kind: field_kind(&shape, secret),
            })
        })
        .collect();
    McpElicitationKind::Form { fields }
}

/// 单个属性的 schema → 字段类型。先认候选项形态(`enum` / `anyOf` const),
/// 再按 `type` 落到文本 / 数值 / 布尔。
fn field_kind(shape: &serde_json::Value, secret: bool) -> McpElicitationFieldKind {
    if let Some(options) = enum_options(shape.get("enum")) {
        return McpElicitationFieldKind::Enum {
            options,
            multi: false,
        };
    }
    if let Some(options) = const_options(shape.get("anyOf")) {
        return McpElicitationFieldKind::Enum {
            options,
            multi: true,
        };
    }
    match shape.get("type").and_then(serde_json::Value::as_str) {
        Some("number") => McpElicitationFieldKind::Number { integer: false },
        Some("integer") => McpElicitationFieldKind::Number { integer: true },
        Some("boolean") => McpElicitationFieldKind::Boolean,
        _ => McpElicitationFieldKind::Text { secret },
    }
}

fn string_field(shape: &serde_json::Value, key: &str) -> Option<String> {
    shape
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn enum_options(values: Option<&serde_json::Value>) -> Option<Vec<String>> {
    let items = values?.as_array()?;
    let options: Vec<String> = items
        .iter()
        .filter_map(|item| item.as_str().map(str::to_string))
        .collect();
    (!options.is_empty()).then_some(options)
}

/// 多选形态:`{"anyOf": [{"const": "a", "title": "..."}, ...]}`。
fn const_options(items: Option<&serde_json::Value>) -> Option<Vec<String>> {
    let items = items?.as_array()?;
    let options: Vec<String> = items
        .iter()
        .filter_map(|item| {
            item.get("const")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect();
    (!options.is_empty()).then_some(options)
}

/// 已构造好的 transport 与一个 `RunningService<RoleClient, ()>`
/// —— 本 crate 不暴露 rmcp 类型穿透到 caller;`manager.rs` 直接消费。
pub struct McpTransportHandle {
    /// `Peer<RoleClient>`,由 manager 与 tool adapter 共享。
    pub peer: Arc<rmcp::service::Peer<rmcp::RoleClient>>,
    /// shutdown 时打断 in-flight call。`()` handler 没实现 ping/anything,
    /// 我们用 `CancellationToken` 走双 cancel(manager shutdown + ctx.cancel)。
    pub cancel: tokio_util::sync::CancellationToken,
}

/// 根据 transport 类型分发构造。
#[allow(clippy::too_many_arguments)]
pub async fn build(
    server_name: &str,
    transport: McpTransport,
    command: Option<&str>,
    args: &[String],
    env: &HashMap<String, String>,
    url: Option<&str>,
    headers: &HashMap<String, String>,
    lifecycle_tx: mpsc::Sender<McpLifecycleEvent>,
    tool_snapshots: Arc<tokio::sync::RwLock<HashMap<String, crate::manager::ToolSnapshot>>>,
    oauth: Option<crate::config::McpOAuthConfig>,
) -> Result<McpTransportHandle, McpError> {
    match transport {
        McpTransport::Stdio => {
            build_stdio(
                server_name,
                command,
                args,
                env,
                lifecycle_tx,
                tool_snapshots,
            )
            .await
        }
        McpTransport::Http => {
            build_http(
                server_name,
                url,
                headers,
                lifecycle_tx,
                tool_snapshots,
                oauth,
            )
            .await
        }
        McpTransport::Sse => {
            build_sse(
                server_name,
                url,
                headers,
                lifecycle_tx,
                tool_snapshots,
                oauth,
            )
            .await
        }
    }
}

/// 构造 stdio transport。
///
/// 完整流程:
/// 1. 构造 `tokio::process::Command`,设 `stdin/stdout=Stdio::piped()`,
///    `stderr=Stdio::inherit()`(让 MCP server 自己的 banner 可见);
/// 2. `env_clear()` 后注入 `HOME`/`PATH` + 用户 env;
/// 3. spawn 出 `tokio::process::Child`;
/// 4. 用 `TokioChildProcess` 包装 `(read, write)`;
/// 5. `().serve(transport)` 跑 initialize 握手。
#[allow(clippy::too_many_arguments)]
pub async fn build_stdio(
    server_name: &str,
    command: Option<&str>,
    args: &[String],
    env: &HashMap<String, String>,
    lifecycle_tx: mpsc::Sender<McpLifecycleEvent>,
    tool_snapshots: Arc<tokio::sync::RwLock<HashMap<String, crate::manager::ToolSnapshot>>>,
) -> Result<McpTransportHandle, McpError> {
    let cmd_str =
        command.ok_or_else(|| McpError::ConfigInvalid("stdio requires `command`".to_string()))?;
    // 展开 command 与 args 里的环境变量($VAR / ${VAR})。
    // 用户常在 config 写 `args = ["$HOME"]` 指代家目录;不展开的话子进程会收到
    // 字面量 "$HOME",拼出非法路径(`<cwd>/$HOME`)。展开发生在 spawn 前,
    // 用的是注入后的 env(含 HOME/PATH + 用户声明的 env)。
    let cmd_str = expand_env_vars(cmd_str, env);
    let expanded_args: Vec<String> = args.iter().map(|a| expand_env_vars(a, env)).collect();
    let mut cmd = Command::new(&cmd_str);
    cmd.args(&expanded_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        // Zhuye 主进程 panic / cancel 时,kill 子进程避免 zombie。
        // tokio::process::Command::kill_on_drop 自 1.21 起稳定。
        .kill_on_drop(true);
    inject_safe_env(&mut cmd, env);
    let transport = TokioChildProcess::new(cmd).map_err(McpError::Spawn)?;
    let cancel = tokio_util::sync::CancellationToken::new();
    let handler = HostClientHandler {
        server_name: server_name.to_string(),
        lifecycle_tx,
        tool_snapshots,
    };
    let running = rmcp::service::ServiceExt::serve(handler, transport)
        .await
        .map_err(|e| McpError::Initialize(format!("stdio serve: {e}")))?;
    // Peer<R> derives Clone (rmcp 1.7);抽出 owned Peer 用 Arc 包好共享。
    let peer = Arc::new(running.peer().clone());
    // 同时等待主动关闭和对端断开；对端断开时通知所有使用该连接的调用。
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = cancel_clone.cancelled() => {},
            _ = running.waiting() => cancel_clone.cancel(),
        }
    });
    Ok(McpTransportHandle { peer, cancel })
}

/// 构造 streamable-http transport。
///
/// `headers["Authorization"]` 自动剥离到 `auth_header`,其余 header
/// 透传到 `custom_headers`。
#[allow(clippy::too_many_arguments)]
pub async fn build_http(
    server_name: &str,
    url: Option<&str>,
    headers: &HashMap<String, String>,
    lifecycle_tx: mpsc::Sender<McpLifecycleEvent>,
    tool_snapshots: Arc<tokio::sync::RwLock<HashMap<String, crate::manager::ToolSnapshot>>>,
    oauth: Option<crate::config::McpOAuthConfig>,
) -> Result<McpTransportHandle, McpError> {
    let url_str = url.ok_or_else(|| McpError::ConfigInvalid("http requires `url`".to_string()))?;
    let mut cfg = StreamableHttpClientTransportConfig::with_uri(url_str);
    let mut custom_headers: std::collections::HashMap<
        reqwest::header::HeaderName,
        reqwest::header::HeaderValue,
    > = std::collections::HashMap::new();
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("authorization") {
            // 去掉 "Bearer " 前缀,rmcp 内部会自己加回去。
            let token = v.strip_prefix("Bearer ").unwrap_or(v.as_str()).to_string();
            cfg = cfg.auth_header(token);
        } else {
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| McpError::ConfigInvalid(format!("bad header name {k:?}: {e}")))?;
            let value = reqwest::header::HeaderValue::from_str(v)
                .map_err(|e| McpError::ConfigInvalid(format!("bad header value {k:?}: {e}")))?;
            custom_headers.insert(name, value);
        }
    }
    cfg = cfg.custom_headers(custom_headers);
    let reqwest_client = reqwest::Client::new();
    // OAuth:静态 token / 环境变量 / 客户端凭据换 token。
    // 拿不到 token 不直接失败 —— 先按匿名握手,服务端 401 时再按
    // 「需要授权」上报(见下方握手错误分支),避免无谓的预取请求。
    let mut token_error: Option<String> = None;
    if let Some(oauth) = oauth.as_ref() {
        match crate::oauth::resolve_token(&reqwest_client, oauth, url_str).await {
            Ok(Some(token)) => cfg = cfg.auth_header(token),
            Ok(None) => {}
            Err(error) => {
                // 解析失败不直接终止:先匿名握手,服务端要求授权时再把这条
                // 原因一并上报(宿主据此看到「为什么没带上 token」)。
                token_error = Some(error.to_string());
            }
        }
    }
    let transport = StreamableHttpClientTransport::with_client(reqwest_client, cfg);
    let cancel = tokio_util::sync::CancellationToken::new();
    let handler = HostClientHandler {
        server_name: server_name.to_string(),
        lifecycle_tx,
        tool_snapshots,
    };
    let running = rmcp::service::ServiceExt::serve(handler, transport)
        .await
        .map_err(|e| {
            // 401:服务端要求授权。按受保护资源元数据发现端点,把可执行的
            // 信息交给宿主(引擎不代跑浏览器)。
            let message = format!("http serve: {e}");
            // rmcp 把 401 归一成 "Auth required";同时兼容裸 401 / Unauthorized。
            let auth_hint = ["401", "Unauthorized", "Auth required", "authenticate"]
                .iter()
                .any(|needle| message.contains(needle));
            if auth_hint {
                // 端点发现是网络 I/O,不能在错误构造里 await;这里给出可执行的
                // 静态提示,精确端点由 resolve_token(有凭据时)或宿主侧发现。
                let reason = match &token_error {
                    Some(reason) => format!("(取 token 失败:{reason})"),
                    None => String::new(),
                };
                return McpError::AuthRequired(format!(
                    "{message}{reason};该 MCP server 需要 OAuth 授权 —— 请在 [mcp_servers.<name>.oauth] \
                 配置 token / token_env / client_id(+client_secret),或用授权码流程取得 token 后 \
                 通过 headers.Authorization 提供"
                ));
            }
            McpError::Initialize(message)
        })?;
    let peer = Arc::new(running.peer().clone());
    // 与 stdio 一致，在主动关闭或服务端断开时结束连接。
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = cancel_clone.cancelled() => {},
            _ = running.waiting() => cancel_clone.cancel(),
        }
    });
    Ok(McpTransportHandle { peer, cancel })
}

/// 构造 legacy MCP HTTP+SSE transport。
///
/// v1+ 阶段:legacy 纯 SSE endpoint 协议与 streamable-http 在 wire 层相近,
/// 此处复用 `StreamableHttpClientTransport`(内部走 SSE 长连接)。若服务端
/// 仅支持旧式 `endpoint` 事件握手,请改用 `type = "stdio"` 或升级 server。
#[allow(clippy::too_many_arguments)]
pub async fn build_sse(
    server_name: &str,
    url: Option<&str>,
    headers: &HashMap<String, String>,
    lifecycle_tx: mpsc::Sender<McpLifecycleEvent>,
    tool_snapshots: Arc<tokio::sync::RwLock<HashMap<String, crate::manager::ToolSnapshot>>>,
    oauth: Option<crate::config::McpOAuthConfig>,
) -> Result<McpTransportHandle, McpError> {
    tracing::debug!("mcp transport=sse: delegating to streamable-http SSE client");
    build_http(
        server_name,
        url,
        headers,
        lifecycle_tx,
        tool_snapshots,
        oauth,
    )
    .await
}

/// 安全注入 env:`env_clear()` 后只加 `HOME`/`PATH` + 用户 env。
///
/// `HOME` 兜底 `tmpdir()` 以保证 stdio 子进程能找到配置文件。
/// `PATH` 直接复用父进程当前值(进程级 PATH 一般无害)。
fn inject_safe_env(cmd: &mut Command, user_env: &HashMap<String, String>) {
    cmd.env_clear();
    if let Ok(home) = std::env::var("HOME") {
        cmd.env("HOME", home);
    } else if let Some(dir) = std::env::temp_dir().to_str() {
        // tmpdir() 退路 —— 测试 / sandbox 环境可能 HOME 未设。
        cmd.env("HOME", dir);
    }
    // PATH: 优先父进程,无则空(子进程可执行性会立即失败,易诊断)。
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    // Windows 运行时候选变量:env_clear 后缺 SystemRoot / windir 会让
    // node.exe 等 CRT 程序在输出任何字节前就崩溃,父进程侧只见 stdout
    // EOF(表现为连接错误而非 spawn 错误,极难归因)。只透传父进程现有
    // 值,不伪造路径。与 zhuye-lsp::transport::inject_safe_env 同口径。
    #[cfg(windows)]
    for key in [
        "SystemRoot",
        "windir",
        "ComSpec",
        "PATHEXT",
        "USERPROFILE",
        "TEMP",
        "TMP",
        "APPDATA",
        "LOCALAPPDATA",
        "HOMEDRIVE",
        "HOMEPATH",
    ] {
        if let Ok(value) = std::env::var(key) {
            cmd.env(key, value);
        }
    }
    for (k, v) in user_env {
        cmd.env(k, v);
    }
}

/// 展开 string 里的环境变量 `$VAR` / `${VAR}` 为对应值。
///
/// 查询顺序:`user_env`(用户在 `[mcp_servers.x].env` 显式声明的)优先,
/// 其次父进程 `std::env::var`。未定义的变量替换为空字符串(与 shell 行为
/// 一致:`echo $UNDEFINED` 输出空)。
///
/// 仅做变量展开,不支持命令替换/通配符/管道等 shell 特性 —— MCP server
/// 的 command/args 不该有 shell 元语义,展开 `$VAR` 已覆盖 99% 用例
/// (典型:`$HOME`、`$PROJECT_DIR`)。
fn expand_env_vars(s: &str, user_env: &HashMap<String, String>) -> String {
    // 没有 $ 直接返回,避免无谓分配。
    if !s.contains('$') {
        return s.to_string();
    }
    let lookup = |name: &str| -> String {
        user_env
            .get(name)
            .cloned()
            .or_else(|| std::env::var(name).ok())
            .unwrap_or_default()
    };
    let mut out = String::with_capacity(s.len());
    // review 修复:此前按字节遍历且 `out.push(bytes[i] as char)`,非 ASCII
    // 字节会被错误映射成 Latin-1 码点(与 `$HOME` 混排的中文路径整体
    // mojibake)。改为 `char_indices` 按字符边界推进,非 `$` 字符原样拷贝。
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        // 此处 i 是 '$' 的字节索引,`s[i+1..]` 必在字符边界上。
        let rest = &s[i + 1..];
        if let Some(stripped) = rest.strip_prefix('{') {
            if let Some(close) = stripped.find('}') {
                let name = &stripped[..close];
                out.push_str(&lookup(name));
                // 消费到 '}'(绝对字节索引 i+2+close)为止。
                let target = i + 2 + close;
                while let Some(&(idx, _)) = chars.peek() {
                    if idx > target {
                        break;
                    }
                    chars.next();
                }
                continue;
            }
            // 未闭合的 `${`,按普通字符输出,'{' 交由后续迭代处理。
            out.push(c);
            continue;
        }
        // $VAR 形式(字母/数字/下划线)
        let end = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        if end > 0 {
            let name = &rest[..end];
            out.push_str(&lookup(name));
            // 消费 name 的全部字符(最后一个字节在绝对索引 i+end)。
            let target = i + end;
            while let Some(&(idx, _)) = chars.peek() {
                if idx > target {
                    break;
                }
                chars.next();
            }
            continue;
        }
        // 孤立 `$`,原样输出(与 shell 的 `echo $` 行为一致)。
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `inject_safe_env` 不会泄漏父进程的 API key 到子进程。
    /// 通过观察最终 child 的 env 间接断言 —— 我们只验证函数本身
    /// 调用了 `env_clear` (不会保留父进程 `OPENAI_API_KEY`)。
    #[test]
    fn mcp_transport_sse_variant_exists() {
        assert_eq!(
            McpTransport::Sse,
            McpTransport::from(zhuye_config::McpTransport::Sse)
        );
    }

    #[test]
    fn inject_safe_env_drops_parent_env() {
        // SAFETY: 测试中串行修改 env 在单测下可控。
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "sk-leak-test");
            std::env::set_var("HOME", "/tmp/test-home");
            std::env::set_var("PATH", "/usr/bin");
        }
        let mut cmd = Command::new("echo");
        let mut user_env = HashMap::new();
        user_env.insert("MY_KEY".to_string(), "v".to_string());
        inject_safe_env(&mut cmd, &user_env);
        let envs: Vec<(&str, &str)> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| v.map(|vv| (k.to_str().unwrap_or(""), vv.to_str().unwrap_or(""))))
            .collect();
        // 父进程的 OPENAI_API_KEY 一定不在;用户注入的 MY_KEY 在。
        assert!(
            !envs.iter().any(|(k, _)| *k == "OPENAI_API_KEY"),
            "OPENAI_API_KEY leaked into child env: {envs:?}"
        );
        assert!(envs.iter().any(|(k, _)| *k == "MY_KEY"));
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
        }
    }

    /// Windows 回归:env_clear 后运行时候选变量必须透传 —— 缺 SystemRoot
    /// 时 node.exe 等 CRT 程序会在输出任何字节前崩溃(表现为 EOF,与凭证
    /// 透传无关)。与 zhuye-lsp::transport 同名测试同口径。
    #[cfg(windows)]
    #[test]
    fn inject_safe_env_keeps_windows_runtime_vars() {
        let mut cmd = Command::new("cmd.exe");
        inject_safe_env(&mut cmd, &HashMap::new());
        let keys: Vec<String> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| v.map(|_| k.to_string_lossy().into_owned()))
            .collect();
        assert!(
            keys.iter().any(|k| k.eq_ignore_ascii_case("SystemRoot")),
            "SystemRoot 未透传,Windows 子进程 CRT 起不来: {keys:?}"
        );
        assert!(
            !keys
                .iter()
                .any(|k| k.eq_ignore_ascii_case("OPENAI_API_KEY")),
            "白名单不得放行凭证类变量: {keys:?}"
        );
    }

    #[test]
    fn expand_env_vars_dollar_brace_form() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        assert_eq!(expand_env_vars("${FOO}/x", &env), "bar/x");
        assert_eq!(expand_env_vars("pre-${FOO}-post", &env), "pre-bar-post");
    }

    #[test]
    fn expand_env_vars_bare_dollar_form() {
        let mut env = HashMap::new();
        env.insert("HOME".to_string(), "/Users/test".to_string());
        assert_eq!(expand_env_vars("$HOME/sub", &env), "/Users/test/sub");
        assert_eq!(expand_env_vars("--root $HOME", &env), "--root /Users/test");
    }

    #[test]
    fn expand_env_vars_undefined_becomes_empty() {
        let env = HashMap::new();
        // 未定义变量 → 空字符串(与 shell 一致)。
        assert_eq!(expand_env_vars("$UNDEFINED_VAR/x", &env), "/x");
        assert_eq!(expand_env_vars("${UNDEFINED_VAR}", &env), "");
    }

    #[test]
    fn expand_env_vars_no_dollar_passthrough() {
        let env = HashMap::new();
        // 无 $ 直接返回原值(不分配)。
        assert_eq!(expand_env_vars("plain-string", &env), "plain-string");
        assert_eq!(expand_env_vars("/usr/bin/npx", &env), "/usr/bin/npx");
    }

    #[test]
    fn expand_env_vars_user_env_overrides_parent() {
        // user_env 优先级高于父进程 std::env::var。
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), "/custom/path".to_string());
        assert_eq!(expand_env_vars("$PATH", &env), "/custom/path");
    }
}
