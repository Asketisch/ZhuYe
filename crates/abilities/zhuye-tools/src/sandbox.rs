//! `PathSandbox` —— 路径沙箱(对齐主流 harness 的 workspace-write 取舍)。
//!
//! - **读**:全盘可读(相对路径仍以 workspace 为基)。编译 / 分析常需读
//!   系统头与依赖,限制读会大面积破坏可用性;仅拒绝用户主目录下的
//!   凭证路径(SSH 密钥 / 钥匙串 / 云凭证等,与 OS 沙箱同口径)。
//! - **写 / 改 / 删**:允许根 = workspace ∪ 额外可写目录(`writable_dirs`)
//!   ∪ `/tmp` ∪ `$TMPDIR`;根内的 `.git` / `.agents` / `.aws` 任意层,
//!   以及 `.zhuye` 之下的提权面子集(config / sessions / cron 等,
//!   见 [`PROTECTED_ZHUYE_CHILDREN`] denylist)
//!   元数据目录保持只读(读不受影响)—— 改 hook / 配置是常见提权面。
//! - [`resolve_sandbox_path`](旧的工作区墙)保留给 LSP 等仍按工作区
//!   收敛的集成。
//!
//! 详见 `docs/tools-and-hooks.md §8.1`。

use std::path::{Component, Path, PathBuf};

use tracing::warn;

use crate::tool::ToolError;

mod safe_file;
mod safe_write;
pub use safe_file::{PinnedFile, open_readonly};
pub(crate) use safe_write::PinnedWrite;

/// 凭证路径后缀(相对各用户主目录)—— 读拒绝。
///
/// 与 `zhuye-sandbox` 的 `os_sandbox.rs::seatbelt_profile` 里 sensitive_reads
/// 同口径:OS 沙箱关闭(WorkspaceOnly)时这层是唯一防线,改动需两侧同步。
const SENSITIVE_READ_SUFFIXES: [&str; 12] = [
    ".ssh",
    ".aws",
    ".gnupg",
    ".kube",
    ".docker",
    ".config/gh",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".cargo/credentials",
    ".config/git/credentials",
    "Library/Keychains",
];

/// 写保护的工作区元数据分量:出现在路径任意一层即拒绝写 / 改 / 删。
/// `.git`(含 hooks)、`.agents`、`.aws` 都是「改了就能借宿主身份提权」
/// 的位置;`.zhuye` 单独走 [`PROTECTED_ZHUYE_CHILDREN`] 的 denylist。
/// 读仍允许,`git status` 不受影响。
const PROTECTED_WRITE_COMPONENTS: [&str; 3] = [".git", ".agents", ".aws"];

/// `.zhuye` 之下的写保护子项(denylist):会话历史 / 配置 / 权限规则 /
/// 计划任务等「改了就能借宿主身份提权或污染未来上下文」的位置。
/// 反例(放行):`spill`(v1.11 大输出落盘)、`plan`(PlanWrite 草稿)、
/// `worktrees`(side-channel 工作树)、`uploads`(附件暂存)等引擎托管
/// 的工作数据 —— 枚举式豁免已被 CI 证伪(A23 / plan / spill 三连误伤),
/// 保护面收敛到真正的提权清单。新引擎工作目录默认可写;新提权面进本表。
const PROTECTED_ZHUYE_CHILDREN: [&str; 10] = [
    "config.toml",
    "permissions.toml",
    "remote-hosts.json",
    "sessions",
    "cron",
    "prompt_history.jsonl",
    "workspaces.json",
    "quota-state.json",
    "session-notes",
    "autopilot",
];

/// 候选用户主目录:当前 `$HOME` + (macOS) `/Users` 下的真实用户目录。
/// 与 `seatbelt_profile` 的 homes 枚举同口径。
fn candidate_homes() -> Vec<PathBuf> {
    let mut homes = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        homes.push(home.canonicalize().unwrap_or(home));
    }
    #[cfg(target_os = "macos")]
    if let Ok(entries) = std::fs::read_dir("/Users") {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                homes.push(entry.path());
            }
        }
    }
    homes.sort();
    homes.dedup();
    homes
}

/// `abs` 落在某个用户主目录的凭证后缀下 → Some(该后缀)。
fn sensitive_read_match(abs: &Path) -> Option<String> {
    for home in candidate_homes() {
        if abs.starts_with(&home) {
            for suffix in SENSITIVE_READ_SUFFIXES {
                if abs.starts_with(home.join(suffix)) {
                    return Some(suffix.to_string());
                }
            }
        }
    }
    None
}

