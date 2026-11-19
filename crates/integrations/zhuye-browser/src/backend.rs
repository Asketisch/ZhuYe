//! 后端选择:`real` feature 开启 → CDP 真实后端;关闭 → 统一 Unavailable
//! 降级(不 panic,工具返回 isError 文案)。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
#[cfg(feature = "real")]
use serde_json::{Value, json};

use crate::error::BrowserError;

/// 浏览器控制能力等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserCapability {
    /// 无能力(降级态;所有操作返回 Unavailable)。
    None,
    /// CDP 控制可用(浏览器可启动 / 可 attach)。
    Full,
}

/// 浏览器页面状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserStatus {
    /// 当前 URL(未连接时为 None)。
    pub url: Option<String>,
    /// 页面标题(未连接时为 None)。
    pub title: Option<String>,
    /// 浏览器是否在运行(有活跃 CDP 连接的目标)。
    pub running: bool,
}

/// 浏览器控制后端 trait。
///
/// `reference` 参数一律来自 `snapshot` 返回的 `@eN` 引用,且必须携带
/// 取到该引用时的快照版本(`snapshot_version`)—— F39 快照绑定:界面
/// 变化后旧 ref 显式失效,防止"点到另一个同名按钮"。
#[async_trait]
pub trait BrowserBackend: Send + Sync {
    /// 当前能力。
    fn capability(&self) -> BrowserCapability;
    /// 页面状态(url / title / running)。
    async fn status(&self) -> Result<BrowserStatus, BrowserError>;
    /// 导航到 URL。
    async fn navigate(&self, url: &str) -> Result<(), BrowserError>;
    /// 拍可及性快照(返回版本号供后续动作携带)。
    async fn snapshot(&self) -> Result<zhuye_computer_use::A11ySnapshot, BrowserError>;
    /// 点击元素。
    async fn click(&self, reference: &str, snapshot_version: u64) -> Result<(), BrowserError>;
    /// 填写输入框(聚焦 → 全选 → 键入,覆盖原值)。
    async fn fill(
        &self,
        reference: &str,
        text: &str,
        snapshot_version: u64,
    ) -> Result<(), BrowserError>;
    /// 按组合键(`"ctrl+c"` / `"enter"` / `"tab"`)。
    async fn press_key(&self, keys: &str) -> Result<(), BrowserError>;
    /// 滚轮滚动(正 dy = 下,正 dx = 右;与 CDP mouseWheel 语义一致)。
    async fn scroll(&self, dx: i32, dy: i32) -> Result<(), BrowserError>;
    /// 截图(JPEG bytes)。
    async fn screenshot(&self) -> Result<Vec<u8>, BrowserError>;
    /// 关闭当前受控标签页(headless 下最后一个标签关闭即浏览器退出)。
    async fn close(&self) -> Result<(), BrowserError>;
}

/// 进程级共享后端(按 feature 决定真实 / 降级)。
///
/// `real` 关闭时返回降级后端;开启时返回 CDP 后端(连接惰性建立,
/// 首个工具调用时 attach / launch)。
pub fn browser_backend() -> std::sync::Arc<dyn BrowserBackend> {
    #[cfg(feature = "real")]
    {
        std::sync::Arc::new(CdpBrowserBackend::new())
    }
    #[cfg(not(feature = "real"))]
    {
        std::sync::Arc::new(UnavailableBrowserBackend)
    }
}

/// 降级后端:`real` feature 关闭时全部操作返回 `Unavailable`。
pub struct UnavailableBrowserBackend;

/// 统一的降级实现:`real` 关闭时逐能力返回 Unavailable 文案。
macro_rules! degraded {
    ($self:expr, $capability:expr) => {
        Err(BrowserError::unavailable(
            $capability,
            "built without `real` feature (no CDP); rebuild with --features real",
        ))
    };
}

#[async_trait]
impl BrowserBackend for UnavailableBrowserBackend {
    fn capability(&self) -> BrowserCapability {
        BrowserCapability::None
    }

    async fn status(&self) -> Result<BrowserStatus, BrowserError> {
        degraded!(self, "status")
    }

    async fn navigate(&self, _url: &str) -> Result<(), BrowserError> {
        degraded!(self, "navigate")
    }

