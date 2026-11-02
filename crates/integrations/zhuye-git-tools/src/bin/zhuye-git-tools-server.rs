//! `zhuye-git-tools-server` — git 工作流的 MCP stdio server(WP17,复用
//! `zhuye-git` 的能力族;harness-import 同范式:手写 JSON-RPC 2.0 over
//! stdio,stdout 只走协议,日志走 stderr)。
//!
//! 工具:`git_branch_summary`(分支 + 与默认分支的 diff 统计 + 打开的
//! PR)/ `git_checkpoint_create`(显式打快照,tag 化,可 rewind)。
//! 探测链的细节(远程 HEAD 优先 / fork 父仓库优先)见 `zhuye-git::branch`。
//!
//! 另提供 `zhuye-cp-hook` 二进制:Stop hook 用(turn 结束自动 checkpoint)。
//! **永远退出 0** —— 插件 hook fail-closed(非零退出 = Deny),而 Stop
//! hook 的 Deny 语义是「否决完成、强制续跑」,checkpoint 失败若返回非零
//! 会造成无限续跑循环。hook 是纯信息性的,失败只该记录不该拦截。

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use serde_json::{Value, json};
use zhuye_git::runner::local_runner;

const PROTOCOL_VERSION: &str = "2025-06-18";

fn tools_json() -> Value {
    json!({
        "tools": [
            {
                "name": "git_branch_summary",
                "description": "探测 git 仓库状态:当前分支、与默认分支(origin 优先)的已提交 diff 统计(增/删行)、当前分支打开的 PR(gh CLI;fork 时父仓库优先)。全部尽力而为,缺失项不报错。未提交的工作树编辑不计入统计。",
                "inputSchema": {
                    "type": "object",
                    "properties": {"cwd": {"type": "string", "description": "仓库路径(默认当前目录)"}}
                }
            },
            {
                "name": "git_checkpoint_create",
                "description": "对工作区打 git checkpoint 快照(git add -A + commit + 打 zhuye-cp/ tag)。可用 zhuye 的 rewind 工具回退。无改动时返回 no-changes。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "cwd": {"type": "string", "description": "仓库路径(默认当前目录)"},
                        "label": {"type": "string", "description": "快照标签(人可读)"}
                    }
                }
            }
        ]
    })
}

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

async fn handle_request(
    method: &str,
    params: &Value,
    runner: &dyn zhuye_git::runner::CommandRunner,
) -> Value {
    match method {
        "initialize" => json!({
            "protocolVersion": PROTOCOL_VERSION,
            "serverInfo": {
                "name": "zhuye-git-tools",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {"tools": {"listChanged": false}},
        }),
        "tools/list" => tools_json(),
        "tools/call" => dispatch_tool(params, runner).await,
        "ping" => json!({}),
        other => json!({
            "content": [{"type": "text", "text": format!("method not implemented: {other}")}],
            "isError": true,
        }),
    }
}

async fn dispatch_tool(params: &Value, runner: &dyn zhuye_git::runner::CommandRunner) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let result: Result<String, String> = async {
        let cwd = args
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        match name {
            "git_branch_summary" => {
                if !zhuye_git::checkpoint::is_repo(&cwd) {
                    return Err(format!("{} 不是 git 仓库", cwd.display()));
                }
                let branch = zhuye_git::current_branch_name(runner, &cwd).await;
                let summary = zhuye_git::status_line_git_summary(runner, &cwd).await;
                let mut out = format!(
                    "branch: {}\n",
                    branch.as_deref().unwrap_or("(detached / unknown)")
                );
                if let Some(stats) = &summary.branch_change_stats {
                    out.push_str(&format!(
                        "diff vs default branch: +{} -{}\n",
                        stats.additions, stats.deletions
                    ));
                } else {
                    out.push_str("diff vs default branch: (unavailable)\n");
                }
                match &summary.pull_request {
                    Some(pr) => out.push_str(&format!("open PR: #{} {}\n", pr.number, pr.url)),
                    None => out.push_str("open PR: (none found)\n"),
                }
                Ok(out)
            }
            "git_checkpoint_create" => {
                let label = args.get("label").and_then(Value::as_str).unwrap_or("");
                match zhuye_git::checkpoint::create(&cwd, label) {
                    Ok(Some(cp)) => Ok(format!(
                        "checkpoint `{}` created @ {} ({})",
                        cp.subject, cp.sha, cp.committed_at
                    )),
                    Ok(None) => Ok("no-changes (工作区干净,未创建快照)".into()),
                    Err(e) => Err(e.to_string()),
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