/// 读路径解析:全盘可读,相对路径以 `workspace` 为基。
///
/// 与旧 [`resolve_sandbox_path`] 的差别:不再要求路径落在 workspace 内
/// (对齐主流 harness 的「全盘只读」),但凭证路径拒绝。文件不存在仍是 `Io`
/// (与旧行为一致,调用方靠 open 报错)。
pub fn resolve_readable_path(workspace: &Path, path: &Path) -> Result<PathBuf, ToolError> {
    let abs_workspace = workspace
        .canonicalize()
        .map_err(|e| ToolError::Io(e.to_string()))?;
    let abs_path = if path.is_absolute() {
        path.canonicalize()
            .map_err(|e| ToolError::Io(e.to_string()))?
    } else {
        abs_workspace
            .join(path)
            .canonicalize()
            .map_err(|e| ToolError::Io(e.to_string()))?
    };
    if let Some(suffix) = sensitive_read_match(&abs_path) {
        warn!(path = %abs_path.display(), "read: 凭证路径拒绝");
        return Err(ToolError::PermissionDenied {
            reason: format!(
                "拒绝读取敏感凭证路径 ~/{suffix}(SSH 密钥 / 云凭证 / 钥匙串等,\
                 与 OS 沙箱同口径)"
            ),
        });
    }
    Ok(abs_path)
}

/// 把**目录类**路径参数(grep / glob 的 `path`)解析为可读搜索根。
///
/// 与 [`resolve_readable_path`] 的差别只在错误信息:这里的调用场景是
/// "搜索根目录",路径不存在 / 不是目录时应给出可操作的提示,而不是裸
/// Io 错误。默认根(workspace,不传 `path`)不走本函数。
pub fn resolve_readable_dir(workspace: &Path, path: &Path) -> Result<PathBuf, ToolError> {
    let resolved = resolve_readable_path(workspace, path).map_err(|e| match e {
        // 规范化失败几乎总是"不存在",给搜索场景一句人话。
        ToolError::Io(_) => ToolError::InvalidArgs {
            message: format!(
                "path 不存在或无法访问: {} (相对 workspace 或任意绝对路径)",
                path.display()
            ),
        },
        other => other,
    })?;
    if !resolved.is_dir() {
        return Err(ToolError::InvalidArgs {
            message: format!("path 不是目录: {}", path.display()),
        });
    }
    Ok(resolved)
}

/// 逐组件解析成绝对路径,允许末尾若干组件不存在(新建文件场景)。
///
/// 已存在的组件随时 canonicalize:符号链接一旦指向允许根之外,后续
/// `starts_with` 判定立即失败,防止「先建链接再写」逃逸。绝对路径的
/// `RootDir` 分量会重置累积路径,与相对路径同一套循环处理。
fn resolve_components(base: &Path, path: &Path) -> PathBuf {
    let mut final_path = base.to_path_buf();
    for component in path.components() {
        if component == Component::ParentDir {
            final_path.pop();
            continue;
        }
        final_path.push(component);
        if final_path.exists() {
            if let Ok(c) = final_path.canonicalize() {
                final_path = c;
            }
        }
        // 否则保持原样,调用方后续会创建它(或它的父目录)。
    }
    final_path
}