    async fn snapshot(&self) -> Result<zhuye_computer_use::A11ySnapshot, BrowserError> {
        degraded!(self, "snapshot")
    }

    async fn click(&self, _reference: &str, _snapshot_version: u64) -> Result<(), BrowserError> {
        degraded!(self, "click")
    }

    async fn fill(
        &self,
        _reference: &str,
        _text: &str,
        _snapshot_version: u64,
    ) -> Result<(), BrowserError> {
        degraded!(self, "fill")
    }

    async fn press_key(&self, _keys: &str) -> Result<(), BrowserError> {
        degraded!(self, "press_key")
    }

    async fn scroll(&self, _dx: i32, _dy: i32) -> Result<(), BrowserError> {
        degraded!(self, "scroll")
    }

    async fn screenshot(&self) -> Result<Vec<u8>, BrowserError> {
        degraded!(self, "screenshot")
    }

    async fn close(&self) -> Result<(), BrowserError> {
        degraded!(self, "close")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn degraded_backend_reports_unavailable_not_panic() {
        // 无 `real` feature(默认)时:能力 None,操作返回 Unavailable。
        let backend = UnavailableBrowserBackend;
        if backend.capability() == BrowserCapability::None {
            let err = backend.snapshot().await.unwrap_err();
            assert!(
                matches!(err, BrowserError::Unavailable { ref capability, .. } if capability == "snapshot")
            );
            let err = backend
                .navigate("https://example.invalid")
                .await
                .unwrap_err();
            assert!(matches!(err, BrowserError::Unavailable { .. }));
        }
    }

    #[test]
    fn browser_error_display_is_actionable() {
        let e = BrowserError::unavailable("screenshot", "no browser found");
        assert!(e.to_string().contains("screenshot"));
        assert!(e.to_string().contains("no browser found"));
    }
}

// ====== CDP 真实后端(`real` feature)======

/// CDP 浏览器控制后端。
///
/// 连接惰性建立:首个工具调用时 attach 运行中实例,失败则启动 headless
/// 新实例(隔离 profile,`DevToolsActivePort` 发现契约与桌面 browser_backend
/// 共享)。快照引用绑定 F39 协议:`@eN` 只在取到它的快照版本内有效。
#[cfg(feature = "real")]
pub struct CdpBrowserBackend {
    inner: tokio::sync::Mutex<Inner>,
}

#[cfg(feature = "real")]
struct Inner {
    conn: Option<crate::cdp::CdpConnection>,
    session_id: Option<String>,
    target_id: Option<String>,
    /// 我们拉起的实例(attach 的运行实例为 None;Drop 时回收 headless)。
    process: Option<crate::chrome::BrowserProcess>,
    /// 当前快照版本(0 = 尚未拍过快照)。
    snapshot_version: u64,
    /// 快照绑定(ref 只在本版本内有效)。
    binding: Option<zhuye_computer_use::SnapshotBinding>,
    /// ref → 元素定位(nodeId + 视口中心点),随快照重建。
    elements: std::collections::HashMap<String, ElementInfo>,
}

#[cfg(feature = "real")]
#[derive(Clone)]
struct ElementInfo {
    /// CDP AX nodeId(保留给后续 `DOM` 精确定位;当前动作走 bounds 中心)。
    #[allow(dead_code)]
    node_id: String,
    center: (f64, f64),
}

/// v1 安全约束:仅回环 + 单 target 强制;`require_token` 不启用 —— 原生
/// CDP 无鉴权 token 机制,隔离 profile + loopback 是当前边界(token 代理
/// 为后续项,接口保留在 `BrowserSecurity.require_token`)。
#[cfg(feature = "real")]
const SECURITY: zhuye_computer_use::BrowserSecurity = zhuye_computer_use::BrowserSecurity {
    single_target: true,
    require_token: false,
    loopback_only: true,
};

/// 快照元素数上限(超大页面防输出爆炸)。
#[cfg(feature = "real")]
const SNAPSHOT_ELEMENT_CAP: usize = 300;

#[cfg(feature = "real")]
impl Default for CdpBrowserBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "real")]
impl CdpBrowserBackend {
    pub fn new() -> Self {
        CdpBrowserBackend {
            inner: tokio::sync::Mutex::new(Inner {
                conn: None,
                session_id: None,
                target_id: None,
                process: None,
                snapshot_version: 0,
                binding: None,
                elements: std::collections::HashMap::new(),
            }),
        }
    }

