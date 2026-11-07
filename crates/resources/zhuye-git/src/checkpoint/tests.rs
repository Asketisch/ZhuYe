//! checkpoint 测试(移植 Reflect-TUI `git_checkpoint.rs` 的 6 例;
//! git 不可用时跳过,CI 友好)。

use super::*;
use std::path::PathBuf;

/// 建一个临时 git 仓库。git 不可用时返回 `None`(测试跳过)。
fn temp_repo(tag: &str) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("zhuye-git-cp-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let ok = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if !ok(&["init", "-q"]) {
        return None;
    }
    // 最小 git 身份,避免 commit 因 user.email 缺失失败。
    ok(&["config", "user.email", "test@zhuye.local"]);
    ok(&["config", "user.name", "zhuye-test"]);
    Some(dir)
}

#[test]
fn create_returns_none_without_changes() {
    let Some(dir) = temp_repo("noop") else {
        return;
    };
    std::fs::write(dir.join("a.txt"), "a").unwrap();
    assert!(create(&dir, "").unwrap().is_some(), "首次有改动应创建");
    assert!(create(&dir, "").unwrap().is_none(), "无改动不应创建空提交");
}

#[test]
fn ensure_returns_a_rewindable_checkpoint_for_clean_user_commit() {
    let Some(dir) = temp_repo("ensure-clean") else {
        return;
    };
    std::fs::write(dir.join("a.txt"), "user").unwrap();
    run_git(&dir, &["add", "-A"]).unwrap();
    run_git(&dir, &["commit", "-m", "ordinary commit"]).unwrap();
    let original = current_sha(&dir).unwrap();
    let checkpoint = ensure(&dir, "safe").unwrap();
    assert_ne!(checkpoint.sha, original);
    assert_eq!(
        find_checkpoint(&dir, &checkpoint.sha, 50).unwrap(),
        checkpoint
    );
    assert_eq!(ensure(&dir, "ignored").unwrap().sha, checkpoint.sha);
    rewind(&dir, &checkpoint.sha).unwrap();
}

#[test]
fn list_only_reports_own_commits() {
    let Some(dir) = temp_repo("list") else {
        return;
    };
    std::fs::write(dir.join("a.txt"), "a").unwrap();
    create(&dir, "first").unwrap();
    // git 提交时间戳是秒级,同一秒内的多个 checkpoint 排序不稳定 ——
    // 隔 1.1s 让 committerdate 有区分度(只在此测试付出 1s 代价)。
    std::thread::sleep(std::time::Duration::from_millis(1100));
    // 用户自己的普通提交(非 checkpoint subject)必须被过滤。
    std::fs::write(dir.join("b.txt"), "b").unwrap();
    run_git(&dir, &["add", "-A"]).unwrap();
    run_git(&dir, &["commit", "-m", "user commit"]).unwrap();
    std::fs::write(dir.join("c.txt"), "c").unwrap();
    create(&dir, "second").unwrap();

    let all = list(&dir, 50).unwrap();
    assert_eq!(all.len(), 2, "只列自己创建的提交:{all:?}");
    assert_eq!(all[0].subject, "second", "最新在前");
    assert_eq!(all[1].subject, "first");
}

#[test]
fn rewind_on_clean_tree_has_no_safety_snapshot() {
    let Some(dir) = temp_repo("clean") else {
        return;
    };
    std::fs::write(dir.join("a.txt"), "v1").unwrap();
    let first = create(&dir, "v1").unwrap().unwrap();
    std::fs::write(dir.join("a.txt"), "v2").unwrap();
    create(&dir, "v2").unwrap();

    let (target, safety) = rewind(&dir, &first.sha).unwrap();
    assert_eq!(target.sha, first.sha);
    assert!(safety.is_none(), "干净树不需要安全快照");
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "v1");
}

#[test]
fn rewind_on_dirty_tree_snapshots_uncommitted_work() {
    let Some(dir) = temp_repo("dirty") else {
        return;
    };
    std::fs::write(dir.join("a.txt"), "v1").unwrap();
    let first = create(&dir, "v1").unwrap().unwrap();
    std::fs::write(dir.join("a.txt"), "v2").unwrap();
    create(&dir, "v2").unwrap();
    // 未提交的改动。
    std::fs::write(dir.join("a.txt"), "dirty").unwrap();

    let (_target, safety) = rewind(&dir, &first.sha).unwrap();
    let safety = safety.expect("脏树必须先存安全快照");
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "v1",
        "回退后工作区是目标状态"
    );
    // 反悔:再回退到安全快照能拿回未提交编辑 —— 这正是打 tag 的意义
    // (安全快照已不在分支历史上,git log 看不到)。
    let (back, _) = rewind(&dir, &safety.sha).unwrap();
    assert_eq!(back.sha, safety.sha);
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "dirty");
}

