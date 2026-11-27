//! `zhuye-browser-server` — 浏览器控制插件的 MCP stdio **shim**。
//!
//! 职责(三步,任一步失败转入降级态):
//! 1. 确保本机浏览器(attach 已运行实例 / 隔离 profile 启动 headless 新实例,
//!    见 `zhuye_browser::shim::ensure_browser`);
//! 2. 定位上游 agent-browser 二进制(env 覆盖 → 同目录捆绑 → PATH);
//! 3. spawn `agent-browser mcp`(attach 端点走 `AGENT_BROWSER_CDP` env)
//!    并做 **stdio 双向行代理** —— 上游日志一律 stderr,stdout 只走协议行。
//!
//! 降级态:shim 自己应答 MCP(initialize / tools/list / tools/call /
//! ping),所有工具调用返回 isError + 可操作修复提示 —— **不 panic**
//! (与 computer-use 同范式:宿主在上游缺失 / 未装浏览器时拿到
//! 结构化失败,而不是 server 拉不起来)。
//!
//! wire 形态:line-delimited JSON-RPC 2.0 over stdio(MCP stdio 惯例)。

use std::io::{self, BufRead, Write};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::thread;

use serde_json::{Value, json};
use zhuye_browser::shim::{self, DegradeReason};

fn main() {
    match boot() {
        BootMode::Proxy(pipes) => run_proxy(pipes),
        BootMode::Degraded(reason) => run_degraded(reason),
    }
}

/// 启动结果:真代理态(含全部管道与浏览器句柄)或降级态。
enum BootMode {
    Proxy(ProxyPipes),
    Degraded(DegradeReason),
}

/// 代理态持有的句柄:上游子进程 + 双向管道 + 浏览器进程。
struct ProxyPipes {
    child: Child,
    child_stdin: ChildStdin,
    child_stdout: ChildStdout,
    /// 我们拉起的 headless 浏览器(attach 场景为空壳句柄)。放在结构体里
    /// 一起活到 run_proxy 结束:修复原因 —— 若在 boot 里提前丢弃,Drop 会
    /// 立即杀掉刚启动的浏览器,上游 daemon attach 到的是死端口;若全部
    /// 忽略,headless 实例则变孤儿进程。生命周期必须覆盖整个代理会话。
    _browser: zhuye_browser::chrome::BrowserProcess,
}

/// 启动三步:确保浏览器 → 定位上游 → spawn。任一步失败进降级态。
fn boot() -> BootMode {
    let degraded = |reason| BootMode::Degraded(reason);
    let (cdp_url, browser) = match shim::ensure_browser() {
        Ok(pair) => pair,
        Err(e) => return degraded(DegradeReason::BrowserMissing(e.to_string())),
    };
    let upstream = match shim::locate_upstream_binary() {
        Some(path) => path,
        None => return degraded(DegradeReason::UpstreamMissing),
    };
    let mut child = match shim::upstream_command(&upstream, &cdp_url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // 上游 stderr 透传到本进程 stderr:诊断信息进宿主日志,不污染协议。
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("[browser-server] 上游 {} 启动失败: {e}", upstream.display());
            return degraded(DegradeReason::UpstreamMissing);
        }
    };
    let child_stdin = child.stdin.take().expect("piped stdin 必然可取");
    let child_stdout = child.stdout.take().expect("piped stdout 必然可取");
    eprintln!(
        "[browser-server] 上游 {} 已启动,attach {cdp_url}",
        upstream.display()
    );
    BootMode::Proxy(ProxyPipes {
        child,
        child_stdin,
        child_stdout,
        _browser: browser,
    })
}

/// 真代理态:
/// - 下行线程:宿主 stdin → 上游 stdin(宿主请求下行;宿主断开即关上游
///   stdin,上游 MCP 循环读 EOF 后自行退出);
/// - 主线程:上游 stdout → 宿主 stdout(应答上行),上游退出(EOF)后
///   兜底 kill + wait,保证不留僵尸进程。
fn run_proxy(pipes: ProxyPipes) {
    // 解构拿走各字段所有权:stdin 归下行线程,stdout/child 归主线程,
    // `_browser` 留在本函数作用域,返回时 Drop(回收 headless 实例;
    // attach 场景为空壳句柄,不影响桌面端拉起的浏览器)。
    let ProxyPipes {
        mut child,
        child_stdin,
        child_stdout,
        _browser,
    } = pipes;
    // 下游线程持有上游 stdin:宿主 stdin 结束 → 管道随线程结束被 drop →
    // 上游读 EOF → 优雅退出(这是它的常规收尾路径,不靠 kill)。
    // 句柄不 join:主循环收尾后进程直接退出,线程随进程终止(见函数尾注)。
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut upstream_stdin = child_stdin;
        for line in stdin.lock().lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if writeln!(upstream_stdin, "{line}").is_err() {
                break;
            }
            upstream_stdin.flush().ok();
        }
    });
    let mut stdout = io::stdout();
    let reader = io::BufReader::new(child_stdout);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            // 上游 stdout 关闭 = 进程退出(常规收尾)。
            Err(_) => break,
        };
        if writeln!(stdout, "{line}").is_err() {
            // 宿主侧 stdout 断开:上游失去应答对象,直接收尾。
            break;
        }
        stdout.flush().ok();
    }
    // 兜底回收:上游可能还活着(stdout 未关,如宿主 stdout 先断)。
    let _ = child.kill();
    let _ = child.wait();
    // 修复原因:此处不可 join 下游线程 —— 它阻塞在宿主 stdin 上,宿主
    // 仍在线等待应答时 stdin 不会 EOF;若上游提前退出(如二进制损坏),
    // join 会永久死锁,宿主拿不到任何应答。直接返回让进程退出(Rust
    // main 返回即终止全部线程),宿主的 stdin 管道随进程关闭。
}

/// 降级态主循环:静态应答,工具调用统一 isError + 修复提示。
fn run_degraded(reason: DegradeReason) {
    eprintln!("[browser-server] 降级态:{}", reason.message());
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
            let err = json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {"code": -32700, "message": "parse error"},
            });
            writeln!(stdout, "{err}").ok();
            stdout.flush().ok();
            continue;
        };
        // 无 id 的是 notification(initialized 等),不应答。
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result = shim::degraded_response(method, &params, &reason);
        let resp = json!({"jsonrpc": "2.0", "id": id, "result": result});
        writeln!(stdout, "{resp}").ok();
        stdout.flush().ok();
    }
}
