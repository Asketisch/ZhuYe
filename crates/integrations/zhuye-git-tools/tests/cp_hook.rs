//! `zhuye-cp-hook` 的「永远退出 0」不变量集成测试(WP17 审计补)。
//!
//! 外部 hook fail-closed:非零退出 = `HookDecision::Deny`,而 `Stop`
//! hook 的 Deny 语义是**否决完成、强制续跑** —— checkpoint 失败若返回
//! 非零,agent 会被无限续跑直到 turn 上限。所以三条路径都必须 exit 0:
//! 正常快照 / 非 repo / 垃圾 stdin。

use std::process::{Command, Stdio};

fn hook_bin() -> &'static str {
    env!("CARGO_BIN_EXE_zhuye-cp-hook")
}

/// 简单驱动:喂 stdin,等退出,返回 (exit_code, stderr)。
fn run_hook_simple(input: &str, cwd: &std::path::Path) -> (i32, String) {
    use std::io::Write;
    let mut child = Command::new(hook_bin())
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn hook_exits_zero_on_clean_repo_with_no_changes() {
    let dir = tempfile::tempdir().unwrap();
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    if !init.status.success() {
        return; // git 不可用,跳过。
    }
    let (code, stderr) = run_hook_simple("{}", dir.path());
    assert_eq!(code, 0, "无改动也必须 exit 0,stderr={stderr}");
    assert!(
        stderr.contains("no changes") || stderr.contains("no snapshot"),
        "{stderr}"
    );
}

#[test]
fn hook_exits_zero_on_non_repo() {
    let dir = tempfile::tempdir().unwrap();
    let (code, stderr) = run_hook_simple("{}", dir.path());
    assert_eq!(code, 0, "非 repo 也必须 exit 0,stderr={stderr}");
    assert!(stderr.contains("checkpoint failed"), "{stderr}");
}

#[test]
fn hook_exits_zero_on_garbage_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let (code, _) = run_hook_simple("not json {{{", dir.path());
    assert_eq!(code, 0, "垃圾 stdin 也必须 exit 0");
}

#[test]
fn hook_creates_tagged_checkpoint_on_dirty_repo() {
    let dir = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    };
    if !git(&["init", "-q"]) {
        return;
    }
    git(&["config", "user.email", "t@zhuye.local"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(dir.path().join("a.txt"), "v1\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "init"]);
    // 工作区必须**脏**:干净树上 checkpoint 正确地返回 no-changes。
    std::fs::write(dir.path().join("a.txt"), "v2\n").unwrap();

    let (code, stderr) = run_hook_simple("{}", dir.path());
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("checkpoint created"), "{stderr}");
    // 快照打了 tag(reset --hard 后仍可达)。
    let tags = Command::new("git")
        .args(["tag", "--list"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let tags = String::from_utf8_lossy(&tags.stdout).into_owned();
    assert!(tags.contains("zhuye-cp/"), "tag 未打:{tags}");
}
