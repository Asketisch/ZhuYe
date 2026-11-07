//! `zhuye-git-tools` — git 工作流 MCP server 的协议层(WP17)。
//!
//! 派发逻辑放在 lib(可单测);`bin/zhuye-git-tools-server.rs` 只做
//! stdio I/O,`bin/zhuye-cp-hook.rs` 是 Stop hook 入口(永远退出 0,
//! 不变量由集成测试锁定)。

use serde_json::{Value, json};
use std::path::PathBuf;
use zhuye_git::runner::CommandRunner;

const PROTOCOL_VERSION: &str = "2025-06-18";

pub fn tools_json() -> Value {
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

/// MCP 请求派发(initialize / tools/list / tools/call / ping)。
pub async fn handle_request(method: &str, params: &Value, runner: &dyn CommandRunner) -> Value {
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

/// `tools/call` 派发:summary / checkpoint。错误统一 isError 文案。
pub async fn dispatch_tool(params: &Value, runner: &dyn CommandRunner) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let result: Result<String, String> = async {
        let cwd = args.get("cwd").and_then(Value::as_str).map_or_else(
            || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            PathBuf::from,
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn local_runner() -> std::sync::Arc<dyn CommandRunner> {
        zhuye_git::runner::local_runner()
    }

    #[test]
    fn initialize_shape() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let r = rt.block_on(handle_request(
            "initialize",
            &Value::Null,
            local_runner().as_ref(),
        ));
        assert_eq!(r["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(r["serverInfo"]["name"], "zhuye-git-tools");
    }

    #[test]
    fn tools_list_has_both_tools() {
        let tools_value = tools_json();
        let tools = tools_value["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["git_branch_summary", "git_checkpoint_create"]);
    }

    #[tokio::test]
    async fn unknown_tool_is_error() {
        let params = json!({"name": "no_such", "arguments": {}});
        let r = handle_request("tools/call", &params, local_runner().as_ref()).await;
        assert_eq!(r["isError"], true);
        assert!(
            r["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("unknown tool")
        );
    }

    #[tokio::test]
    async fn summary_on_non_repo_reports_error() {
        let dir = TempDir::new().unwrap();
        let params = json!({
            "name": "git_branch_summary",
            "arguments": {"cwd": dir.path().to_str().unwrap()},
        });
        let r = handle_request("tools/call", &params, local_runner().as_ref()).await;
        assert_eq!(r["isError"], true);
        assert!(
            r["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("不是 git 仓库")
        );
    }

    #[tokio::test]
    async fn checkpoint_on_clean_repo_says_no_changes() {
        let dir = TempDir::new().unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        if !init.status.success() {
            return; // git 不可用,跳过。
        }
        let params = json!({
            "name": "git_checkpoint_create",
            "arguments": {"cwd": dir.path().to_str().unwrap(), "label": "t"},
        });
        let r = handle_request("tools/call", &params, local_runner().as_ref()).await;
        assert_eq!(r["isError"], false);
        assert!(
            r["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("no-changes")
        );
    }

    #[tokio::test]
    async fn checkpoint_on_non_repo_is_error() {
        let dir = TempDir::new().unwrap();
        let params = json!({
            "name": "git_checkpoint_create",
            "arguments": {"cwd": dir.path().to_str().unwrap()},
        });
        let r = handle_request("tools/call", &params, local_runner().as_ref()).await;
        assert_eq!(r["isError"], true);
    }

    #[test]
    fn ping_is_empty_object() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let r = rt.block_on(handle_request(
            "ping",
            &Value::Null,
            local_runner().as_ref(),
        ));
        assert_eq!(r, json!({}));
    }
}
