//! `zhuye-git-tools-server` — git 工作流的 MCP stdio server(WP17,复用
//! `zhuye-git` 的能力族;harness-import 同范式:手写 JSON-RPC 2.0 over
//! stdio,stdout 只走协议,日志走 stderr)。
//!
//! 协议派发逻辑在 lib(可单测);本文件只做 stdio I/O。

use std::io::{self, BufRead, Write};

use serde_json::{Value, json};
use zhuye_git::runner::local_runner;
use zhuye_git_tools::handle_request;

fn main() {
    let runner = local_runner();
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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let result = runtime.block_on(handle_request(method, &params, runner.as_ref()));
        let resp = json!({"jsonrpc": "2.0", "id": id, "result": result});
        writeln!(stdout, "{resp}").ok();
        stdout.flush().ok();
    }
}