#[test]
fn rewind_on_clean_user_commit_preserves_that_commit() {
    let Some(dir) = temp_repo("clean-user") else {
        return;
    };
    std::fs::write(dir.join("a.txt"), "v1").unwrap();
    let first = create(&dir, "v1").unwrap().unwrap();
    std::fs::write(dir.join("a.txt"), "user commit").unwrap();
    run_git(&dir, &["add", "-A"]).unwrap();
    run_git(&dir, &["commit", "-m", "user commit"]).unwrap();
    let (_, safety) = rewind(&dir, &first.sha).unwrap();
    let safety = safety.expect("非 checkpoint 的干净提交也必须保留");
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "v1");
    rewind(&dir, &safety.sha).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "user commit"
    );
}

#[test]
fn rewind_from_subdirectory_snapshots_the_entire_repository() {
    let Some(dir) = temp_repo("subdirectory") else {
        return;
    };
    let sub = dir.join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(dir.join("root.txt"), "old root").unwrap();
    std::fs::write(sub.join("nested.txt"), "old nested").unwrap();
    let target = create(&dir, "initial").unwrap().unwrap();
    std::fs::write(dir.join("root.txt"), "new root").unwrap();
    std::fs::write(sub.join("nested.txt"), "new nested").unwrap();
    let (_, safety) = rewind(&sub, &target.sha).unwrap();
    let safety = safety.expect("必须保存整个仓库的修改");
    assert_eq!(
        std::fs::read_to_string(dir.join("root.txt")).unwrap(),
        "old root"
    );
    rewind(&sub, &safety.sha).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("root.txt")).unwrap(),
        "new root"
    );
    assert_eq!(
        std::fs::read_to_string(sub.join("nested.txt")).unwrap(),
        "new nested"
    );
}

#[test]
fn rewind_rejects_non_checkpoint_and_malformed_sha() {
    let Some(dir) = temp_repo("reject") else {
        return;
    };
    std::fs::write(dir.join("a.txt"), "v1").unwrap();
    create(&dir, "v1").unwrap().unwrap();
    // 用户手挑的普通提交。
    std::fs::write(dir.join("b.txt"), "b").unwrap();
    run_git(&dir, &["add", "-A"]).unwrap();
    run_git(&dir, &["commit", "-m", "user commit"]).unwrap();
    let user_sha = run_git(&dir, &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    assert!(matches!(
        rewind(&dir, &user_sha),
        Err(GitError::NotACheckpoint(_))
    ));
    assert!(matches!(
        rewind(&dir, "deadbeef"),
        Err(GitError::NotACheckpoint(_))
    ));
    assert!(matches!(rewind(&dir, ""), Err(GitError::NotACheckpoint(_))));
    assert!(list(&dir, 0).unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn rewind_does_not_reset_if_safety_tag_creation_fails() {
    use std::os::unix::fs::PermissionsExt;
    let Some(dir) = temp_repo("tag-failure") else {
        return;
    };
    std::fs::write(dir.join("a.txt"), "original").unwrap();
    let target = create(&dir, "original").unwrap().unwrap();
    let hook = dir.join(".git/hooks/reference-transaction");
    std::fs::write(&hook, "#!/bin/sh\nwhile read old new ref; do\n case \"$ref\" in refs/tags/zhuye-cp/*) exit 1;; esac\ndone\nexit 0\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.join("a.txt"), "unsaved changes").unwrap();
    let error = rewind(&dir, &target.sha).unwrap_err().to_string();
    assert!(error.contains("标签创建失败"), "{error}");
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "unsaved changes"
    );
    let head = current_sha(&dir).unwrap();
    assert_ne!(head, target.sha);
    assert!(error.contains(&head));
}

#[test]
fn not_a_repo_reports_clearly() {
    let Some(dir) = temp_repo("norepo") else {
        return;
    };
    // 把 .git 挪走,留下非仓库目录。
    std::fs::remove_dir_all(dir.join(".git")).unwrap();
    assert!(!is_repo(&dir));
    assert!(matches!(create(&dir, "x"), Err(GitError::NotARepo)));
    assert!(matches!(list(&dir, 10), Err(GitError::NotARepo)));
    assert!(matches!(rewind(&dir, "abc"), Err(GitError::NotARepo)));
}