    /// 确保已连接并 attach 到一个 page target(失败则启动新实例)。
    async fn ensure_attached(&self, inner: &mut Inner) -> Result<(), BrowserError> {
        if inner.conn.is_some() && inner.session_id.is_some() {
            return Ok(());
        }
        inner.conn = None;
        inner.session_id = None;
        inner.target_id = None;
        inner.binding = None;
        inner.elements.clear();

        let profile = crate::chrome::profile_dir();
        // 1) 已有实例 → DevToolsActivePort attach。
        let port_file = profile.join("DevToolsActivePort");
        if port_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&port_file) {
                if let Some((port, path)) = crate::chrome::parse_devtools_active_port(&content) {
                    let ws_url = crate::chrome::browser_ws_url(port, &path);
                    match crate::chrome::query_page_targets(&ws_url).await {
                        Ok(_) => {
                            self.attach_to_browser(inner, &ws_url).await?;
                            return Ok(());
                        }
                        Err(err) => {
                            tracing::warn!(target: "zhuye_browser", %err, "已有实例 attach 失败,转为启动新实例");
                            // 陈旧端口文件 → 清掉,防新实例 singleton 冲突残留。
                            let _ = std::fs::remove_file(&port_file);
                        }
                    }
                }
            }
        }
        // 2) 无运行实例 → 启动 headless 新实例。
        let executable = crate::chrome::discover_executable().ok_or_else(|| {
            BrowserError::unavailable(
                "browser",
                "本机未找到 Chrome/Chromium/Edge;可设 ZHUYE_BROWSER_EXECUTABLE 指向可执行文件",
            )
        })?;
        let process = crate::chrome::launch(&executable, &profile, true)?;
        let (port, path) = process.devtools_port()?;
        let ws_url = crate::chrome::browser_ws_url(port, &path);
        self.attach_to_browser(inner, &ws_url).await?;
        inner.process = Some(process);
        Ok(())
    }

    /// 连接浏览器级 ws → 单 target 校验 → attachToTarget(flatten)。
    async fn attach_to_browser(&self, inner: &mut Inner, ws_url: &str) -> Result<(), BrowserError> {
        let host = crate::cdp::extract_host(ws_url)?;
        let targets = crate::chrome::query_page_targets(ws_url).await?;
        // has_token=false:v1 不启用 token(见 SECURITY 注释)。
        SECURITY
            .validate(targets.len(), &host, false)
            .map_err(BrowserError::other)?;
        let target = targets
            .first()
            .ok_or_else(|| BrowserError::other("浏览器无 page target(可能只剩扩展后台页)"))?;
        let target_id = target
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::other("targetId 缺失"))?
            .to_string();

        let mut conn = crate::cdp::CdpConnection::connect(ws_url).await?;
        let result = conn
            .command(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        let session_id = result
            .as_str()
            .ok_or_else(|| BrowserError::other("attachToTarget 未返回 sessionId"))?
            .to_string();
        inner.conn = Some(conn);
        inner.session_id = Some(session_id);
        inner.target_id = Some(target_id);
        Ok(())
    }

    /// 带 session 的命令(内部连接必须已建立)。
    async fn target_command(
        &self,
        inner: &mut Inner,
        method: &str,
        params: Value,
    ) -> Result<Value, BrowserError> {
        let session = inner
            .session_id
            .clone()
            .ok_or_else(|| BrowserError::other("未连接浏览器"))?;
        inner
            .conn
            .as_mut()
            .ok_or_else(|| BrowserError::other("未连接浏览器"))?
            .command_on_target(&session, method, params)
            .await
    }
}

#[cfg(feature = "real")]
#[async_trait]
impl BrowserBackend for CdpBrowserBackend {
    fn capability(&self) -> BrowserCapability {
        BrowserCapability::Full
    }