/// 写允许根(全部规范化):workspace ∪ 额外可写目录 ∪ `/tmp` ∪ `/var/tmp`
/// ∪ `$TMPDIR`。macOS 上 `/tmp` 是符号链接,规范化后为 `/private/tmp`,
/// 与经符号链接解析的写入路径同根比较。
fn writable_roots(workspace: &Path, extra_writable: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = vec![workspace.to_path_buf()];
    roots.extend(extra_writable.iter().cloned());
    roots.push(PathBuf::from("/tmp"));
    roots.push(PathBuf::from("/var/tmp"));
    roots.push(std::env::temp_dir());
    let mut roots: Vec<PathBuf> = roots
        .into_iter()
        .map(|r| r.canonicalize().unwrap_or(r))
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

/// 写 / 改 / 删路径解析。
///
/// 最终路径(含尚不存在的新建文件)必须落在 [`writable_roots`] 之内,
/// 否则 `PathEscape`;路径任意一层是 `.git` / `.agents` / `.aws` 分量,
/// 或落在 `.zhuye` 的提权子项([`PROTECTED_ZHUYE_CHILDREN`])之下则拒绝
/// (读不受影响)。文件不存在不报错,由调用方的打开 / 创建步骤自行处理。
pub fn resolve_writable_path(
    workspace: &Path,
    extra_writable: &[PathBuf],
    path: &Path,
) -> Result<PathBuf, ToolError> {
    let canonical_workspace = workspace
        .canonicalize()
        .map_err(|e| ToolError::Io(e.to_string()))?;
    let final_path = resolve_components(&canonical_workspace, path);
    let roots = writable_roots(&canonical_workspace, extra_writable);
    if !roots.iter().any(|root| final_path.starts_with(root)) {
        warn!(
            path = %path.display(),
            resolved = %final_path.display(),
            "write: path escapes writable roots, rejecting"
        );
        return Err(ToolError::PathEscape {
            path: path.to_path_buf(),
        });
    }
    if let Some(component) = find_protected_component(&final_path) {
        warn!(
            path = %final_path.display(),
            "write: 工作区元数据目录拒绝写入"
        );
        return Err(ToolError::PermissionDenied {
            reason: format!(
                "拒绝写入工作区元数据目录 {component}\
                 (.git/.zhuye/.agents/.aws 保持只读;读不受影响)"
            ),
        });
    }
    Ok(final_path)
}

/// 找出路径中受写保护的元数据分量。
///
/// `.git` / `.agents` / `.aws` 出现在任意一层即拒绝;`.zhuye` 例外 ——
/// 其下的 `worktrees` 子树(side-channel / worktree 任务的工作目录)与
/// `plan` 子树(PlanWrite 草稿)是引擎托管的合法写入目标,不是「改了
/// 就能借宿主身份提权」的元数据,放行。
fn find_protected_component(final_path: &Path) -> Option<String> {
    let names: Vec<String> = final_path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    for (idx, name) in names.iter().enumerate() {
        if name != ".zhuye" {
            // `.git` / `.agents` / `.aws` 任意一层出现即拒绝(提权面)。
            if PROTECTED_WRITE_COMPONENTS.contains(&name.as_str()) {
                return Some(name.clone());
            }
            continue;
        }
        // `.zhuye` 反转为 denylist:只保护明确的提权面(配置 / 权限 /
        // 会话历史 / 计划任务等),其余子树(spill / plan / worktrees /
        // uploads 等引擎托管的工作数据)放行。枚举式豁免(worktrees /
        // plan)已被证伪 —— spill(v1.11)是第三个被误伤的引擎写入点,
        // 与其追着新工作目录打补丁,不如把保护面收敛到真正的提权清单。
        if let Some(next) = names.get(idx + 1) {
            if PROTECTED_ZHUYE_CHILDREN.contains(&next.as_str()) {
                return Some(format!("{name}/{next}"));
            }
        }
    }
    None
}

/// 把 `path` 解析到 `workspace` 下,确保结果位于 workspace 内。
/// 若越界则返回 `ToolError::PathEscape`。
///
/// 旧的工作区墙,保留给仍按工作区收敛的集成(LSP 等);文件类工具已
/// 迁移到 [`resolve_readable_path`] / [`resolve_writable_path`]。
pub fn resolve_sandbox_path(workspace: &Path, path: &Path) -> Result<PathBuf, ToolError> {
    let abs_workspace = workspace
        .canonicalize()
        .map_err(|e| ToolError::Io(e.to_string()))?;
    let abs_path = if path.is_absolute() {
        path.canonicalize()
            .map_err(|e| ToolError::Io(e.to_string()))?
    } else {
        abs_workspace
            .join(path)
            .canonicalize()
            .map_err(|e| ToolError::Io(e.to_string()))?
    };
    if !abs_path.starts_with(&abs_workspace) {
        return Err(ToolError::PathEscape {
            path: path.to_path_buf(),
        });
    }
    Ok(abs_path)
}

/// 把**目录类**路径参数(grep / glob 的 `path`)解析到 workspace 内。
///
/// 0.6:grep 与 glob 的 `path` 参数此前直接喂给 `WalkBuilder::new(root)`,
/// 没有任何 workspace 包含校验 —— `{"pattern": "x", "path": "/"}` 会遍历
/// 整个文件系统,并把匹配行内容(含绝对路径)吐给模型。`read` 虽然还会拒读
/// 这些路径,但内容已经在工具输出里泄露了。
///
/// 与 [`resolve_sandbox_path`] 的差别只在错误信息:这里的调用场景是"搜索
/// 根目录",路径不存在 / 不是目录时应给出可操作的提示,而不是裸 Io 错误。
pub fn resolve_sandbox_dir(workspace: &Path, path: &Path) -> Result<PathBuf, ToolError> {
    let resolved = resolve_sandbox_path(workspace, path).map_err(|e| match e {
        ToolError::PathEscape { .. } => e,
        // 规范化失败几乎总是"不存在",给搜索场景一句人话。
        ToolError::Io(_) => ToolError::InvalidArgs {
            message: format!(
                "path 不存在或无法访问: {} (相对 workspace 或 workspace 内的绝对路径)",
                path.display()
            ),
        },
        other => other,
    })?;
    if !resolved.is_dir() {
        return Err(ToolError::InvalidArgs {
            message: format!("path 不是目录: {}", path.display()),
        });
    }
    Ok(resolved)
}

/// 把 workspace 内的绝对路径转成相对 workspace 的显示路径。
///
/// 0.6:grep / glob 的输出此前用 `entry.path().display()`,对 workspace 根
/// 启动的搜索会给出冗长的绝对路径(每一行都带一遍),既烧 token 又不便读。
/// 转不成相对路径时(符号链接指向 workspace 外等)原样返回绝对路径。
pub fn display_path_relative(path: &Path, workspace: &Path) -> String {
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    abs.strip_prefix(&workspace)
        .unwrap_or(&abs)
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn setup() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("zhuye_sandbox_test_{}_{}", std::process::id(), n));
        if dir.exists() {
            fs::remove_dir_all(&dir).ok();
        }
        let inner = dir.join("inner");
        fs::create_dir_all(&inner).unwrap();
        fs::write(inner.join("a.txt"), "hello").unwrap();
        inner
    }

    fn teardown(inner: &Path) {
        let _ = fs::remove_dir_all(inner.parent().unwrap());
    }

    #[test]
    fn relative_path_under_workspace_ok() {
        let inner = setup();
        let canonical_inner = inner.canonicalize().unwrap();
        let resolved = resolve_sandbox_path(&inner, Path::new("a.txt")).unwrap();
        assert!(resolved.starts_with(&canonical_inner));
        teardown(&inner);
    }

    #[test]
    fn absolute_path_under_workspace_ok() {
        let inner = setup();
        let abs = inner.join("a.txt");
        let resolved = resolve_sandbox_path(&inner, &abs).unwrap();
        assert_eq!(resolved, abs.canonicalize().unwrap());
        teardown(&inner);
    }

    #[test]
    fn path_escape_returns_error() {
        let inner = setup();
        // 使用一个不在 inner 内的路径。
        let result = resolve_sandbox_path(&inner, Path::new("../etc/passwd"));
        // 可能是 PathEscape(canonicalize 失败或越界)或 Io(文件不存在) —
        // 两者都视为不安全信号,可接受。
        assert!(result.is_err());
        teardown(&inner);
    }

    #[test]
    fn missing_file_returns_io_error() {
        let inner = setup();
        let result = resolve_sandbox_path(&inner, Path::new("missing.txt"));
        assert!(matches!(result, Err(ToolError::Io(_))));
        teardown(&inner);
    }

    // ── workspace-write 口径:读全盘,写限根 ─────────────────────────

    #[test]
    fn readable_path_outside_workspace_ok() {
        let inner = setup();
        // 系统文件在 workspace 外,读应放行(编译 / 分析需要)。
        let resolved = resolve_readable_path(&inner, Path::new("/etc/hosts"));
        assert!(resolved.is_ok() || matches!(resolved, Err(ToolError::Io(_))));
        if let Ok(abs) = resolved {
            assert!(abs.starts_with("/etc") || abs.starts_with("/private/etc"));
        }
        teardown(&inner);
    }

    #[test]
    fn readable_credential_path_denied() {
        let inner = setup();
        // $HOME/.ssh 是凭证目录:直接以该目录为读目标(存在即触发,
        // 不依赖某个具体的密钥文件存在)。
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .expect("测试需要 HOME 或 USERPROFILE");
        let ssh_dir = Path::new(&home).join(".ssh");
        if ssh_dir.is_dir() {
            let result = resolve_readable_path(&inner, &ssh_dir);
            assert!(
                matches!(result, Err(ToolError::PermissionDenied { .. })),
                "凭证路径必须拒绝: {result:?}"
            );
        }
        teardown(&inner);
    }

    /// 写根之外的探针目录(temp 自身是写根,负例必须用 $HOME)。
    fn probe_outside_roots(tag: &str) -> PathBuf {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .expect("测试需要 HOME 或 USERPROFILE");
        let dir = Path::new(&home).join(format!(
            ".zhuye_sb_unit_probe_{}_{}",
            tag,
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writable_extra_dir_ok_and_outside_denied() {
        let inner = setup();
        let outside = inner.parent().unwrap().join("extra-writable");
        fs::create_dir_all(&outside).unwrap();

        // 附加可写目录内的文件:允许(含尚不存在的新建路径)。
        let target = outside.join("new.txt");
        let resolved =
            resolve_writable_path(&inner, std::slice::from_ref(&outside), &target).unwrap();
        assert!(resolved.starts_with(outside.canonicalize().unwrap()));

        // $HOME 下不在任何允许根内:拒绝。
        let probe = probe_outside_roots("extra");
        let elsewhere = probe.join("f.txt");
        assert!(matches!(
            resolve_writable_path(&inner, &[outside], Path::new(&elsewhere)),
            Err(ToolError::PathEscape { .. })
        ));
        let _ = fs::remove_dir_all(&probe);

        // 相对路径仍以 workspace 为基,workspace 内允许。
        assert!(resolve_writable_path(&inner, &[], Path::new("src/new.rs")).is_ok());
        teardown(&inner);
    }

    #[test]
    fn writable_tmp_ok() {
        let inner = setup();
        let target = std::env::temp_dir().join("zhuye_write_probe.txt");
        assert!(resolve_writable_path(&inner, &[], &target).is_ok());
        teardown(&inner);
    }

    #[test]
    fn writable_git_and_workspace_meta_denied_but_readable_ok() {
        let inner = setup();
        let git_dir = inner.join(".git");
        fs::create_dir_all(git_dir.join("hooks")).unwrap();

        // 写 .git/hooks:拒绝(提权面),读 .git/config:放行。
        assert!(matches!(
            resolve_writable_path(&inner, &[], Path::new(".git/hooks/pre-commit")),
            Err(ToolError::PermissionDenied { .. })
        ));
        assert!(resolve_readable_path(&inner, Path::new(".git")).is_ok());

        // 绝对路径与嵌套分量同样拦。
        let abs = git_dir.canonicalize().unwrap().join("config");
        assert!(matches!(
            resolve_writable_path(&inner, &[], &abs),
            Err(ToolError::PermissionDenied { .. })
        ));
        // .gitignore 不受影响(整名比较,不是前缀)。
        assert!(resolve_writable_path(&inner, &[], Path::new(".gitignore")).is_ok());
        teardown(&inner);
    }

    /// v1.3 回归:`.zhuye` 元数据默认拒写,但其下的 `worktrees`(side-channel
    /// 工作树)与 `plan`(PlanWrite 草稿)是引擎托管的工作数据,必须放行 ——
    /// 否则 A23 side-channel 与 plan 草稿落盘全被沙箱挡掉(CI 实证)。
    /// `sessions` 等其余子树保持拒绝。
    #[test]
    fn zhuye_worktrees_and_plan_writable_but_sessions_denied() {
        let inner = setup();

        // worktrees / plan:放行(新建文件路径,无需真实存在)。
        assert!(
            resolve_writable_path(
                &inner,
                &[],
                Path::new(".zhuye/worktrees/side/side-output.txt")
            )
            .is_ok()
        );
        assert!(resolve_writable_path(&inner, &[], Path::new(".zhuye/plan/draft.md")).is_ok());

        // sessions / 根下散文件:拒绝(会话与配置是提权面)。
        assert!(matches!(
            resolve_writable_path(&inner, &[], Path::new(".zhuye/sessions/2026/x.jsonl")),
            Err(ToolError::PermissionDenied { .. })
        ));
        assert!(matches!(
            resolve_writable_path(&inner, &[], Path::new(".zhuye/config.toml")),
            Err(ToolError::PermissionDenied { .. })
        ));

        // 深层 worktree(多层嵌套)同样放行。
        assert!(
            resolve_writable_path(
                &inner,
                &[],
                Path::new(".zhuye/worktrees/feature-a/deep/nested/file.txt")
            )
            .is_ok()
        );
        teardown(&inner);
    }

    #[test]
    fn writable_symlink_escape_denied() {
        let inner = setup();
        // workspace 内建符号链接指向写根之外($HOME 探针),写入应被
        // 解析后的真实路径拦下。
        let probe = probe_outside_roots("sym");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&probe, inner.join("link")).unwrap();
        #[cfg(unix)]
        assert!(matches!(
            resolve_writable_path(&inner, &[], Path::new("link/pwned.txt")),
            Err(ToolError::PathEscape { .. })
        ));
        let _ = fs::remove_dir_all(&probe);
        teardown(&inner);
    }
}
