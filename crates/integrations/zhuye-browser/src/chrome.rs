//! 浏览器发现 / 启动 / attach:可执行文件探测(平台常见路径 + env 覆盖)、
//! 隔离 profile 启动(`--remote-debugging-port=0` → 从 `DevToolsActivePort`
//! 文件解析实际端口)、运行中实例 attach。
//!
//! profile 目录 = `$ZHUYE_HOME/browser-profile`(经 `zhuye-config::paths`,
//! 与桌面 browser_backend 同一发现契约:谁先启动谁持有,后到方 attach)。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};

use crate::error::BrowserError;

/// 可执行文件覆盖 env。
pub const BROWSER_EXECUTABLE_ENV: &str = "ZHUYE_BROWSER_EXECUTABLE";
/// DevToolsActivePort 等待上限(冷启动可能要数秒)。
const DEVTOOLS_WAIT: Duration = Duration::from_secs(15);
/// DevToolsActivePort 轮询间隔。
const DEVTOOLS_POLL: Duration = Duration::from_millis(150);

/// 发现本机浏览器可执行文件。
///
/// 顺序:env 覆盖 → 平台常见路径。找不到返回 None(上层给 Unavailable 文案)。
pub fn discover_executable() -> Option<PathBuf> {
    if let Ok(custom) = std::env::var(BROWSER_EXECUTABLE_ENV) {
        let path = PathBuf::from(custom);
        if path.exists() {
            return Some(path);
        }
        // env 指向的路径不存在 → 视为配置错误,直接放弃探测。
        return None;
    }
    for candidate in platform_candidates() {
        if candidate.exists() {
            return Some(candidate);
        }
    }
    // PATH 探测(Linux 常见 chromium 包名)。
    for name in [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "microsoft-edge",
    ] {
        if which_ok(name) {
            return Some(PathBuf::from(name));
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn platform_candidates() -> Vec<PathBuf> {
    [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

#[cfg(target_os = "windows")]
fn platform_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(pf) = std::env::var("ProgramFiles") {
        for rel in [
            r"Google\Chrome\Application\chrome.exe",
            r"Microsoft\Edge\Application\msedge.exe",
        ] {
            out.push(PathBuf::from(&pf).join(rel));
        }
    }
    if let Ok(pf) = std::env::var("ProgramFiles(x86)") {
        for rel in [
            r"Google\Chrome\Application\chrome.exe",
            r"Microsoft\Edge\Application\msedge.exe",
        ] {
            out.push(PathBuf::from(&pf).join(rel));
        }
    }
    if let Ok(lap) = std::env::var("LOCALAPPDATA") {
        out.push(PathBuf::from(&lap).join(r"Google\Chrome\Application\chrome.exe"));
    }
    out
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_candidates() -> Vec<PathBuf> {
    [
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
        "/usr/bin/microsoft-edge",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

fn which_ok(name: &str) -> bool {
    std::env::var("PATH").is_ok_and(|paths| {
        paths
            .split(std::path::MAIN_SEPARATOR_STR)
            .any(|dir| !dir.is_empty() && Path::new(dir).join(name).exists())
    })
}

/// 隔离 profile 目录(`$ZHUYE_HOME/browser-profile`)。
pub fn profile_dir() -> PathBuf {
    zhuye_config::paths::data_path_or_relative(&["browser-profile"])
}

/// 启动浏览器(headless 或有头)并等 `DevToolsActivePort` 就绪。
///
/// 返回子进程句柄(由调用方决定随进程退出还是常驻;headless 下插件
/// 进程退出即回收)。启动失败 / 超时给结构化错误。
pub fn launch(
    executable: &Path,
    data_dir: &Path,
    headless: bool,
) -> Result<BrowserProcess, BrowserError> {
    std::fs::create_dir_all(data_dir)
        .map_err(|e| BrowserError::other(format!("profile 目录创建失败: {e}")))?;
    let mut args: Vec<String> = vec![
        format!("--user-data-dir={}", data_dir.display()),
        "--remote-debugging-port=0".into(),
        // 仅监听回环(BrowserSecurity loopback_only 的前置条件;Chrome
        // 默认也只绑 127.0.0.1,显式声明防旧版本语义漂移)。
        "--remote-debugging-address=127.0.0.1".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        // 隔离环境不加载用户扩展/书签同步。
        "--disable-extensions".into(),
        "about:blank".into(),
    ];
    if headless {
        // 新 headless(Chrome 112+ 与有头行为一致,screencast/screenshot 可用)。
        args.insert(0, "--headless=new".into());
    }
    let child = std::process::Command::new(executable)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| BrowserError::other(format!("浏览器启动失败: {e}")))?;
    let mut process = BrowserProcess {
        child: Some(child),
        data_dir: data_dir.to_path_buf(),
    };
    process.wait_devtools_active_port()?;
    Ok(process)
}

/// 运行中的浏览器进程(持有子进程;Drop 时回收 headless 实例)。
pub struct BrowserProcess {
    child: Option<std::process::Child>,
    data_dir: PathBuf,
}

impl BrowserProcess {
    /// 轮询等 `DevToolsActivePort` 文件出现并解析。
    fn wait_devtools_active_port(&mut self) -> Result<(), BrowserError> {
        let deadline = std::time::Instant::now() + DEVTOOLS_WAIT;
        loop {
            if self.devtools_port().is_ok() {
                return Ok(());
            }
            // 子进程提前退出(路径/参数错)→ 立即报错,不等满超时。
            if let Some(child) = self.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(BrowserError::other(format!(
                        "浏览器启动即退出(status={status});可执行文件或 profile 可能损坏"
                    )));
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(BrowserError::other(
                    "等待 DevToolsActivePort 超时(15s):浏览器可能未完成启动",
                ));
            }
            std::thread::sleep(DEVTOOLS_POLL);
        }
    }

    /// 解析 `DevToolsActivePort`:首行端口,次行 browser ws 路径。
    pub fn devtools_port(&self) -> Result<(u16, String), BrowserError> {
        let file = self.data_dir.join("DevToolsActivePort");
        let content = std::fs::read_to_string(&file)
            .map_err(|e| BrowserError::other(format!("DevToolsActivePort 读取失败: {e}")))?;
        parse_devtools_active_port(&content)
            .ok_or_else(|| BrowserError::other("DevToolsActivePort 格式异常(应为端口+路径两行)"))
    }
}

impl Drop for BrowserProcess {
    fn drop(&mut self) {
        // headless 实例随插件进程退出;有头实例由 Chrome 自身生命周期管理
        // (kill 只针对我们拉起的子进程)。
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 解析 DevToolsActivePort 内容(纯函数,单测覆盖)。
pub fn parse_devtools_active_port(content: &str) -> Option<(u16, String)> {
    let mut lines = content.lines();
    let port = lines.next()?.trim().parse::<u16>().ok()?;
    let path = lines.next()?.trim();
    if path.is_empty() {
        return None;
    }
    Some((port, path.to_string()))
}

/// 组装浏览器级 ws 端点。
pub fn browser_ws_url(port: u16, path: &str) -> String {
    format!("ws://127.0.0.1:{port}{path}")
}

/// 现有实例的 page targets(供单 target 校验与 attach)。
///
/// 用一次性短连接查 targets:浏览器级 ws 上发 `Target.getTargets`。
pub async fn query_page_targets(ws_url: &str) -> Result<Vec<Value>, BrowserError> {
    let mut conn = crate::cdp::CdpConnection::connect(ws_url).await?;
    let result = conn.command("Target.getTargets", json!({})).await?;
    let targets = result
        .get("targetInfos")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(targets
        .into_iter()
        .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_devtools_active_port_basic() {
        let (port, path) = parse_devtools_active_port("9222\n/devtools/browser/abc-123\n").unwrap();
        assert_eq!(port, 9222);
        assert_eq!(path, "/devtools/browser/abc-123");
    }

    #[test]
    fn parse_devtools_active_port_rejects_garbage() {
        assert!(parse_devtools_active_port("").is_none());
        assert!(parse_devtools_active_port("not-a-port\n/x\n").is_none());
        assert!(parse_devtools_active_port("9222\n").is_none());
        assert!(parse_devtools_active_port("9222\n\n").is_none());
    }

    #[test]
    fn browser_ws_url_joins_port_and_path() {
        assert_eq!(
            browser_ws_url(9222, "/devtools/browser/x"),
            "ws://127.0.0.1:9222/devtools/browser/x"
        );
    }

    #[test]
    fn profile_dir_is_under_data_dir() {
        let dir = profile_dir();
        assert!(dir.ends_with("browser-profile"), "实际: {dir:?}");
    }
}
