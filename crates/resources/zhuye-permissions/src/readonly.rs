//! # 命令准入:只读语义分类器(F04)
//!
//! `bash_classify` 管的是「危险 vs 安全」三档;**本模块管更细的问题**:
//! 这条命令是不是**纯读**?
//!
//! 区别在于审批噪音:
//!
//! - `git status` / `git config --get user.name` / `gh pr view 42` 是纯读,
//!   自动放行,不弹窗;
//! - `git config user.name x` / `git commit` / `gh pr create` 会写,
//!   该问就问。
//!
//! 现有实现把 `git` 整条命令按「安全」处理就漏掉了 `git config x y` 这类
//! 写入 —— 反过来把 `git` 全归「危险」则会为每次 `git status` 弹窗。
//! 本模块按 **子命令 + flag** 粒度判定。
//!
//! ## 设计要点
//!
//! 1. **保守**:判不准就返回 `Unknown`(当作"可能写",走审批),
//!    不因为"看起来像读"而放行。
//! 2. **不解析 shell**:输入是已经切好的 argv,由调用方负责解析
//!    (含引号 / 转义)。本模块不做 shell 词法,避免"解析差异 = 绕过"。
//! 3. **OPAQUE 优先**:`sh -c "..."` / `eval` / `xargs` 这类包装器里的
//!    内容是**不透明**的 —— 一律 `Opaque`(= 需审批),绝不因为外层看着
//!    像读就放行内层。

use serde::{Deserialize, Serialize};

/// 一条命令的读写语义判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessKind {
    /// 纯读(可自动放行)。
    ReadOnly,
    /// 会写(需审批)。
    Writes,
    /// 判不准(保守按"会写"处理)。
    Unknown,
    /// 包装器 / 动态执行,内容不可静态判定(必须审批)。
    Opaque,
}

impl AccessKind {
    /// 是否可自动放行(仅 ReadOnly)。
    pub fn is_auto_allow(self) -> bool {
        matches!(self, AccessKind::ReadOnly)
    }

    pub fn label(self) -> &'static str {
        match self {
            AccessKind::ReadOnly => "read_only",
            AccessKind::Writes => "writes",
            AccessKind::Unknown => "unknown",
            AccessKind::Opaque => "opaque",
        }
    }
}

/// 判定一条命令(已切好的 argv)。
///
/// `argv[0]` 是程序名,其余是参数。空 argv → `Unknown`。
pub fn classify_access(argv: &[String]) -> AccessKind {
    let Some(program) = argv.first().map(|s| basename(s)) else {
        return AccessKind::Unknown;
    };
    let args = &argv[1..];

    // 1) 包装器 / 动态执行:内容不可静态判定 → Opaque。
    if is_opaque_program(program) {
        return AccessKind::Opaque;
    }

    // 2) 已知只读程序(整条命令无写语义)。
    if is_always_readonly_program(program) {
        // 但 `find -delete` / `find -exec` 会写/执行 —— 例外。
        if program == "find"
            && has_any_flag(
                args,
                &[
                    "-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprint0",
                    "-fprintf", "-fls",
                ],
            )
        {
            return AccessKind::Writes;
        }
        // `sort -o file` 会写。
        if program == "sort"
            && args.iter().any(|arg| {
                arg == "-o"
                    || arg.starts_with("-o")
                    || arg == "--output"
                    || arg.starts_with("--output=")
            })
        {
            return AccessKind::Writes;
        }
        if program == "sort" && args.iter().any(|arg| arg.starts_with("--compress-program")) {
            return AccessKind::Opaque;
        }
        if program == "rg"
            && args
                .iter()
                .any(|arg| arg == "--pre" || arg.starts_with("--pre="))
        {
            return AccessKind::Opaque;
        }
        if program == "diff"
            && args
                .iter()
                .any(|arg| arg == "--output" || arg.starts_with("--output="))
        {
            return AccessKind::Writes;
        }
        return AccessKind::ReadOnly;
    }

    // 3) 已知会写的程序。
    if is_always_write_program(program) {
        return AccessKind::Writes;
    }

    // 4) 子命令粒度的程序。
    match program {
        "git" => classify_git(args),
        "gh" => classify_gh(args),
        "npm" | "pnpm" | "yarn" | "cargo" => classify_package_manager(program, args),
        _ => AccessKind::Unknown,
    }
}

/// 取程序名(去目录前缀;`/usr/bin/git` → `git`)。
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn has_any_flag(args: &[String], flags: &[&str]) -> bool {
    args.iter().any(|a| flags.contains(&a.as_str()))
}

fn is_opaque_program(program: &str) -> bool {
    matches!(
        program,
        "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "csh" | "tcsh"
    ) || matches!(
        program,
        "eval" | "exec" | "xargs" | "parallel" | "source" | "."
    )
}

