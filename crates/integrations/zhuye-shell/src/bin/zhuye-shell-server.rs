//! `zhuye-shell-server` — 常驻 shell 会话的 MCP stdio server(WP16)。
//!
//! 工具:`shell_run`(启动,返回 session_id)/ `shell_read`(增量输出 +
//! 退出码)/ `shell_write`(stdin)/ `shell_kill` / `shell_list`。
//!
//! 与内置 `bash` 工具的分工:bash 一次性;本 server 管理长驻交互会话
//! (REPL / dev server / watch)。PTY 不做(见 zhuye-shell 模块 doc)。
//! 进程环境 env_clear(不继承宿主凭据),cwd 取参数(默认临时目录)。

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use zhuye_shell::ShellSessions;

const PROTOCOL_VERSION: &str = "2025-06-18";

fn tools_json() -> Value {
    json!({
        "tools": [
            {
                "name": "shell_run",
                "description": "启动一个常驻 shell 会话(后台进程),立即返回 session_id。输出用 shell_read 增量取,stdin 用 shell_write,停止用 shell_kill。适合 REPL / dev server / watch 等长任务;一次性命令请用内置 bash。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "完整命令行(经 sh -c 执行)"},
                        "cwd": {"type": "string", "description": "工作目录(默认系统临时目录)"}
                    },
                    "required": ["command"]
                }
            },
            {
                "name": "shell_read",
                "description": "读取会话的自上次调用以来的新增输出(stdout / stderr 分流)与退出状态。",
                "inputSchema": {
                    "type": "object",
                    "properties": {"session_id": {"type": "string"}},
                    "required": ["session_id"]
                }
            },
            {
                "name": "shell_write",
                "description": "向会话的 stdin 写入文本(自行带 \\n;写 EOF 用 shell_close_stdin)。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": {"type": "string"},
                        "input": {"type": "string"}
                    },
                    "required": ["session_id", "input"]
                }
            },
            {
                "name": "shell_close_stdin",
                "description": "关闭会话 stdin(让读 stdin 的程序收到 EOF 自然退出)。",
                "inputSchema": {
                    "type": "object",
                    "properties": {"session_id": {"type": "string"}},
                    "required": ["session_id"]
                }
            },
            {
                "name": "shell_kill",
                "description": "终止会话(SIGKILL)。",
                "inputSchema": {
                    "type": "object",
                    "properties": {"session_id": {"type": "string"}},
                    "required": ["session_id"]
                }
            },
            {
                "name": "shell_list",
                "description": "列出全部活跃会话 id。",
                "inputSchema": {"type": "object", "properties": {}}
            }
        ]
    })
}

fn main() {
    let pool = Arc::new(ShellSessions::new());
    let default_cwd = std::env::temp_dir();
    // 全局 runtime:**不能**每请求新建 —— ShellSession::spawn 在请求的
    // runtime 里 tokio::spawn pump 任务,请求 runtime drop 会 abort pump,
    // 后续 read 永远拿不到输出(实测:run 后 read 恒空)。所有请求共用
    // 同一个 runtime,pump 与退出监视随 server 进程存活。
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime"),
    );
    eprintln!("[shell-server] ready");
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
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(handle_request(method, &params, &pool, &default_cwd))
        }))
        .unwrap_or_else(|p| {
            json!({
                "content": [{"type": "text", "text": format!("server panic: {p:?}")}],
                "isError": true,
            })
        });

        let resp = json!({"jsonrpc": "2.0", "id": id, "result": result});
        writeln!(stdout, "{resp}").ok();
        stdout.flush().ok();
    }
}

async fn handle_request(
    method: &str,
    params: &Value,
    pool: &ShellSessions,
    default_cwd: &std::path::Path,
) -> Value {
    match method {
        "initialize" => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "serverInfo": {
                "name": "zhuye-shell",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {"tools": {"listChanged": false}},
        }),
        "tools/list" => tools_json(),
        "tools/call" => dispatch_tool(params, pool, default_cwd).await,
        "ping" => json!({}),
        other => json!({
            "content": [{"type": "text", "text": format!("method not implemented: {other}")}],
            "isError": true,
        }),
    }
}

async fn dispatch_tool(
    params: &Value,
    pool: &ShellSessions,
    default_cwd: &std::path::Path,
) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let result: Result<String, String> = async {
        match name {
            "shell_run" => {
                let command = str_arg(&args, "command")?;
                let cwd = args
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map_or_else(|| default_cwd.to_path_buf(), PathBuf::from);
                let session = pool
                    .run(vec!["sh".into(), "-c".into(), command], &cwd, true)
                    .map_err(|e| format!("spawn failed: {e}"))?;
                Ok(format!(
                    "session {} started: {} (cwd {})",
                    session.id,
                    session.command,
                    cwd.display()
                ))
            }
            "shell_read" => {
                let id = str_arg(&args, "session_id")?;
                let session = pool.get(&id).ok_or(format!("unknown session: {id}"))?;
                let (delta, _exited) = session.read_delta();
                let mut out = String::new();
                if !delta.stdout.is_empty() {
                    out.push_str(&format!("--- stdout ---\n{}", delta.stdout));
                }
                if !delta.stderr.is_empty() {
                    out.push_str(&format!("--- stderr ---\n{}", delta.stderr));
                }
                match delta.exit_code {
                    Some(code) => out.push_str(&format!("--- exited: {code} ---\n")),
                    None => out.push_str("--- still running ---\n"),
                }
                Ok(out)
            }
            "shell_write" => {
                let id = str_arg(&args, "session_id")?;
                let input = str_arg(&args, "input")?;
                let session = pool.get(&id).ok_or(format!("unknown session: {id}"))?;
                let ok = session
                    .write_stdin(&input)
                    .await
                    .map_err(|e| format!("write failed: {e}"))?;
                if ok {
                    Ok("written".into())
                } else {
                    Err("session stdin already closed (process exited?)".into())
                }
            }
            "shell_close_stdin" => {
                let id = str_arg(&args, "session_id")?;
                let session = pool.get(&id).ok_or(format!("unknown session: {id}"))?;
                session.close_stdin().await;
                Ok("stdin closed (EOF sent)".into())
            }
            "shell_kill" => {
                let id = str_arg(&args, "session_id")?;
                if pool.remove(&id).await {
                    Ok("killed".into())
                } else {
                    Err(format!("unknown session: {id}"))
                }
            }
            "shell_list" => {
                let ids = pool.list();
                if ids.is_empty() {
                    Ok("(no active sessions)".into())
                } else {
                    Ok(ids.join("\n"))
                }
            }
            other => Err(format!("unknown tool: {other}")),
        }
    }
    .await;
    match result {
        Ok(text) => json!({
            "content": [{"type": "text", "text": text}],
            "isError": false,
        }),
        Err(err) => json!({
            "content": [{"type": "text", "text": err}],
            "isError": true,
        }),
    }
}

fn str_arg(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| format!("缺少 {key} 参数"))
}