    /// 页面状态。**不主动拉起浏览器**(区别于其他操作):未连接时如实
    /// 报 running=false,避免查询状态就有启动副作用。
    async fn status(&self) -> Result<BrowserStatus, BrowserError> {
        let mut inner = self.inner.lock().await;
        if inner.conn.is_none() || inner.session_id.is_none() {
            return Ok(BrowserStatus {
                url: None,
                title: None,
                running: false,
            });
        }
        let result = self
            .target_command(
                &mut inner,
                "Runtime.evaluate",
                json!({
                    "expression": "JSON.stringify({url: location.href, title: document.title})",
                    "returnByValue": true,
                }),
            )
            .await?;
        let raw = result
            .pointer("/result/value")
            .and_then(Value::as_str)
            .unwrap_or("{}");
        let parsed: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
        Ok(BrowserStatus {
            url: parsed.get("url").and_then(Value::as_str).map(String::from),
            title: parsed
                .get("title")
                .and_then(Value::as_str)
                .map(String::from),
            running: true,
        })
    }

    async fn navigate(&self, url: &str) -> Result<(), BrowserError> {
        let mut inner = self.inner.lock().await;
        self.ensure_attached(&mut inner).await?;
        self.target_command(&mut inner, "Page.navigate", json!({ "url": url }))
            .await?;
        // 等文档 load 完成(最多 8s),让 snapshot 的语义紧随导航。
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
        loop {
            let ready = self
                .target_command(
                    &mut inner,
                    "Runtime.evaluate",
                    json!({ "expression": "document.readyState", "returnByValue": true }),
                )
                .await;
            match ready {
                Ok(v) if v.pointer("/result/value").and_then(Value::as_str) == Some("complete") => {
                    break;
                }
                _ if tokio::time::Instant::now() >= deadline => break,
                _ => tokio::time::sleep(std::time::Duration::from_millis(150)).await,
            }
        }
        // 导航使旧快照失效:显式清绑定,旧 ref 报「先取快照」而非点错。
        inner.binding = None;
        inner.elements.clear();
        Ok(())
    }

    async fn snapshot(&self) -> Result<zhuye_computer_use::A11ySnapshot, BrowserError> {
        let mut inner = self.inner.lock().await;
        self.ensure_attached(&mut inner).await?;
        let tree = self
            .target_command(&mut inner, "Accessibility.getFullAXTree", json!({}))
            .await?;
        let title = self
            .target_command(
                &mut inner,
                "Runtime.evaluate",
                json!({ "expression": "document.title", "returnByValue": true }),
            )
            .await
            .ok()
            .and_then(|v| {
                v.pointer("/result/value")
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .unwrap_or_default();
        let snapshot = map_ax_tree(&tree, title, inner.snapshot_version + 1)?;
        // 重建绑定与 ref → 定位映射(旧 ref 全部失效)。
        inner.snapshot_version = snapshot.version;
        inner.binding = Some(zhuye_computer_use::SnapshotBinding::new(snapshot.version));
        inner.elements.clear();
        for (reference, info) in &snapshot_element_index(&tree)? {
            if snapshot.find(reference).is_some() {
                inner.elements.insert(reference.clone(), info.clone());
            }
        }
        Ok(snapshot)
    }

    async fn click(&self, reference: &str, snapshot_version: u64) -> Result<(), BrowserError> {
        let mut inner = self.inner.lock().await;
        check_binding(&inner, reference, snapshot_version)?;
        let center = element_center(&inner, reference)?;
        dispatch_mouse_click(&mut inner, self, center.0, center.1).await
    }

    async fn fill(
        &self,
        reference: &str,
        text: &str,
        snapshot_version: u64,
    ) -> Result<(), BrowserError> {
        let mut inner = self.inner.lock().await;
        check_binding(&inner, reference, snapshot_version)?;
        let center = element_center(&inner, reference)?;
        dispatch_mouse_click(&mut inner, self, center.0, center.1).await?;
        // 全选旧值 → insertText 覆盖(macOS 有头实例 Cmd 语义差异:headless
        // 与 Linux/Windows 一致用 ctrl;macOS 有头场景由模型自行用 evaluate
        // 兜底 —— v1 覆盖 headless 主路径)。
        dispatch_key_press(&mut inner, self, "ctrl+a").await?;
        self.target_command(&mut inner, "Input.insertText", json!({ "text": text }))
            .await?;
        Ok(())
    }

    async fn press_key(&self, keys: &str) -> Result<(), BrowserError> {
        let mut inner = self.inner.lock().await;
        self.ensure_attached(&mut inner).await?;
        dispatch_key_press(&mut inner, self, keys).await
    }

    async fn scroll(&self, dx: i32, dy: i32) -> Result<(), BrowserError> {
        let mut inner = self.inner.lock().await;
        self.ensure_attached(&mut inner).await?;
        self.target_command(
            &mut inner,
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseWheel",
                "x": 400, "y": 300,
                "deltaX": dx, "deltaY": dy,
            }),
        )
        .await?;
        Ok(())
    }