/// 整条命令只读的程序(不含写子命令变体)。
fn is_always_readonly_program(program: &str) -> bool {
    matches!(
        program,
        // 查看类
        "ls" | "cat" | "head" | "tail" | "wc" | "stat"
        | "du" | "df" | "pwd" | "whoami" | "id" | "uname" | "hostname" | "date"
        | "uptime" | "ps" | "env" | "printenv" | "which" | "whereis" | "type"
        | "dirname" | "basename" | "realpath" | "readlink"
        // 搜索类
        | "grep" | "egrep" | "fgrep" | "rg" | "ag" | "ack" | "fd" | "locate"
        | "find" | "tree"
        // 文本处理(纯 stdout)
        | "sort" | "uniq" | "cut" | "tr" | "diff" | "cmp"
        | "comm" | "join" | "paste" | "column" | "nl" | "rev" | "fold" | "expand"
        // 编码 / 摘要
        | "base64" | "md5" | "md5sum" | "shasum" | "sha256sum" | "sha1sum"
        | "od" | "hexdump" | "jq" | "tomlq"
        // 环境探测
        | "true" | "false" | "test" | "[" | "echo" | "printf"
        // 解析查询
        | "dig" | "nslookup" | "host"
    )
}

fn is_always_write_program(program: &str) -> bool {
    matches!(
        program,
        "rm" | "rmdir"
            | "mv"
            | "cp"
            | "ln"
            | "chmod"
            | "chown"
            | "chgrp"
            | "touch"
            | "mkdir"
            | "tee"
            | "truncate"
            | "dd"
            | "shred"
            | "install"
            | "rsync"
    )
}

/// `git` 子命令粒度。
fn classify_git(args: &[String]) -> AccessKind {
    // Git 的配置覆盖、外部程序和输出路径可产生副作用，不能视为只读。
    if args.iter().any(|arg| {
        arg == "-c"
            || (arg.starts_with("-c") && arg.len() > 2)
            || arg.starts_with("--config-env")
            || arg.starts_with("--exec-path")
            || arg == "--output"
            || arg.starts_with("--output=")
    }) {
        return AccessKind::Opaque;
    }
    // 跳过全局选项(`-C <dir>` / `--no-pager` / `-c k=v`),找到子命令。
    let mut idx = 0;
    while idx < args.len() {
        let a = &args[idx];
        if a == "-C" || a == "-c" || a == "--git-dir" || a == "--work-tree" {
            idx += 2;
            continue;
        }
        if a.starts_with('-') {
            idx += 1;
            continue;
        }
        break;
    }
    let Some(sub) = args.get(idx).map(|s| s.as_str()) else {
        // 只有全局选项,没有子命令 —— `git --version` 之类。
        return AccessKind::ReadOnly;
    };
    let rest = &args[idx + 1..];

    match sub {
        // 纯读子命令
        "status" | "log" | "diff" | "show" | "blame" | "annotate" | "grep" | "describe"
        | "rev-parse" | "rev-list" | "ls-files" | "ls-tree" | "ls-remote" | "cat-file"
        | "for-each-ref" | "shortlog" | "whatchanged" | "version" | "help" | "count-objects"
        | "fsck" | "reflog" => AccessKind::ReadOnly,

        // `git config`:读 `--get` / `--list` / `-l`,写 `name value`。
        "config" => {
            if has_any_flag(
                rest,
                &["--get", "--get-all", "--get-regexp", "--list", "-l"],
            ) || rest.is_empty()
            {
                AccessKind::ReadOnly
            } else {
                AccessKind::Writes
            }
        }

        // `git branch`:带 `-a` / `-v` / `--list` 是读,其余(创建/删除/改名)是写。
        "branch" => {
            if has_any_flag(rest, &["-a", "-v", "-vv", "--list", "--show-current"])
                && !has_any_flag(rest, &["-d", "-D", "-m", "-M", "-c", "-C"])
            {
                AccessKind::ReadOnly
            } else {
                AccessKind::Writes
            }
        }

        // `git tag` / `git remote` 同构。
        "tag" => {
            if rest.is_empty() || has_any_flag(rest, &["-l", "--list"]) {
                AccessKind::ReadOnly
            } else {
                AccessKind::Writes
            }
        }
        "remote" => {
            if rest.is_empty() || has_any_flag(rest, &["-v", "--verbose"]) {
                AccessKind::ReadOnly
            } else {
                AccessKind::Writes
            }
        }

        // 明确写入类
        "commit" | "add" | "rm" | "mv" | "reset" | "checkout" | "switch" | "restore" | "merge"
        | "rebase" | "cherry-pick" | "revert" | "push" | "fetch" | "pull" | "clone" | "init"
        | "clean" | "stash" | "apply" | "am" | "gc" | "prune" | "filter-branch" | "update-ref"
        | "worktree" | "submodule" | "notes" | "archive" | "bundle" | "repack" => {
            AccessKind::Writes
        }

        _ => AccessKind::Unknown,
    }
}

