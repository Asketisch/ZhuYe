//! 分支 / PR 探测测试(移植 Reflect-TUI 的 5 例,FakeRunner 脚本化
//! git / gh 响应链)。

use super::*;
use crate::runner::{CommandOutput, CommandRunner, WorkspaceCommand, WorkspaceCommandError};
use pretty_assertions::assert_eq;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

#[tokio::test]
async fn branch_diff_stats_prefers_remote_default_ref_over_stale_local_branch() {
    let runner = FakeRunner::new(vec![
        response(&["git", "rev-parse", "--git-dir"], 0, ".git\n"),
        response(&["git", "remote"], 0, "origin\n"),
        response(
            &["git", "symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
            0,
            "refs/remotes/origin/main\n",
        ),
        response(
            &[
                "git",
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/remotes/origin/main",
            ],
            0,
            "remote-main-sha\n",
        ),
        response(
            &["git", "merge-base", "HEAD", "refs/remotes/origin/main"],
            0,
            "base-sha\n",
        ),
        response(
            &["git", "diff", "--numstat", "base-sha..HEAD"],
            0,
            "1\t0\tfile\n",
        ),
    ]);

    let stats = branch_diff_stats_to_default_branch(&runner, Path::new("/repo"))
        .await
        .expect("branch diff stats");

    assert_eq!(
        stats,
        GitBranchDiffStats {
            additions: 1,
            deletions: 0,
        }
    );
    assert!(runner.saw(&["git", "merge-base", "HEAD", "refs/remotes/origin/main"]));
}

#[tokio::test]
async fn open_pull_request_uses_current_branch_view_first() {
    let runner = FakeRunner::new(vec![response(
        &["gh", "pr", "view", "--json", "number,url,state"],
        0,
        r#"{"number":20252,"url":"https://github.com/o/r/pull/20252","state":"OPEN"}"#,
    )]);

    let pull_request = open_pull_request(&runner, Path::new("/repo"))
        .await
        .expect("pull request");

    assert_eq!(
        pull_request,
        StatusLinePullRequest {
            number: 20_252,
            url: "https://github.com/o/r/pull/20252".to_string(),
        }
    );
    assert!(!runner.saw(&["git", "rev-parse", "HEAD"]));
}

#[tokio::test]
async fn open_pull_request_falls_back_to_parent_repo_commit_lookup() {
    let runner = FakeRunner::new(vec![
        response(&["gh", "pr", "view", "--json", "number,url,state"], 1, ""),
        response(&["git", "rev-parse", "HEAD"], 0, "head-sha\n"),
        response(
            &["gh", "repo", "view", "--json", "nameWithOwner,parent"],
            0,
            r#"{"nameWithOwner":"o/cli","parent":{"nameWithOwner":"o/r"}}"#,
        ),
        response(
            &[
                "gh",
                "api",
                "-H",
                "Accept: application/vnd.github+json",
                "repos/o/r/commits/head-sha/pulls",
            ],
            0,
            r#"[{"number":20252,"html_url":"https://github.com/o/r/pull/20252","state":"open"}]"#,
        ),
    ]);

    let pull_request = open_pull_request(&runner, Path::new("/repo"))
        .await
        .expect("pull request");

    assert_eq!(
        pull_request,
        StatusLinePullRequest {
            number: 20_252,
            url: "https://github.com/o/r/pull/20252".to_string(),
        }
    );
    assert!(runner.saw(&[
        "gh",
        "api",
        "-H",
        "Accept: application/vnd.github+json",
        "repos/o/r/commits/head-sha/pulls",
    ]));
}

#[test]
fn status_line_pr_view_parser_requires_open_pr() {
    assert_eq!(
        pull_request_from_view_output(r#"{"number":20252,"url":"https://x/20252","state":"OPEN"}"#),
        Some(StatusLinePullRequest {
            number: 20_252,
            url: "https://x/20252".to_string(),
        })
    );
    assert_eq!(
        pull_request_from_view_output(
            r#"{"number":20252,"url":"https://x/20252","state":"MERGED"}"#
        ),
        None
    );
}

#[test]
fn status_line_pr_fallback_searches_parent_repo_first() {
    assert_eq!(
        repo_search_order_from_output(
            r#"{"nameWithOwner":"o/cli","parent":{"nameWithOwner":"o/r"}}"#
        ),
        Some(vec!["o/r".to_string(), "o/cli".to_string()])
    );
}

fn response(argv: &[&str], exit_code: i32, stdout: &str) -> FakeResponse {
    FakeResponse {
        argv: argv.iter().map(|arg| (*arg).to_string()).collect(),
        output: CommandOutput {
            exit_code,
            stdout: stdout.to_string(),
            stderr: String::new(),
            truncated: false,
            timed_out: false,
        },
    }
}

struct FakeResponse {
    argv: Vec<String>,
    output: CommandOutput,
}

struct FakeRunner {
    responses: Mutex<VecDeque<FakeResponse>>,
    seen: Mutex<Vec<Vec<String>>>,
}

impl FakeRunner {
    fn new(responses: Vec<FakeResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn saw(&self, argv: &[&str]) -> bool {
        let argv: Vec<String> = argv.iter().map(|arg| (*arg).to_string()).collect();
        self.seen
            .lock()
            .expect("seen lock")
            .iter()
            .any(|seen| seen == &argv)
    }
}

impl CommandRunner for FakeRunner {
    fn run<'a>(
        &'a self,
        command: &'a WorkspaceCommand,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, WorkspaceCommandError>> + Send + 'a>>
    {
        self.seen
            .lock()
            .expect("seen lock")
            .push(command.argv.clone());
        Box::pin(async move {
            let mut responses = self.responses.lock().expect("responses lock");
            let index = responses
                .iter()
                .position(|response| response.argv == command.argv)
                .unwrap_or_else(|| panic!("missing fake response for {:?}", command.argv));
            let response = responses.remove(index).expect("fake response");
            Ok(response.output)
        })
    }
}