    async fn screenshot(&self) -> Result<Vec<u8>, BrowserError> {
        let mut inner = self.inner.lock().await;
        self.ensure_attached(&mut inner).await?;
        let result = self
            .target_command(
                &mut inner,
                "Page.captureScreenshot",
                json!({ "format": "jpeg", "quality": 80 }),
            )
            .await?;
        let data = result
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::other("captureScreenshot 未返回 data"))?;
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data)
            .map_err(|e| BrowserError::other(format!("截图 base64 解码失败: {e}")))
    }

    async fn close(&self) -> Result<(), BrowserError> {
        let mut inner = self.inner.lock().await;
        self.ensure_attached(&mut inner).await?;
        let target_id = inner
            .target_id
            .clone()
            .ok_or_else(|| BrowserError::other("未连接浏览器"))?;
        inner
            .conn
            .as_mut()
            .ok_or_else(|| BrowserError::other("未连接浏览器"))?
            .command("Target.closeTarget", json!({ "targetId": target_id }))
            .await?;
        // headless 下最后一个标签关闭即浏览器退出;清连接态,下次调用
        // 重新走 attach/launch(陈旧端口文件已由 ensure_attached 清理)。
        inner.conn = None;
        inner.session_id = None;
        inner.target_id = None;
        inner.binding = None;
        inner.elements.clear();
        Ok(())
    }
}

// ====== CDP 实现的内部助手 ======

#[cfg(feature = "real")]
fn check_binding(
    inner: &Inner,
    reference: &str,
    snapshot_version: u64,
) -> Result<(), BrowserError> {
    let Some(binding) = inner.binding else {
        return Err(BrowserError::other(
            "尚未拍快照:先调用 browser_snapshot 获取元素引用",
        ));
    };
    // F39 快照绑定:ref 版本不匹配 → 显式陈旧错误(防点错同名元素)。
    zhuye_computer_use::SnapshotBinding::check(&binding, reference, snapshot_version)
        .map_err(|e| BrowserError::other(e.to_string()))
}

#[cfg(feature = "real")]
fn element_center(inner: &Inner, reference: &str) -> Result<(f64, f64), BrowserError> {
    let info = inner.elements.get(reference).ok_or_else(|| {
        BrowserError::other(format!(
            "引用 {reference} 不在当前快照中,请重新 browser_snapshot"
        ))
    })?;
    Ok(info.center)
}

#[cfg(feature = "real")]
async fn dispatch_mouse_click(
    inner: &mut Inner,
    backend: &CdpBrowserBackend,
    x: f64,
    y: f64,
) -> Result<(), BrowserError> {
    for mouse_type in ["mousePressed", "mouseReleased"] {
        backend
            .target_command(
                inner,
                "Input.dispatchMouseEvent",
                json!({
                    "type": mouse_type,
                    "x": x, "y": y,
                    "button": "left", "clickCount": 1,
                }),
            )
            .await?;
    }
    Ok(())
}

/// 按组合键:修饰键 bitmask + 主键 keyDown/keyUp(可打印主键带 text,
/// 让页面收到正确的字符输入)。
#[cfg(feature = "real")]
async fn dispatch_key_press(
    inner: &mut Inner,
    backend: &CdpBrowserBackend,
    keys: &str,
) -> Result<(), BrowserError> {
    let parsed = parse_key_combo(keys)?;
    let (modifiers, main) = parsed;
    let mut params = json!({
        "key": main.key,
        "code": main.code,
        "windowsVirtualKeyCode": main.virtual_key,
        "modifiers": modifiers,
    });
    if let Some(text) = main.text {
        params["text"] = json!(text);
    }
    let mut down = params.clone();
    down["type"] = json!("keyDown");
    backend
        .target_command(inner, "Input.dispatchKeyEvent", down)
        .await?;
    let mut up = params;
    up["type"] = json!("keyUp");
    backend
        .target_command(inner, "Input.dispatchKeyEvent", up)
        .await?;
    Ok(())
}