/// `gh` 多词粒度:`gh pr view` 读,`gh pr create` 写。
fn classify_gh(args: &[String]) -> AccessKind {
    let Some(noun) = args.first().map(|s| s.as_str()) else {
        return AccessKind::Unknown;
    };
    let Some(verb) = args.get(1).map(|s| s.as_str()) else {
        // `gh pr` / `gh issue` 无动词 —— 保守。
        return AccessKind::Unknown;
    };

    // 读动词:view / list / status / diff / checks / checks 等。
    let read_verbs = [
        "view", "list", "status", "diff", "checks", "checks", "download",
    ];
    if read_verbs.contains(&verb) {
        return AccessKind::ReadOnly;
    }
    // 写动词:create / merge / close / reopen / edit / delete / comment / review ...
    let write_verbs = [
        "create", "merge", "close", "reopen", "edit", "delete", "comment", "review", "approve",
        "ready", "lock", "unlock", "transfer", "pin", "unpin", "rerun", "cancel", "add", "remove",
        "set", "submit", "publish", "upload", "release",
    ];
    if write_verbs.contains(&verb) {
        return AccessKind::Writes;
    }
    // `gh api`:`-X GET`(默认)是读,其它方法写。
    if noun == "api" {
        return if has_any_flag(args, &["-X", "--method"]) {
            // 检查方法名
            let upper: Vec<String> = args.iter().map(|a| a.to_ascii_uppercase()).collect();
            if upper.iter().any(|a| a == "GET") {
                AccessKind::ReadOnly
            } else {
                AccessKind::Writes
            }
        } else {
            AccessKind::ReadOnly
        };
    }
    AccessKind::Unknown
}