/// 主键描述(key / code / windowsVirtualKeyCode / 可打印 text)。
#[cfg(feature = "real")]
struct KeyDef {
    key: String,
    code: String,
    virtual_key: u32,
    text: Option<String>,
}

/// 解析 `"ctrl+shift+enter"` 形态的组合键 → (修饰键 bitmask, 主键)。
#[cfg(feature = "real")]
fn parse_key_combo(keys: &str) -> Result<(u32, KeyDef), BrowserError> {
    let mut modifiers = 0u32;
    let mut main: Option<String> = None;
    for part in keys.split('+') {
        let name = part.trim().to_ascii_lowercase();
        if name.is_empty() {
            return Err(BrowserError::other(format!("组合键格式无效: {keys:?}")));
        }
        match name.as_str() {
            "ctrl" | "control" => modifiers |= 2,
            "alt" | "option" => modifiers |= 1,
            "shift" => modifiers |= 8,
            "cmd" | "command" | "meta" | "super" => modifiers |= 4,
            _ => {
                if main.is_some() {
                    return Err(BrowserError::other(format!(
                        "组合键含多个主键: {keys:?}(修饰键+单主键)"
                    )));
                }
                main = Some(name);
            }
        }
    }
    let main = main.ok_or_else(|| BrowserError::other(format!("组合键缺主键: {keys:?}")))?;
    Ok((modifiers, map_key(&main)))
}

/// 键名 → CDP KeyDef(常用集;未知名按单字符处理)。
#[cfg(feature = "real")]
fn map_key(name: &str) -> KeyDef {
    match name {
        "enter" | "return" => KeyDef {
            key: "Enter".into(),
            code: "Enter".into(),
            virtual_key: 13,
            text: Some("\r".into()),
        },
        "tab" => KeyDef {
            key: "Tab".into(),
            code: "Tab".into(),
            virtual_key: 9,
            text: Some("\t".into()),
        },
        "esc" | "escape" => KeyDef {
            key: "Escape".into(),
            code: "Escape".into(),
            virtual_key: 27,
            text: None,
        },
        "backspace" => KeyDef {
            key: "Backspace".into(),
            code: "Backspace".into(),
            virtual_key: 8,
            text: None,
        },
        "delete" | "del" => KeyDef {
            key: "Delete".into(),
            code: "Delete".into(),
            virtual_key: 46,
            text: None,
        },
        "space" => KeyDef {
            key: " ".into(),
            code: "Space".into(),
            virtual_key: 32,
            text: Some(" ".into()),
        },
        "left" => KeyDef {
            key: "ArrowLeft".into(),
            code: "ArrowLeft".into(),
            virtual_key: 37,
            text: None,
        },
        "up" => KeyDef {
            key: "ArrowUp".into(),
            code: "ArrowUp".into(),
            virtual_key: 38,
            text: None,
        },
        "right" => KeyDef {
            key: "ArrowRight".into(),
            code: "ArrowRight".into(),
            virtual_key: 39,
            text: None,
        },
        "down" => KeyDef {
            key: "ArrowDown".into(),
            code: "ArrowDown".into(),
            virtual_key: 40,
            text: None,
        },
        "home" => KeyDef {
            key: "Home".into(),
            code: "Home".into(),
            virtual_key: 36,
            text: None,
        },
        "end" => KeyDef {
            key: "End".into(),
            code: "End".into(),
            virtual_key: 35,
            text: None,
        },
        "pageup" => KeyDef {
            key: "PageUp".into(),
            code: "PageUp".into(),
            virtual_key: 33,
            text: None,
        },
        "pagedown" => KeyDef {
            key: "PageDown".into(),
            code: "PageDown".into(),
            virtual_key: 34,
            text: None,
        },
        single if single.chars().count() == 1 => {
            let c = single.chars().next().unwrap();
            KeyDef {
                key: single.to_string(),
                code: String::new(),
                virtual_key: c.to_ascii_uppercase() as u32,
                text: Some(single.to_string()),
            }
        }
        other => KeyDef {
            key: other.to_string(),
            code: String::new(),
            virtual_key: 0,
            text: None,
        },
    }
}

/// CDP AX 节点 → ElementRole 子集映射。
#[cfg(feature = "real")]
fn map_role(role: &str) -> zhuye_computer_use::ElementRole {
    use zhuye_computer_use::ElementRole;
    match role {
        "button" => ElementRole::Button,
        "link" => ElementRole::Link,
        "textbox" | "searchbox" | "spinbutton" => ElementRole::TextField,
        "checkbox" => ElementRole::Checkbox,
        "radio" => ElementRole::Radio,
        "combobox" | "listbox" | "menu_list" => ElementRole::Combobox,
        "menuitem" | "menuitemcheckbox" | "menuitemradio" => ElementRole::MenuItem,
        "tab" => ElementRole::Tab,
        "heading" => ElementRole::Heading,
        "text" | "statictext" | "label" | "paragraph" | "listitem" | "cell" => ElementRole::Text,
        "image" | "img" => ElementRole::Image,
        "dialog" | "alertdialog" => ElementRole::Dialog,
        _ => ElementRole::Other,
    }
}

/// AX 树 → A11ySnapshot(忽略 ignored 节点与无名结构节点;深度按
/// parentId 链(nodes 数组为先序,父深度先于子已知);截断到上限)。
#[cfg(feature = "real")]
fn map_ax_tree(
    tree: &Value,
    title: String,
    version: u64,
) -> Result<zhuye_computer_use::A11ySnapshot, BrowserError> {
    use zhuye_computer_use::{A11yElement, A11ySnapshot};
    let nodes = tree
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| BrowserError::other("getFullAXTree 响应缺 nodes"))?;

    let mut elements = Vec::new();
    let mut depth_of: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for node in nodes {
        if node
            .get("ignored")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let Some(id) = node.get("nodeId").and_then(Value::as_str) else {
            continue;
        };
        // 父深度 + 1;根节点(parentId 缺失或未知)深度 0。
        let depth = node
            .get("parentId")
            .and_then(Value::as_str)
            .and_then(|pid| depth_of.get(pid).copied())
            .map_or(0, |d| d + 1);
        depth_of.insert(id.to_string(), depth);

        let role = map_role(
            node.pointer("/role/value")
                .and_then(Value::as_str)
                .unwrap_or("generic"),
        );
        let name = node
            .pointer("/name/value")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // 截断:只收录可交互元素 + 有名字的元素(纯结构节点无模型价值)。
        if elements.len() >= SNAPSHOT_ELEMENT_CAP {
            break;
        }
        if !role.is_interactive() && name.is_empty() {
            continue;
        }
        elements.push(A11yElement {
            reference: format!("@e{}", elements.len() + 1),
            role,
            name,
            disabled: node
                .get("disabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            visible: true,
            value: node
                .get("value")
                .and_then(|v| v.get("value"))
                .and_then(Value::as_str)
                .map(String::from),
            depth,
        });
    }
    Ok(A11ySnapshot::new(version, title, elements))
}

/// ref → 元素定位(nodeId + bounds 中心),与 map_ax_tree 同序同编号。
#[cfg(feature = "real")]
fn snapshot_element_index(
    tree: &Value,
) -> Result<std::collections::HashMap<String, ElementInfo>, BrowserError> {
    let mut index = std::collections::HashMap::new();
    let nodes = tree
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| BrowserError::other("getFullAXTree 响应缺 nodes"))?;
    let mut seq = 0usize;
    for node in nodes {
        if node
            .get("ignored")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let interactive = map_role(
            node.pointer("/role/value")
                .and_then(Value::as_str)
                .unwrap_or("generic"),
        )
        .is_interactive();
        let name = node
            .pointer("/name/value")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !interactive && name.is_empty() {
            continue;
        }
        if seq >= SNAPSHOT_ELEMENT_CAP {
            break;
        }
        seq += 1;
        let bounds = node.get("bounds");
        let center = match bounds {
            Some(b) => {
                let x = b.get("x").and_then(Value::as_f64).unwrap_or(0.0);
                let y = b.get("y").and_then(Value::as_f64).unwrap_or(0.0);
                let w = b.get("width").and_then(Value::as_f64).unwrap_or(0.0);
                let h = b.get("height").and_then(Value::as_f64).unwrap_or(0.0);
                (x + w / 2.0, y + h / 2.0)
            }
            None => (0.0, 0.0),
        };
        index.insert(
            format!("@e{seq}"),
            ElementInfo {
                node_id: node
                    .get("nodeId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                center,
            },
        );
    }
    Ok(index)
}