/// 包管理器:`npm ls` / `pnpm list` / `cargo metadata` 读;`install` / `add` 写。
fn classify_package_manager(program: &str, args: &[String]) -> AccessKind {
    let Some(sub) = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(|s| s.as_str())
    else {
        return AccessKind::Unknown;
    };
    let read_subs = [
        "ls", "list", "view", "info", "search", "outdated", "why", "metadata", "tree", "audit",
    ];
    if read_subs.contains(&sub) {
        return AccessKind::ReadOnly;
    }
    let write_subs = [
        "install",
        "i",
        "add",
        "remove",
        "rm",
        "uninstall",
        "update",
        "upgrade",
        "publish",
        "build",
        "run",
        "exec",
        "link",
        "init",
        "new",
        "clean",
        "prune",
        "test",
    ];
    if write_subs.contains(&sub) {
        // `cargo run`/`test`/`build` 会写 target 目录 —— 归写。
        return AccessKind::Writes;
    }
    let _ = program;
    AccessKind::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(|x| x.to_string()).collect()
    }

    fn kind(s: &str) -> AccessKind {
        classify_access(&argv(s))
    }

    #[test]
    fn simple_readonly_programs() {
        assert_eq!(kind("ls -la"), AccessKind::ReadOnly);
        assert_eq!(kind("cat README.md"), AccessKind::ReadOnly);
        assert_eq!(kind("grep -rn TODO src/"), AccessKind::ReadOnly);
        assert_eq!(kind("git --version"), AccessKind::ReadOnly);
    }

    #[test]
    fn simple_write_programs() {
        assert_eq!(kind("rm -rf target"), AccessKind::Writes);
        assert_eq!(kind("mv a b"), AccessKind::Writes);
        assert_eq!(kind("chmod 755 x"), AccessKind::Writes);
        assert_eq!(kind("touch new.txt"), AccessKind::Writes);
    }

    #[test]
    fn opaque_wrappers_always_need_approval() {
        assert_eq!(kind("sh -c ls"), AccessKind::Opaque);
        assert_eq!(kind("bash -c 'rm -rf /'"), AccessKind::Opaque);
        assert_eq!(kind("eval cross"), AccessKind::Opaque);
        assert_eq!(kind("xargs rm"), AccessKind::Opaque);
        assert_eq!(kind("parallel echo"), AccessKind::Opaque);
    }

    #[test]
    fn git_config_read_vs_write() {
        // 读
        assert_eq!(kind("git config --get user.name"), AccessKind::ReadOnly);
        assert_eq!(kind("git config --list"), AccessKind::ReadOnly);
        assert_eq!(kind("git config -l"), AccessKind::ReadOnly);
        assert_eq!(kind("git config --get-regexp user"), AccessKind::ReadOnly);
        // 写
        assert_eq!(kind("git config user.name Alice"), AccessKind::Writes);
        assert_eq!(kind("git config --unset user.name"), AccessKind::Writes);
    }

    #[test]
    fn git_read_subcommands() {
        for cmd in [
            "git status",
            "git log --oneline",
            "git diff HEAD",
            "git show HEAD",
            "git blame src/lib.rs",
            "git rev-parse HEAD",
            "git ls-files",
            "git reflog",
        ] {
            assert_eq!(kind(cmd), AccessKind::ReadOnly, "{cmd}");
        }
    }

    #[test]
    fn git_write_subcommands() {
        for cmd in [
            "git commit -m x",
            "git add .",
            "git push",
            "git reset --hard",
            "git checkout main",
            "git merge feat",
            "git clean -fd",
            "git stash",
        ] {
            assert_eq!(kind(cmd), AccessKind::Writes, "{cmd}");
        }
    }

    #[test]
    fn git_global_options_skipped_before_subcommand() {
        // `-C <dir>` 与 `-c k=v` 都要跳过,s e.g. `git -C repo status`。
        assert_eq!(kind("git -C /tmp/repo status"), AccessKind::ReadOnly);
        assert_eq!(kind("git --no-pager log"), AccessKind::ReadOnly);
        assert_eq!(kind("git -C /tmp/repo commit -m x"), AccessKind::Writes);
    }

    #[test]
    fn git_branch_read_vs_write() {
        assert_eq!(kind("git branch -a"), AccessKind::ReadOnly);
        assert_eq!(kind("git branch -v"), AccessKind::ReadOnly);
        assert_eq!(kind("git branch new-branch"), AccessKind::Writes);
        assert_eq!(kind("git branch -d old"), AccessKind::Writes);
    }

    #[test]
    fn gh_multiword_granularity() {
        // 读
        assert_eq!(kind("gh pr view 42"), AccessKind::ReadOnly);
        assert_eq!(kind("gh pr list"), AccessKind::ReadOnly);
        assert_eq!(kind("gh issue view 7"), AccessKind::ReadOnly);
        // 写
        assert_eq!(kind("gh pr create --title x"), AccessKind::Writes);
        assert_eq!(kind("gh pr merge 42"), AccessKind::Writes);
        assert_eq!(kind("gh issue close 7"), AccessKind::Writes);
    }

    #[test]
    fn gh_api_method_sensitive() {
        assert_eq!(kind("gh api repos/a/b"), AccessKind::ReadOnly, "默认 GET");
        assert_eq!(kind("gh api -X GET repos/a/b"), AccessKind::ReadOnly);
        assert_eq!(kind("gh api -X POST repos/a/b"), AccessKind::Writes);
    }

    #[test]
    fn find_write_flags_detected() {
        assert_eq!(kind("find . -name x"), AccessKind::ReadOnly);
        assert_eq!(kind("find . -delete"), AccessKind::Writes);
        assert_eq!(kind("find . -exec rm {} ;"), AccessKind::Writes);
    }

    #[test]
    fn dynamic_or_writing_flags_are_never_auto_allowed() {
        for cmd in [
            "sed -i s/a/b/ file",
            "awk script file",
            "curl -o file http://127.0.0.1",
            "wget -O file http://127.0.0.1",
            "sort -ofile input",
            "sort --output=file input",
            "sort --compress-program=sh input",
            "find . -fprint output",
            "rg --pre=sh needle",
            "git -c core.pager=sh status",
            "git log --output=file",
            "yq -i script file",
            "xxd -r input output",
        ] {
            assert!(!kind(cmd).is_auto_allow(), "{cmd}");
        }
    }

    #[test]
    fn package_manager_read_vs_write() {
        assert_eq!(kind("npm ls"), AccessKind::ReadOnly);
        assert_eq!(kind("pnpm list"), AccessKind::ReadOnly);
        assert_eq!(kind("cargo metadata"), AccessKind::ReadOnly);
        assert_eq!(kind("npm install"), AccessKind::Writes);
        assert_eq!(kind("cargo build"), AccessKind::Writes);
        assert_eq!(kind("cargo test"), AccessKind::Writes);
    }

    #[test]
    fn unknown_program_is_unknown() {
        assert_eq!(kind("some-custom-binary --flag"), AccessKind::Unknown);
        assert_eq!(classify_access(&[]), AccessKind::Unknown);
    }

    #[test]
    fn absolute_path_program_resolved() {
        assert_eq!(kind("/usr/bin/git status"), AccessKind::ReadOnly);
        assert_eq!(kind("/bin/rm x"), AccessKind::Writes);
    }
}