#[cfg(all(test, feature = "real"))]
mod real_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_key_combo_maps_modifiers_and_main_key() {
        let (modifiers, main) = parse_key_combo("ctrl+shift+enter").unwrap();
        assert_eq!(modifiers, 2 | 8);
        assert_eq!(main.key, "Enter");
        assert_eq!(main.virtual_key, 13);
        let (modifiers, main) = parse_key_combo("enter").unwrap();
        assert_eq!(modifiers, 0);
        assert_eq!(main.key, "Enter");
    }

    #[test]
    fn parse_key_combo_rejects_multi_main_and_missing_main() {
        assert!(parse_key_combo("ctrl+a+b").is_err());
        assert!(parse_key_combo("ctrl+").is_err());
        assert!(parse_key_combo("").is_err());
    }

    #[test]
    fn map_role_covers_common_ax_roles() {
        use zhuye_computer_use::ElementRole;
        assert_eq!(map_role("button"), ElementRole::Button);
        assert_eq!(map_role("textbox"), ElementRole::TextField);
        assert_eq!(map_role("generic"), ElementRole::Other);
        assert!(map_role("link").is_interactive());
    }

    #[test]
    fn map_ax_tree_skips_ignored_and_unnamed_structural_nodes() {
        let tree = json!({
            "nodes": [
                { "nodeId": "1", "ignored": false, "role": {"value": "root"},
                  "childIds": ["2", "3"], "parentId": "" },
                { "nodeId": "2", "ignored": false, "role": {"value": "button"},
                  "name": {"value": "提交"}, "bounds": {"x": 10, "y": 20, "width": 100, "height": 40} },
                { "nodeId": "3", "ignored": true, "role": {"value": "generic"} }
            ]
        });
        let snap = map_ax_tree(&tree, "测试页".into(), 3).unwrap();
        assert_eq!(snap.version, 3);
        // 只有按钮进入快照(root 无名且不可交互)。
        assert_eq!(snap.len(), 1);
        let button = snap.find("@e1").unwrap();
        assert_eq!(button.role, zhuye_computer_use::ElementRole::Button);
        assert_eq!(button.name, "提交");
        assert!(button.is_actionable());

        let index = snapshot_element_index(&tree).unwrap();
        let info = index.get("@e1").unwrap();
        assert_eq!(info.node_id, "2");
        assert!((info.center.0 - 60.0).abs() < f64::EPSILON);
        assert!((info.center.1 - 40.0).abs() < f64::EPSILON);
    }

    #[test]
    fn snapshot_index_sequence_matches_snapshot_refs() {
        // 两个命名元素 → @e1/@e2 编号与快照一致(点击才点得对)。
        let tree = json!({
            "nodes": [
                { "nodeId": "1", "ignored": false, "role": {"value": "textbox"},
                  "name": {"value": "搜索"}, "bounds": {"x": 0, "y": 0, "width": 10, "height": 10} },
                { "nodeId": "2", "ignored": false, "role": {"value": "link"},
                  "name": {"value": "首页"} }
            ]
        });
        let snap = map_ax_tree(&tree, String::new(), 1).unwrap();
        let index = snapshot_element_index(&tree).unwrap();
        assert_eq!(snap.len(), 2);
        assert!(index.contains_key("@e1") && index.contains_key("@e2"));
        assert_eq!(snap.find("@e1").unwrap().name, "搜索");
        assert_eq!(snap.find("@e2").unwrap().name, "首页");
    }

    #[test]
    fn check_binding_rejects_stale_refs() {
        let inner = Inner {
            conn: None,
            session_id: None,
            target_id: None,
            process: None,
            snapshot_version: 2,
            binding: Some(zhuye_computer_use::SnapshotBinding::new(2)),
            elements: std::collections::HashMap::new(),
        };
        assert!(check_binding(&inner, "@e1", 2).is_ok());
        let err = check_binding(&inner, "@e1", 1).unwrap_err();
        assert!(err.to_string().contains("stale"));
        let no_binding = Inner {
            binding: None,
            ..inner
        };
        let err = check_binding(&no_binding, "@e1", 2).unwrap_err();
        assert!(err.to_string().contains("browser_snapshot"));
    }
}
