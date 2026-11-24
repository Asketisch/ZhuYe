//! `os_sandbox` — v1.2 P0-1 真实 OS 沙箱(macOS Seatbelt + Linux Landlock)。
//!
//! 把 v0 的 stub 升级为真正限制文件系统访问的沙箱。bash 工具默认在外层
//! 沙箱中执行,**`rm -rf /` 等破坏系统操作被内核拒绝**(`Operation not
//! permitted`)。
//!
//! ## 平台后端
//!
//! - **macOS**:Seatbelt,经 `/usr/bin/sandbox-exec` 外部进程 + 生成的
//!   `.sb` profile(`(version 1)(allow default deny) ...`)。允许写
//!   workspace + 可选额外目录 + 临时目录;读全盘保留(编译/分析常需读
//!   系统头文件);网络默认关。
//! - **Linux**:Landlock ABI v3+,经 syscall(`landlock_create_ruleset`
//!   / `landlock_add_rule`),在 `pre_exec` 之间(子进程 fork 后、exec 前)
//!   应用。对 workspace 写、workspace 外读写按规则放行,其余写拒。
//!   沙箱启用时，内核不支持或安装失败会拒绝执行。
//!
//! ## 不变式
//!
//! - **v1.3 起**:`OsSandbox::default()` 返回 `enabled = true`,以满足"默认
//!   要求 OS sandbox"的严格安全基线。要回到旧透传行为显式设
//!   `enabled = false` 或用 `OsSandbox::passthrough()`。
//! - `strict = true`(默认)→ 后端不可用 / profile 写失败 / Landlock
//!   不支持 时 `enforce_or_fail()` 返回 `Err`,由 BashTool 把
//!   `ToolError::SandboxUnavailable` 抛出,**绝不**回退裸 `sh -c`。
//! - Linux 已启用沙箱时无论 strict 如何都失败关闭；显式关闭才可裸执行。
//!   其他平台的非严格模式保留原有降级语义。
//!
//! 验证标准(gap doc):sandbox 内执行 `rm -rf /` 应被阻止。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(target_os = "linux")]
#[path = "network_filter.rs"]
mod network_filter;
// 仅 seatbelt(macOS)使用;其余平台保持 import 干净,避免 -D warnings 下
// unused import 报错。
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicU64, Ordering};

/// seatbelt 临时 profile 文件序号:同一进程内并发调用 `seatbelt_argv`
/// 时 pid 相同,必须靠自增序号区分(见 `seatbelt_argv` 内注释)。
#[cfg(target_os = "macos")]
static SEATBELT_PROFILE_SEQ: AtomicU64 = AtomicU64::new(0);

/// 沙箱后端状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsSandboxStatus {
    /// 未启用(默认;透传所有命令)。
    Disabled,
    /// 已启用且 backend 为 macOS Seatbelt(`sandbox-exec`)。
    Seatbelt,
    /// 已启用且 backend 为 Linux Landlock。
    Landlock,
    /// 已启用但当前平台无可用 backend(降级 = 不沙箱)。
    NoBackend,
    /// 已启用且 backend 为 Windows Job Object(+ AppContainer,F24)。
    /// Job Object 在任意 Windows 上可用,无需特权。
    WindowsJobObject,
}

/// 默认受保护的元数据目录名(位于每个可写根的顶层)。
///
/// 与 `zhuye-tools::sandbox` 的 `PROTECTED_WRITE_COMPONENTS` 同口径:
/// `.git`(改 hook 即提权)、`.agents`、`.aws`(凭据)都是「改了就能借宿主
/// 身份提权」的位置。macOS Seatbelt 可用 deny 规则强制只读;Linux
/// Landlock 的规则按位合并,无法在可写根内撤销嵌套路径的写权(见
/// [`PathProtectionVerdict::Unsupported`])。
pub const DEFAULT_PROTECTED_ENTRIES: [&str; 3] = [".git", ".agents", ".aws"];

/// `.git` 是否承载用户资产:HEAD 指向的分支**有提交**即视为用户仓库
/// (改它的 hook / config 即提权 —— 用户随后在自己的终端跑 git,hook
/// 以用户身份、在沙箱外执行);刚 `git init` 的空仓库(unborn HEAD,无
/// 任何提交)视为脚手架,归 agent 所有,全程可写。
///
/// 判定失败(读不到 HEAD 等)一律 fail-closed 视为用户资产;`.git` 为
/// 文件(worktree 指针)同样 fail-closed —— 穿过它的写落在主仓库
/// `.git`,后者有独立的保护口径。
fn git_dir_is_user_owned(git_dir: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(git_dir) else {
        return false; // 不存在 → 非用户资产
    };
    if !meta.is_dir() {
        return true; // worktree 指针文件 → fail-closed
    }
    let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) else {
        return true; // 读不到 HEAD → fail-closed
    };
    let head = head.trim();
    let Some(ref_name) = head.strip_prefix("ref: ") else {
        return true; // detached HEAD(指向具体 commit)→ 必有历史
    };
    let ref_name = ref_name.trim();
    if git_dir.join(ref_name).symlink_metadata().is_ok() {
        return true; // refs/heads/<branch> 存在 → 有提交
    }
    match std::fs::read_to_string(git_dir.join("packed-refs")) {
        Ok(packed) => packed
            .lines()
            .any(|line| line.split_whitespace().nth(1) == Some(ref_name)),
        Err(_) => false, // 松散与打包引用都没有 → unborn HEAD → 空仓库
    }
}

/// 可写根顶层的 `.git` 是否受保护(非空仓库 = 用户资产)。
fn root_git_is_protected(root: &Path) -> bool {
    git_dir_is_user_owned(&root.join(".git"))
}

/// 会话级 `.git` 归属快照:可写根 → 本会话**首次**为该根构建 profile 时,
/// 其顶层 `.git` 是否为用户资产(有提交历史的仓库,见
/// [`root_git_is_protected`])。
///
/// `.git` 只读保护的是**会话开始前就属于用户的仓库** —— 改它的 hook /
/// config 即提权(用户随后在自己的终端里跑 git,hook 以用户身份、在沙箱
/// 外执行)。会话内 agent `git init` 出来的仓库(含跨会话续作时仍是
/// 空仓库的情形)没有这层含义:其中的一切与 agent 写的脚本同级信任,
/// 必须**全程**可写 —— 提交历史一经产生,下个会话起按用户资产恢复保护。
///
/// 判定必须锚在会话上,不能按「此刻是否存在」逐条命令判:`OsSandbox`
/// 每条命令新建、profile 每条命令重建,逐命令判定会让 agent 卡进「init
/// 与 add 在同一条命令里成功,下一条命令起整个 `.git` 变只读,commit
/// 永远拿不到 index.lock」的半截状态(桌面 sess_99d9bf0a 实证)。而无条件
/// deny 则连 `git init` 本身都不可能(更早的实证)。
///
/// 判定时刻取「首次为该根构建 profile」而非会话对象创建:第一条 bash
/// 命令之前 agent 没有任何手段创建 `.git`(文件工具层拒绝含 `.git` 分量
/// 的路径),两者等价;这样也不必在配置装配期就凑齐全部可写根(workspace
/// 可热切换,额外可写目录每 turn 下发)。
///
/// `.agents` / `.aws` / `.zhuye` 不在快照范围内,恒受保护(凭据与引擎
/// 托管配置不应由 agent 创建)。
#[derive(Clone, Default)]
pub struct GitProtectionSnapshot(Arc<std::sync::Mutex<HashMap<PathBuf, bool>>>);

impl GitProtectionSnapshot {
    pub fn new() -> Self {
        Self::default()
    }

    /// `root` 顶层的 `.git` 在本会话内是否受保护;首次询问时按归属判定
    /// 并记忆,之后不再随文件系统变化(下一条命令就要写 `.git`)。
    pub fn protects(&self, root: &Path) -> bool {
        let mut roots = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *roots
            .entry(root.to_path_buf())
            .or_insert_with(|| root_git_is_protected(root))
    }
}

impl std::fmt::Debug for GitProtectionSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitProtectionSnapshot")
            .field("roots", &self.0.lock().map_or(0, |roots| roots.len()))
            .finish()
    }
}

/// 单条敏感路径的保护结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathProtectionVerdict {
    /// 平台后端已强制该路径只读(写被内核拒绝)。
    Enforced,
    /// 该路径落在所有可写根之外 —— 写本就被默认拒绝,等价于只读。
    EnforcedByDefault,
    /// 平台无法表达该策略:可写根**内**的嵌套路径无法被单独撤销写权。
    Unsupported {
        /// 面向宿主的原因说明(中文,可直接展示)。
        reason: String,
    },
}

/// 一条敏感路径的保护状态(报告给宿主的能力面)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathProtection {
    /// 请求方给出的路径(相对路径按 workspace 解析前保留原样)。
    pub requested: PathBuf,
    /// 解析符号链接后的真实路径(不存在时退回请求路径)。
    pub resolved: PathBuf,
    /// 是否为宿主显式要求的路径(`false` = 默认元数据条目)。
    pub explicit: bool,
    /// 平台是否真的强制了只读。
    pub verdict: PathProtectionVerdict,
}

/// Landlock 无法在可写根内撤销嵌套路径写权的原因(统一文案)。
const LANDLOCK_NESTED_UNSUPPORTED: &str = "Landlock 的规则按位合并:可写根授予的写权会传播到其下的嵌套路径,\
     内核没有按路径撤销(deny)规则的能力,因此可写根内的嵌套敏感路径无法被强制只读";

/// `ZHUYE_SANDBOX_STRICT_PATHS` → 敏感路径保护必须落地(默认关)。
fn strict_paths_from_env() -> bool {
    std::env::var("ZHUYE_SANDBOX_STRICT_PATHS")
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}

/// 检测当前平台可用的沙箱 backend。
///
/// - macOS:`sandbox-exec` 在 `/usr/bin/sandbox-exec` → `Seatbelt`。
/// - Linux:Landlock syscall 可用性在 apply 时探测,这里乐观返回 `Landlock`
///   (内核不支持时 apply 降级)。
/// - 其他平台 → `NoBackend`。
pub fn detect_backend() -> OsSandboxStatus {
    #[cfg(target_os = "macos")]
    {
        if Path::new("/usr/bin/sandbox-exec").exists() {
            OsSandboxStatus::Seatbelt
        } else {
            OsSandboxStatus::NoBackend
        }
    }
    #[cfg(target_os = "linux")]
    {
        OsSandboxStatus::Landlock
    }
    #[cfg(target_os = "windows")]
    {
        // F24:Job Object 在任意 Windows 可用(无需特权);
        // AppContainer profile 由 spawn 路径按 plan 创建。
        OsSandboxStatus::WindowsJobObject
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        OsSandboxStatus::NoBackend
    }
}

/// OS 级沙箱配置。
///
/// **v1.3 默认值翻转**:`enabled = true`(v1.2 是 `false`),以满足
/// "默认要求 OS sandbox" 的严格安全基线。要回到旧透传行为显式
/// 调 `OsSandbox::passthrough()` 或 `OsSandbox::with_enabled(false, ...)`。
/// `strict = true`(默认)→ 后端不可用 / profile 写失败 / Landlock 不支持
/// 时 `enforce_or_fail()` 返回 `Err`,由 BashTool 抛
/// `ToolError::SandboxUnavailable`,绝不回退裸 `sh -c`。
#[derive(Debug, Clone)]
pub struct OsSandbox {
    pub enabled: bool,
    /// 额外可写目录(workspace 总是可写)。绝对路径。
    pub writable_dirs: Vec<PathBuf>,
    /// 是否允许出站网络。默认 false。
    pub allow_network: bool,
    /// v1.3:严格模式。`true` 时后端不可用 / profile 写失败 → 返回
    /// `Err` 而不是降级透传(BashTool 据此抛 `ToolError::SandboxUnavailable`)。
    /// `false` 时其他平台保留降级语义；Linux 启用沙箱时仍失败关闭。
    pub strict: bool,
    /// 宿主显式要求只读的敏感路径(绝对路径,或相对 workspace 的路径)。
    ///
    /// 这些路径**必须**在沙箱内不可写。若当前平台无法表达该策略
    /// (见 [`PathProtectionVerdict::Unsupported`]),且 `strict_paths` 为真,
    /// [`OsSandbox::enforce_or_fail`] 会失败关闭 —— 平台做不到的事不静默放过。
    pub protected_paths: Vec<PathBuf>,
    /// 敏感路径保护是否必须落地。`true` 时任何「平台无法强制」的受保护
    /// 路径都会让沙箱拒绝执行(默认 false:报告但不阻断)。
    pub strict_paths: bool,
    /// 会话级 `.git` 归属快照(见 [`GitProtectionSnapshot`]):`Some` 时
    /// `.git` 仅在会话开始前已存在才受保护,agent 自建的仓库全程可写;
    /// `None` = 调用方没有会话上下文,`.git` 恒受保护(历史口径)。
    pub git_snapshot: Option<GitProtectionSnapshot>,
    /// 探测到的 backend(`enabled=false` 时为 `Disabled`)。
    backend: OsSandboxStatus,
}

impl Default for OsSandbox {
    fn default() -> Self {
        // v1.3:默认要求 OS sandbox,严格模式,fail-closed。
        Self {
            enabled: true,
            writable_dirs: Vec::new(),
            allow_network: false,
            strict: true,
            protected_paths: Vec::new(),
            strict_paths: false,
            git_snapshot: None,
            backend: detect_backend(),
        }
    }
}

impl OsSandbox {
    /// v1.3:显式透传构造(关闭 sandbox,不严格)。旧默认行为兜底;
    /// 调用方需明确表达"我接受无沙箱"。
    pub fn passthrough() -> Self {
        Self {
            enabled: false,
            writable_dirs: Vec::new(),
            allow_network: false,
            strict: false,
            protected_paths: Vec::new(),
            strict_paths: false,
            git_snapshot: None,
            backend: OsSandboxStatus::Disabled,
        }
    }

    /// 构造一个启用沙箱的实例,自动探测 backend。`writable_dirs` 是
    /// workspace 之外额外可写目录。
    pub fn enabled(writable_dirs: Vec<PathBuf>) -> Self {
        Self {
            enabled: true,
            writable_dirs,
            allow_network: false,
            strict: true,
            protected_paths: Vec::new(),
            strict_paths: false,
            git_snapshot: None,
            backend: detect_backend(),
        }
    }

    /// v1.3:从 env `ZHUYE_SANDBOX_OS_LEVEL` 读启用状态。
    /// - `0` / `false` / `no` / `off` → 关闭(`passthrough`)
    /// - 未设置 / `1` / `true` / `yes` / `on` → 启用(默认严格)
    ///
    /// `ZHUYE_SANDBOX_STRICT=0` 在非 Linux 平台保留旧降级语义；
    /// Linux 启用沙箱后仍要求成功安装规则。
    pub fn from_env() -> Self {
        Self::with_enabled(resolve_enabled(
            std::env::var("ZHUYE_SANDBOX_OS_LEVEL").ok().as_deref(),
            None,
        ))
    }

    /// v1.x:env > config > 基线 三段优先级解析出完整沙箱策略。
    ///
    /// 收编桌面端 `app-core/src/sandbox.rs` 的纯函数口径(env 优先是
    /// 运维临时降级的逃生口),并修复核心 CLI 此前的桥接缺口:
    /// `[sandbox].os_level = Some(false)` **不生效**(headless 只桥接
    /// `Some(true)`,因为 `from_env()` 未设 env 时默认开启)。
    ///
    /// `writable_dirs` / `allow_network` 仅在沙箱启用时有意义(关闭时
    /// 本就不受限);strict 仍读 env(`ZHUYE_SANDBOX_STRICT`)。
    pub fn resolve(
        cfg_os_level: Option<bool>,
        writable_dirs: Vec<PathBuf>,
        allow_network: bool,
    ) -> Self {
        let on = resolve_enabled(
            std::env::var("ZHUYE_SANDBOX_OS_LEVEL").ok().as_deref(),
            cfg_os_level,
        );
        let mut s = Self::with_enabled(on);
        if on {
            s.writable_dirs = writable_dirs;
            s.allow_network = allow_network;
        }
        s
    }

    /// v1.5 R1:显式指定开关的构造器 —— `ThreadSettingsOverrides
    /// .sandbox_policy` 每 turn 覆盖时使用,绕过 env。strict 仍读 env
    /// (`ZHUYE_SANDBOX_STRICT`,fail-closed 基线不变)。
    pub fn with_enabled(on: bool) -> Self {
        let strict = std::env::var("ZHUYE_SANDBOX_STRICT").ok().is_none_or(|s| {
            !matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        });
        if on {
            let mut s = Self::enabled(extra_writable_from_env());
            s.strict = strict;
            s.strict_paths = strict_paths_from_env();
            s
        } else {
            let mut s = Self::passthrough();
            s.strict = strict;
            s.strict_paths = strict_paths_from_env();
            s
        }
    }

    /// 追加「必须只读」的敏感路径(相对路径按 workspace 解析)。
    ///
    /// 与 [`OsSandbox::enforce_protected_paths`] 配套:`strict_paths` 为真时,
    /// 平台无法强制的路径会让执行失败关闭。
    pub fn with_protected_paths(
        mut self,
        paths: impl IntoIterator<Item = PathBuf>,
        strict: bool,
    ) -> Self {
        self.protected_paths = paths.into_iter().collect();
        self.strict_paths = strict;
        self
    }

    /// 当前生效状态(综合 enabled + backend)。
    pub fn status(&self) -> OsSandboxStatus {
        if !self.enabled {
            OsSandboxStatus::Disabled
        } else {
            self.backend
        }
    }

    /// 人可读状态行(给 TUI / `/sandbox-toggle` pill / 启动 banner 用)。
    /// v1.3:状态行同时标注严格模式 / 网络边界 / 敏感读规则,便于运维
    /// 一眼看到当前安全下限。
    ///
    /// 网络策略按后端报告；Linux 断网经子进程 seccomp 安装，安装失败时
    /// 命令不启动。Seatbelt 关网时仍放行 localhost 与 Unix socket。
    pub fn status_line(&self) -> String {
        let net = match self.status() {
            // Seatbelt 真拦出站(默认放行 localhost / Unix socket)。
            OsSandboxStatus::Seatbelt => {
                if self.allow_network {
                    "网络:允许".to_string()
                } else {
                    "网络:仅本机(localhost+Unix socket)".to_string()
                }
            }
            OsSandboxStatus::Landlock => {
                if self.allow_network {
                    "网络:不受沙箱约束".to_string()
                } else {
                    "网络:请求禁止创建和连接套接字(seccomp，命令成功启动后生效；允许 socketpair)"
                        .to_string()
                }
            }
            OsSandboxStatus::Disabled => "网络:不受沙箱约束".to_string(),
            OsSandboxStatus::NoBackend if self.strict || cfg!(target_os = "linux") => {
                "网络:不适用(无法执行)".to_string()
            }
            OsSandboxStatus::NoBackend => "网络:不受沙箱约束".to_string(),
            OsSandboxStatus::WindowsJobObject => {
                "网络:未实施(Windows 后端未接入 bash spawn 路径)".to_string()
            }
        };
        let sensitive = match self.status() {
            OsSandboxStatus::Seatbelt => "敏感读:默认拒绝(凭据/SSH/钥匙串)",
            OsSandboxStatus::NoBackend if self.strict || cfg!(target_os = "linux") => {
                "敏感读:不适用(无法执行)"
            }
            OsSandboxStatus::WindowsJobObject => {
                "敏感读:未实施(Windows 后端未接入 bash spawn 路径)"
            }
            _ => "敏感读:不受沙箱约束",
        };
        match self.status() {
            OsSandboxStatus::Disabled => {
                format!(
                    "os-sandbox: 未启用(透传,bash 裸执行);{net};{sensitive};strict={}",
                    self.strict
                )
            }
            OsSandboxStatus::Seatbelt => {
                format!(
                    "os-sandbox: 已请求 macOS Seatbelt(bash 启动成功后受 sandbox-exec 约束);{net};{sensitive};strict={}",
                    self.strict
                )
            }
            OsSandboxStatus::Landlock => {
                format!(
                    "os-sandbox: 已请求 Linux Landlock(bash 启动成功后生效);{net};{sensitive};strict={}",
                    self.strict
                )
            }
            OsSandboxStatus::WindowsJobObject => {
                // 诚实口径:Windows 后端(Job Object + AppContainer)已实现,
                // 但**尚未接入 bash 的 spawn 路径** —— 命令实际未受文件/网络
                // 约束,且未在 Windows 真机验证过行为。宿主据此判断能力边界。
                format!(
                    "os-sandbox: Windows Job Object 后端已实现但未接入 bash spawn 路径(命令未受文件/网络约束,未在真机验证);{net};{sensitive};strict={}",
                    self.strict
                )
            }
            OsSandboxStatus::NoBackend => {
                if self.strict || cfg!(target_os = "linux") {
                    format!(
                        "os-sandbox: 已请求启用但当前平台无可用 backend — BashTool 会失败关闭;{net};{sensitive};strict={}",
                        self.strict
                    )
                } else {
                    format!(
                        "os-sandbox: 已请求启用但当前平台无可用 backend(降级透传);{net};{sensitive};strict={}",
                        self.strict
                    )
                }
            }
        }
    }

    /// 是否真的会把命令放进沙箱(用于 BashTool 决定是否 wrap)。
    pub fn is_active(&self) -> bool {
        matches!(
            self.status(),
            OsSandboxStatus::Seatbelt | OsSandboxStatus::Landlock
        )
    }

    /// 敏感路径保护报告 —— 逐条说明平台**是否真的**强制了只读。
    ///
    /// 报告对象 = 默认元数据条目([`DEFAULT_PROTECTED_ENTRIES`],位于每个
    /// 可写根顶层)+ 宿主显式要求的 [`OsSandbox::protected_paths`]。
    ///
    /// 判定口径:
    /// - 路径落在所有可写根**之外** → 写本就被默认拒绝(`EnforcedByDefault`);
    /// - 路径落在可写根**之内**且后端是 macOS Seatbelt → deny 规则强制
    ///   只读(`Enforced`);
    /// - 路径落在可写根**之内**且后端是 Linux Landlock → 内核无法按路径
    ///   撤销写权(`Unsupported`,附原因)。
    ///
    /// 符号链接按真实路径判定:软链指向可写根外 → `EnforcedByDefault`;
    /// 指向可写根内 → 与目标同口径(不能用软链把「受保护」降级成
    /// 「不受保护」,也不能靠软链把「不可写」说成「可写」)。
    pub fn path_protection_report(&self, workspace: &Path) -> Vec<PathProtection> {
        let roots: Vec<PathBuf> = std::iter::once(workspace.to_path_buf())
            .chain(self.writable_dirs.iter().cloned())
            .map(|root| root.canonicalize().unwrap_or(root))
            .collect();
        let mut report: Vec<PathProtection> = Vec::new();

        // 默认元数据条目:每个可写根顶层的 `.git` / `.agents` / `.aws`。
        // `.git` 的归属随会话快照(见 git_is_protected):agent 自建的仓库
        // 实际可写,报告它只会撒谎。
        for root in &roots {
            for name in DEFAULT_PROTECTED_ENTRIES {
                if name == ".git" && !self.git_is_protected(root) {
                    continue;
                }
                let candidate = root.join(name);
                report.push(self.classify(candidate.clone(), candidate, false, &roots));
            }
        }
        // 宿主显式要求的路径(相对路径按 workspace 解析)。
        for requested in &self.protected_paths {
            let absolute = if requested.is_absolute() {
                requested.clone()
            } else {
                workspace.join(requested)
            };
            let resolved = absolute.canonicalize().unwrap_or_else(|_| absolute.clone());
            report.push(self.classify(requested.clone(), resolved, true, &roots));
        }
        report
    }

    /// 单条路径的判定(报告与失败关闭共用同一口径)。
    fn classify(
        &self,
        requested: PathBuf,
        resolved: PathBuf,
        explicit: bool,
        roots: &[PathBuf],
    ) -> PathProtection {
        let inside_writable_root = roots.iter().any(|root| resolved.starts_with(root));
        let status = self.status();
        let verdict = if !matches!(
            status,
            OsSandboxStatus::Seatbelt | OsSandboxStatus::Landlock
        ) {
            PathProtectionVerdict::Unsupported {
                reason: format!("当前平台无可用沙箱后端(status={status:?}),无法强制敏感路径只读"),
            }
        } else if !inside_writable_root {
            // 可写根之外:写被默认拒绝,等价只读。
            PathProtectionVerdict::EnforcedByDefault
        } else {
            match self.status() {
                // Seatbelt 支持 deny 规则且后匹配优先 → 可写根内的嵌套
                // 路径可以真正只读。
                OsSandboxStatus::Seatbelt => PathProtectionVerdict::Enforced,
                // Landlock 的规则按位合并,没有按路径撤销的能力。
                OsSandboxStatus::Landlock => PathProtectionVerdict::Unsupported {
                    reason: LANDLOCK_NESTED_UNSUPPORTED.to_string(),
                },
                _ => PathProtectionVerdict::Unsupported {
                    reason: format!(
                        "当前平台无可用沙箱后端(status={:?}),无法强制敏感路径只读",
                        self.status()
                    ),
                },
            }
        };
        PathProtection {
            requested,
            resolved,
            explicit,
            verdict,
        }
    }

    /// 报告里「平台无法强制」的受保护路径(仅显式要求的)。
    pub fn unprotected_paths(&self, workspace: &Path) -> Vec<PathProtection> {
        self.path_protection_report(workspace)
            .into_iter()
            .filter(|item| {
                item.explicit && matches!(item.verdict, PathProtectionVerdict::Unsupported { .. })
            })
            .collect()
    }

    /// 敏感路径保护的失败关闭检查。
    ///
    /// `strict_paths = false`(默认):只报告不阻断 —— 平台做不到的事不
    /// 静默宣称成功,但也不让整个会话不可用。
    ///
    /// `strict_paths = true`:宿主明确要求「这些路径必须只读」,而当前
    /// 平台无法强制 → 返回 `Err`,由 BashTool 抛
    /// `ToolError::SandboxUnavailable`,**绝不**在无保护的情况下执行。
    pub fn enforce_protected_paths(&self, workspace: &Path) -> Result<(), String> {
        if !self.strict_paths {
            return Ok(());
        }
        let unprotected = self.unprotected_paths(workspace);
        if unprotected.is_empty() {
            return Ok(());
        }
        let detail = unprotected
            .iter()
            .map(|item| {
                let reason = match &item.verdict {
                    PathProtectionVerdict::Unsupported { reason } => reason.as_str(),
                    _ => "未知原因",
                };
                format!("{}: {reason}", item.resolved.display())
            })
            .collect::<Vec<_>>()
            .join("；");
        Err(format!(
            "宿主要求的敏感路径只读策略在当前平台无法强制({detail});\
             拒绝在无保护的情况下执行。如需继续,请关闭该要求或改用支持该策略的平台。"
        ))
    }

    /// v1.3:fail-closed 检查。`strict && enabled` 时,后端不可用 / 平台
    /// 不是 macOS+Linux → 返回 `Err(reason)`;BashTool 据此抛
    /// `ToolError::SandboxUnavailable` 而不是裸 `sh -c`。
    /// Linux 无论 strict 如何均要求所请求的沙箱可用；其他平台非严格模式保留降级。
    ///
    /// Linux 先查询 Landlock ABI；缺少 ABI v3 的截断控制时拒绝
    /// 启动。实际安装仍在子进程中完成，安装失败会返回启动错误。
    pub fn enforce_or_fail(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(()); // 显式关闭 → 不检查
        }
        if !self.strict && !cfg!(target_os = "linux") {
            return Ok(()); // 非严格模式 → 允许降级
        }
        match self.status() {
            OsSandboxStatus::Seatbelt | OsSandboxStatus::Landlock => {
                // Linux:detect_backend() 是乐观的,补一次非破坏性内核探测,
                // 不让"内核无 Landlock"静默降级成没沙箱。
                #[cfg(target_os = "linux")]
                if !landlock_kernel_supported() {
                    return Err(
                        "OS sandbox 后端不可用:Linux 内核不支持 Landlock ABI v3(需 >= 6.2 且启用 \
                         Landlock LSM);拒绝无沙箱执行。如需关闭沙箱，显式设置 \
                         ZHUYE_SANDBOX_OS_LEVEL=0。"
                            .into(),
                    );
                }
                Ok(())
            }
            // F24:Windows Job Object 无需内核探测(任意 Windows 可用);
            // AppContainer profile 失败时 spawn 路径已降级并 warn。
            OsSandboxStatus::WindowsJobObject => Ok(()),
            OsSandboxStatus::NoBackend => Err(format!(
                "OS sandbox 后端不可用(平台:{});拒绝裸执行。\
                 在 macOS 需 /usr/bin/sandbox-exec;在 Linux 需 Landlock ABI v3(>= 6.2)。",
                std::env::consts::OS
            )),
            OsSandboxStatus::Disabled => Err("OsSandbox 状态矛盾:enabled=true 但 Disabled".into()),
        }
    }

    /// 可写根 `root` 顶层的 `.git` 是否受保护:有会话快照时随快照
    /// ([`GitProtectionSnapshot`]),没有则恒受保护。
    fn git_is_protected(&self, root: &Path) -> bool {
        match &self.git_snapshot {
            Some(snapshot) => snapshot.protects(root),
            None => true,
        }
    }

    // ── macOS Seatbelt ──────────────────────────────────────────────

    /// 生成 Seatbelt `.sb` profile 文本(给 `sandbox-exec -p`)。
    ///
    /// 策略:
    /// - `deny` 为默认(显式 allow 才放行)。
    /// - 允许写:workspace + `writable_dirs` + `$TMPDIR` + `/private/var/folders`
    ///   (macOS 真实 tempdir);**顶层 `.zhuye` 保持只读**,顶层 `.git`
    ///   仅保护会话开始前已存在的仓库(见 [`GitProtectionSnapshot`];改 hook
    ///   与配置是常见提权面,deny 规则置于末尾优先)。
    /// - 允许读:全盘(`(allow file-read*)`)—— 编译 / 分析常需读系统头与
    ///   依赖,限制读会大面积破坏可用性。写受限是核心安全边界。
    /// - 允许进程:`(allow process-exec)` 不限目录(子进程继承沙箱;
    ///   Xcode 的 git / 工作区构建产物不在四个固定 bin 目录里)。
    /// - 网络:`allow_network` 控制。开启 = `(allow network*)`;关闭 =
    ///   拒出站,但放行 localhost(bind/inbound/outbound)与 Unix socket
    ///   —— 本地 IPC(数据库 / docker / LSP server)是常见合法依赖。
    /// - **显式 deny** `(deny file-write*)` 到系统敏感路径作为兜底,即使
    ///   上面 allow 有遗漏也挡 `rm -rf /`。
    #[cfg(target_os = "macos")]
    pub fn seatbelt_profile(&self, workspace: &Path) -> String {
        let mut allow_write: Vec<String> = Vec::new();
        // workspace(规范化,展开符号链接)。
        if let Ok(canon) = workspace.canonicalize() {
            allow_write.push(canon.to_string_lossy().into_owned());
        } else {
            allow_write.push(workspace.to_string_lossy().into_owned());
        }
        // 额外可写目录。
        for d in &self.writable_dirs {
            allow_write.push(d.to_string_lossy().into_owned());
        }
        // 元数据保护根(workspace + 额外目录的顶层 .git / .zhuye)。
        let meta_roots: Vec<String> = allow_write.clone();
        // 临时目录(macOS tempdir 通常在 /private/var/folders 或 /tmp)。
        allow_write.push("/private/var/folders".to_string());
        allow_write.push("/tmp".to_string());
        allow_write.push("/var/tmp".to_string());

        let write_rules = allow_write
            .iter()
            .map(|p| {
                format!(
                    "(allow file-write* (subpath {}))",
                    serde_json::to_string(p).expect("路径字符串可序列化")
                )
            })
            .collect::<Vec<_>>()
            .join("\n            ");
        let net_rule = if self.allow_network {
            "(allow network*)".to_string()
        } else {
            // 拒出站,放行本机回环与 Unix socket(本地 IPC)。
            // 其余网络操作由顶部 (deny default) 兜底拒绝。
            "(allow network-bind (local ip \"*:*\"))
            (allow network-inbound (local ip \"localhost:*\"))
            (allow network-outbound (remote ip \"localhost:*\"))
            (allow system-socket (socket-domain AF_UNIX))
            (allow network-bind (local unix-socket))
            (allow network-outbound (remote unix-socket))"
                .to_string()
        };
        let net_label = if self.allow_network {
            "允许"
        } else {
            "仅本机(localhost + Unix socket)"
        };

        // subpath 按字面路径匹配,不展开星号;必须为真实用户目录生成规则。
        let mut homes = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            homes.push(home.canonicalize().unwrap_or(home));
        }
        if let Ok(entries) = std::fs::read_dir("/Users") {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    homes.push(entry.path());
                }
            }
        }
        homes.sort();
        homes.dedup();
        let sensitive_reads = homes
            .iter()
            .flat_map(|home| {
                [
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
                ]
                .map(|suffix| {
                    format!(
                        "(deny file-read* (subpath {}))",
                        serde_json::to_string(&home.join(suffix).to_string_lossy())
                            .expect("路径字符串可序列化")
                    )
                })
            })
            .collect::<Vec<_>>()
            .join("\n            ");

        // 元数据只读:workspace 与额外目录顶层的 `.git` / `.agents` / `.aws`
        // / `.zhuye`(与 `zhuye-tools::sandbox` 的 `PROTECTED_WRITE_COMPONENTS`
        // 同口径:改 hook / 改凭据即提权),外加宿主显式要求的受保护路径。
        // deny 置于 allow 之后(Seatbelt 后匹配优先),嵌套 `.git`
        // (submodule)由文件工具层的分量检查兜底。
        //
        // `.zhuye` 的这一条是下面「三明治」的第一步(整树 deny);随后
        // `zhuye_allow` 整树放行(spill / plan / worktrees 等工作数据),
        // 最后 `zhuye_child_denies` 再 deny 提权子项 —— 顺序依赖后匹配优先。
        let mut meta_readonly_paths: Vec<String> = meta_roots
            .iter()
            .flat_map(|root| {
                let root_path = Path::new(root);
                DEFAULT_PROTECTED_ENTRIES
                    .iter()
                    // `.git` 的归属随会话快照:只保护会话开始前已存在的
                    // 仓库(防改 hook 提权),agent 自建的仓库全程可写
                    // —— 无条件 deny 连 `git init` 都不可能,逐命令按存在
                    // 性判又会卡在 init 之后(见 GitProtectionSnapshot)。
                    // `.agents` / `.aws` / `.zhuye` 保持无条件 deny。
                    .filter(|name| **name != ".git" || self.git_is_protected(root_path))
                    .map(move |name| format!("{root}/{name}"))
                    .chain(std::iter::once(format!("{root}/.zhuye")))
            })
            .collect();
        // 宿主显式要求的路径(相对路径按 workspace 解析)。
        for requested in &self.protected_paths {
            let absolute = if requested.is_absolute() {
                requested.clone()
            } else {
                workspace.join(requested)
            };
            meta_readonly_paths.push(absolute.to_string_lossy().into_owned());
        }
        let meta_readonly = meta_readonly_paths
            .iter()
            .map(|path| {
                format!(
                    "(deny file-write* (subpath {}))",
                    serde_json::to_string(path).expect("路径字符串可序列化")
                )
            })
            .collect::<Vec<_>>()
            .join("\n            ");

        // `.zhuye` 三明治:deny 整树 → allow 整树 → deny 提权子项。
        // `.zhuye` 下大部分子树是引擎托管的工作数据(spill / plan /
        // worktrees / uploads…),枚举豁免已被 CI 证伪(A23 / plan / spill
        // 三连误伤),改为整树放行 + 显式 deny 提权子项。规则顺序依赖
        // Seatbelt 后匹配优先:最终命中 = 最后一条匹配规则。
        // 与 zhuye-tools::sandbox 的 PROTECTED_ZHUYE_CHILDREN 同口径
        // (两处手工同步 —— 文件工具的精细拒绝在工具层,本表兜底 bash
        // 重定向这类不经文件工具的写入)。
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
        let zhuye_allow = meta_roots
            .iter()
            .map(|root| {
                format!(
                    "(allow file-write* (subpath {}))",
                    serde_json::to_string(&format!("{root}/.zhuye")).expect("路径字符串可序列化")
                )
            })
            .collect::<Vec<_>>()
            .join("\n            ");
        let zhuye_child_denies = meta_roots
            .iter()
            .flat_map(|root| {
                PROTECTED_ZHUYE_CHILDREN
                    .iter()
                    .map(move |child| format!("{root}/.zhuye/{child}"))
            })
            .map(|path| {
                format!(
                    "(deny file-write* (subpath {}))",
                    serde_json::to_string(&path).expect("路径字符串可序列化")
                )
            })
            .collect::<Vec<_>>()
            .join("\n            ");

        format!(
            "(version 1)
            (deny default)
            ;; 读:全盘放行(编译/分析需读系统头与依赖);v1.3 起对凭据 / SSH /
            ;; 钥匙串 / .aws / .gnupg 等敏感路径显式 deny(Seatbelt 中
            ;; deny 永远优先于 allow)。其他读路径全放行,保持可用性。
            (allow file-read*)
            ;; v1.3:敏感读兜底 —— 拒绝读取凭据 / SSH / 钥匙串。
            {sensitive_reads}
            ;; 写:仅 workspace + 额外目录 + 临时目录
            {write_rules}
            ;; /dev/null 放行:shell 里 `cmd 2>/dev/null` 是高频写法,重定向
            ;; 失败时 sh 会直接中止该命令(静默跳过执行),必须显式允许。
            (allow file-write* (path \"/dev/null\"))
            ;; 网络:{net_label}
            {net_rule}
            ;; 进程:允许 spawn/fork/exec,不限目录(子进程继承本沙箱;
            ;; Xcode 的 git / 工作区构建产物不在固定 bin 目录里)。
            (allow process-fork)
            (allow process-exec)
            ;; 信号 / 系统调用基础
            (allow sysctl-read)
            (allow process-info* (target self))
            ;; 元数据只读:顶层 .git / .zhuye(改 hook 与配置是提权面)。
            {meta_readonly}
            ;; .zhuye 工作数据整树放行(spill / plan / worktrees /
            ;; uploads 等),随后显式 deny 提权子项(后匹配优先)。
            {zhuye_allow}
            {zhuye_child_denies}
            ;; 显式兜底:系统关键目录写一律拒(防 rm -rf / 即便上面有遗漏)
            (deny file-write* (subpath \"/usr\"))
            (deny file-write* (subpath \"/bin\"))
            (deny file-write* (subpath \"/sbin\"))
            (deny file-write* (subpath \"/System\"))
            (deny file-write* (subpath \"/Library\"))",
        )
    }

    /// macOS:把命令包成 `sandbox-exec -p <profile> -- sh -c <cmd>` 的 argv。
    /// 返回 `(program, args)` 供 BashTool 用 `Command::new(program).args(args)`。
    /// profile 较长时 `-p` 内联可能有 shell 转义问题,故优先写临时文件用
    /// `-f <file>`;写失败则回退 `-p`(仍可用,只是 argv 长)。
    #[cfg(target_os = "macos")]
    pub fn seatbelt_argv(
        &self,
        workspace: &Path,
        shell_cmd: &str,
    ) -> std::io::Result<(String, Vec<String>)> {
        let profile = self.seatbelt_profile(workspace);
        // 写临时 .sb 文件(sandbox-exec -f 比 -p 更稳:profile 不进 argv,
        // 不受 ARG_MAX / 转义影响)。
        // 文件名必须**每次调用唯一**:同一进程内并发调用(如并行工具执行、
        // 并行测试)若共用 `zhuye-sandbox-<pid>.sb`,fs::write 截断重写的
        // 窗口内另一线程的 sandbox-exec 会读到半截 profile → 编译失败,
        // 子进程根本不启动(表现为 stdout 空);或读到他人完整 profile,
        // workspace 写白名单错乱。pid + 原子自增序号保证互不踩踏。
        let seq = SEATBELT_PROFILE_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp =
            std::env::temp_dir().join(format!("zhuye-sandbox-{}-{seq}.sb", std::process::id()));
        // review 修复:文件名可预测且 `fs::write` 跟随 symlink —— 同机低权
        // 用户可预置 symlink 让我们覆写任意文件,或抢先把 profile 换成宽松版
        // 弱化沙箱。改用 `OpenOptions::create_new` + 0600:已存在即失败、
        // 不会被 symlink 重定向,且只有创建者可读。
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(profile.as_bytes())?;
        Ok((
            "/usr/bin/sandbox-exec".to_string(),
            vec![
                "-f".to_string(),
                tmp.to_string_lossy().into_owned(),
                "--".to_string(),
                "sh".to_string(),
                "-c".to_string(),
                shell_cmd.to_string(),
            ],
        ))
    }

    /// 非 macOS 平台的占位实现:Seatbelt 是 macOS 专属后端,`status()` 在
    /// 其他平台不会返回 `Seatbelt`,调用方按 status 运行时分派,此实现
    /// 运行时不可达,仅为满足跨平台编译(函数体用到 `std::os::unix`,
    /// Windows 上无法编译)。
    #[cfg(not(target_os = "macos"))]
    pub fn seatbelt_argv(
        &self,
        _workspace: &Path,
        _shell_cmd: &str,
    ) -> std::io::Result<(String, Vec<String>)> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Seatbelt 沙箱仅在 macOS 上可用",
        ))
    }

    // ── Linux Landlock ──────────────────────────────────────────────

    /// Linux:返回一个 `unsafe` 的 `pre_exec` 闭包,在 fork 后、exec 前应用
    /// Landlock 规则。workspace 与额外可写目录可写，其他路径默认拒绝写；
    /// 断网时还安装 seccomp,失败即拒绝启动。
    ///
    /// Linux 内核不支持或规则安装失败时返回错误，不受 strict 开关影响。
    ///
    /// # Safety
    /// 仅在 `pre_exec`(子进程 fork 后)上下文调用;闭包内只做 async-signal-
    /// safe 的 libc 调用(landlock、seccomp 系统调用等)。`unsafe` 因
    /// `pre_exec` 要求 `Send + Sync` 的 `FnMut` 且内部用 raw syscall。
    #[cfg(target_os = "linux")]
    pub fn landlock_pre_exec(
        &self,
        workspace: PathBuf,
    ) -> impl FnMut() -> std::io::Result<()> + Send + Sync + 'static {
        let paths = landlock::prepare_paths(&workspace, &self.writable_dirs);
        let restrict_network = !self.allow_network;
        // 只要请求了 Linux 沙箱，任何文件规则失败都不能降级。
        let filter = network_filter::build();
        move || {
            let paths = paths
                .as_ref()
                .map_err(|errno| std::io::Error::from_raw_os_error(*errno))?;
            landlock::apply_prepared(paths, true)?;
            if restrict_network {
                network_filter::install(&filter)?;
            }
            Ok(())
        }
    }
}

/// 从 env 读额外可写目录(`ZHUYE_SANDBOX_WRITABLE`,`:`分隔)。
fn extra_writable_from_env() -> Vec<PathBuf> {
    std::env::var("ZHUYE_SANDBOX_WRITABLE")
        .ok()
        .map_or_else(Vec::new, |s| {
            s.split(':')
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .collect()
        })
}

// ── Linux Landlock 实现 ─────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod landlock {
    use std::ffi::{CStr, CString};
    use std::io;
    use std::os::raw::{c_char, c_int};
    use std::path::{Path, PathBuf};

    // Landlock syscall 号(x86_64 / aarch64 一致)。
    const SYS_LANDLOCK_CREATE_RULESET: i64 = 444;
    const SYS_LANDLOCK_ADD_RULE: i64 = 445;
    const SYS_LANDLOCK_RESTRICT_SELF: i64 = 446;

    const LANDLOCK_RULE_PATH_BENEATH: i32 = 1;
    // Landlock ABI v1 的 access right 位定义(`linux/landlock.h`)。
    // 逐个命名,避免再把位集合口算成一个十六进制字面量 —— 此前
    // `0xffe` 就是口算错误的产物(见下方编译期断言)。
    //
    // `pub(super)`:位集合的回归测试(本文件末尾 mod tests)在 Linux 上
    // 直接引用这些常量做断言;私有项会 E0603(macOS 不编译本模块,
    // 本地发现不了,CI ubuntu --all-targets 才暴露)。
    pub(super) const ACCESS_FS_EXECUTE: u64 = 1 << 0;
    pub(super) const ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
    pub(super) const ACCESS_FS_READ_FILE: u64 = 1 << 2;
    pub(super) const ACCESS_FS_READ_DIR: u64 = 1 << 3;
    pub(super) const ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
    pub(super) const ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
    pub(super) const ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
    pub(super) const ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
    pub(super) const ACCESS_FS_MAKE_REG: u64 = 1 << 8;
    pub(super) const ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
    pub(super) const ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
    pub(super) const ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
    pub(super) const ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
    /// ABI v3 起提供，限制 truncate/ftruncate 及打开时截断。
    pub(super) const ACCESS_FS_TRUNCATE: u64 = 1 << 14;

    // handled access 位:只限制**写类**操作(写文件/删/建),执行与读
    // 完全不受限 —— 设计注释「限制写是核心安全边界;读 / exec 不受限」
    // 的直接实现:若 handled 含 EXECUTE / READ_*,则 /usr/bin/bash、
    // 动态链接库(均在规则外)的 execve / mmap 全被拒(CI ubuntu 实测
    // spawn failed: EACCES)。
    //
    // **历史 bug(0.1)**:这里原本写作 `0xffe`,与上面注释列出的位集合
    // 不符 —— `0xffe` 多含了 `READ_FILE`(0x4)/ `READ_DIR`(0x8)(于是
    // workspace 外的读被内核拒,execve 时动态链接器读不到 /usr/lib,
    // 表现为 spawn EACCES),又漏掉了 `MAKE_SYM`(0x1000)(于是 workspace
    // 外的符号链接创建完全不受限)。正确的纯写类位集合是 `0x1ff2`。
    pub(super) const ACCESS_FS_WRITE_ONLY: u64 = ACCESS_FS_WRITE_FILE
        | ACCESS_FS_REMOVE_DIR
        | ACCESS_FS_REMOVE_FILE
        | ACCESS_FS_MAKE_CHAR
        | ACCESS_FS_MAKE_DIR
        | ACCESS_FS_MAKE_REG
        | ACCESS_FS_MAKE_SOCK
        | ACCESS_FS_MAKE_FIFO
        | ACCESS_FS_MAKE_BLOCK
        | ACCESS_FS_MAKE_SYM;
    pub(super) const ACCESS_FS_ENFORCED: u64 = ACCESS_FS_WRITE_ONLY | ACCESS_FS_TRUNCATE;

    // ABI v1 基础位不含读、执行或截断；正式规则另加 ABI v3 的截断位。
    const _: () = assert!(
        ACCESS_FS_WRITE_ONLY == 0x1ff2,
        "ACCESS_FS_WRITE_ONLY 必须是纯写类位集合 0x1ff2"
    );
    const _: () = assert!(
        ACCESS_FS_WRITE_ONLY & ACCESS_FS_EXECUTE == 0,
        "handled 不得含 EXECUTE(否则 /usr/bin/* 无法 execve)"
    );
    const _: () = assert!(
        ACCESS_FS_WRITE_ONLY & ACCESS_FS_READ_FILE == 0,
        "handled 不得含 READ_FILE(否则 workspace 外的读被拒,execve 失败)"
    );
    const _: () = assert!(
        ACCESS_FS_WRITE_ONLY & ACCESS_FS_READ_DIR == 0,
        "handled 不得含 READ_DIR"
    );
    const _: () = assert!(
        ACCESS_FS_ENFORCED == ACCESS_FS_WRITE_ONLY | ACCESS_FS_TRUNCATE,
        "必须覆盖读、写与直接截断"
    );

    #[repr(C)]
    struct LandlockRulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
    }

    #[repr(C)]
    struct LandlockPathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    // Rust 2024 edition 要求 extern 块显式标注 unsafe(CI ubuntu 实测
    // 编译失败点:extern blocks must be unsafe)。
    //
    // 签名一律用 `std::os::raw` 的 c_char / c_int,不写死 i8 / i32:
    // `c_char` 的有无符号性随架构变化(aarch64 linux 为 u8,x86_64
    // 为 i8),写死 i8 会让 `open(CString::as_ptr(), ..)` 在 aarch64
    // 上 E0308;`CString::as_ptr()` 的返回类型就是 `*const c_char`。
    unsafe extern "C" {
        fn syscall(num: i64, ...) -> i64;
        fn open(path: *const c_char, flags: c_int, ...) -> c_int;
        fn close(fd: c_int) -> c_int;
        fn __errno_location() -> *mut c_int;
        fn prctl(option: c_int, ...) -> c_int;
    }

    /// `PR_SET_NO_NEW_PRIVS`(linux/prctl.h)。Landlock 的
    /// `restrict_self` 前置要求:必须先设置 no_new_privs(或持
    /// CAP_SYS_ADMIN),否则返回 EPERM —— 此前缺失该调用,Linux 上
    /// 沙箱从未真正生效(CI ubuntu 实测 spawn failed: EPERM)。
    const PR_SET_NO_NEW_PRIVS: i32 = 38;

    fn errno() -> i32 {
        unsafe { *__errno_location() }
    }

    /// 给 ruleset 加一条 `PATH_BENEATH` 规则(允许访问 `path` 本身及其
    /// 下属文件的 `allowed_access` 右)。
    ///
    /// 路径可以是目录或字符设备；打开或规则安装失败都返回错误。
    pub(super) fn prepare_paths(
        workspace: &Path,
        writable: &[PathBuf],
    ) -> Result<Vec<CString>, i32> {
        use std::os::unix::ffi::OsStrExt;
        std::iter::once(workspace)
            .chain(writable.iter().map(PathBuf::as_path))
            .chain(std::iter::once(Path::new("/dev/null")))
            .map(|path| CString::new(path.as_os_str().as_bytes()).map_err(|_| libc::EINVAL))
            .collect()
    }

    fn add_path_beneath_rule(ruleset_fd: i32, path: &CStr, allowed_access: u64) -> io::Result<()> {
        // Landlock 要求 O_PATH 文件描述符；O_DIRECTORY 不是 O_PATH。
        let parent_fd = unsafe { open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if parent_fd < 0 {
            return Err(io::Error::from_raw_os_error(errno()));
        }
        let pb = LandlockPathBeneathAttr {
            allowed_access,
            parent_fd,
        };
        let result = unsafe {
            syscall(
                SYS_LANDLOCK_ADD_RULE,
                ruleset_fd as i64,
                LANDLOCK_RULE_PATH_BENEATH as i64,
                &pb as *const _,
                0,
            )
        };
        let error = (result < 0).then(|| io::Error::from_raw_os_error(errno()));
        unsafe { close(parent_fd) };
        match error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// 无副作用地查询内核 Landlock ABI。ABI v3 才能限制直接截断。
    fn kernel_abi() -> io::Result<i64> {
        const LANDLOCK_CREATE_RULESET_VERSION: i64 = 1;
        let abi = unsafe {
            syscall(
                SYS_LANDLOCK_CREATE_RULESET,
                std::ptr::null::<u8>(),
                0,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };
        if abi < 0 {
            Err(io::Error::from_raw_os_error(errno()))
        } else {
            Ok(abi)
        }
    }

    /// 探测不修改调用者进程；实际安装仍可能失败。
    pub(super) fn kernel_supports_landlock() -> bool {
        matches!(kernel_abi(), Ok(3..))
    }

    fn may_ignore_unsupported(errno: i32, strict: bool) -> bool {
        !strict && matches!(errno, 38 | 95)
    }

    /// 在 pre_exec 上下文应用 Landlock。仅 workspace 与额外可写目录
    /// 放行写类访问，其余路径默认拒绝写。
    pub(super) fn apply_landlock(
        workspace: &Path,
        writable: &[PathBuf],
        strict: bool,
    ) -> io::Result<()> {
        let paths = prepare_paths(workspace, writable).map_err(io::Error::from_raw_os_error)?;
        apply_prepared(&paths, strict)
    }

    pub(super) fn apply_prepared(paths: &[CString], strict: bool) -> io::Result<()> {
        match kernel_abi() {
            Ok(3..) => {}
            Ok(_) if !strict => return Ok(()),
            Ok(_) => return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP)),
            Err(error) if may_ignore_unsupported(error.raw_os_error().unwrap_or(0), strict) => {
                return Ok(());
            }
            Err(error) => return Err(error),
        }
        // 1. create ruleset(仅 handled_access_fs;net 留 0)。
        let attr = LandlockRulesetAttr {
            handled_access_fs: ACCESS_FS_ENFORCED,
            handled_access_net: 0,
        };
        let fd = unsafe {
            syscall(
                SYS_LANDLOCK_CREATE_RULESET,
                &attr as *const _,
                std::mem::size_of::<LandlockRulesetAttr>(),
                0,
            )
        };
        if fd < 0 {
            let e = errno();
            // 仅非严格模式可在内核不支持时降级。
            if may_ignore_unsupported(e, strict) {
                return Ok(());
            }
            // fork 后不得格式化分配错误信息，直接返回内核错误码。
            return Err(io::Error::from_raw_os_error(e));
        }
        let ruleset_fd = fd as i32;

        for dir in &paths[..paths.len() - 1] {
            if let Err(error) = add_path_beneath_rule(ruleset_fd, dir, ACCESS_FS_ENFORCED) {
                unsafe { close(ruleset_fd) };
                return Err(error);
            }
        }

        // 2a. /dev/null:shell 里 `cmd 2>/dev/null` / `cmd >/dev/null` 是高频
        // 写法,重定向失败时 sh 会**中止整条命令**(静默跳过执行),必须显式
        // 允许。与 macOS Seatbelt profile 的
        // `(allow file-write* (path "/dev/null"))` 对齐 —— 此前只有 Seatbelt
        // 放行,Landlock 漏了,导致 Linux 上 `2>/dev/null` 全部失败
        // (CI 实证:sh 报 "cannot create /dev/null: Permission denied",
        // bash 内建工具的持久 cwd 恢复因此被误判成"目录已不存在")。
        //
        // /dev/null 是字符设备(非目录),规则只给文件级写权限(WRITE_FILE);
        // MAKE_* / REMOVE_* 是目录语义,对文件规则无意义。
        if let Err(error) = add_path_beneath_rule(
            ruleset_fd,
            &paths[paths.len() - 1],
            ACCESS_FS_WRITE_FILE | ACCESS_FS_TRUNCATE,
        ) {
            unsafe { close(ruleset_fd) };
            return Err(error);
        }

        // 3. 对自身应用限制(restrict self)。
        //
        // 前置:必须先设 no_new_privs(landlock 官方要求),否则
        // restrict_self 返回 EPERM。prctl 被环境拦截时返回错误，不能静默放行。
        let nnp = unsafe { prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
        if nnp != 0 {
            let e = errno();
            let _ = unsafe { close(ruleset_fd) };
            return Err(io::Error::from_raw_os_error(if e == 0 { 1 } else { e }));
        }
        let r = unsafe { syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset_fd as i64, 0) };
        let e = if r < 0 { errno() } else { 0 };
        unsafe { close(ruleset_fd) };
        if r < 0 {
            if may_ignore_unsupported(e, strict) {
                return Ok(()); // 非严格模式允许降级
            }
            return Err(io::Error::from_raw_os_error(e));
        }
        Ok(())
    }

    // 测试模块保持在实现之后:新版 stable clippy 将「test 模块后跟实现项」
    // (items_after_test_module)列为默认警告,-D warnings 下会挂 CI。
    #[cfg(test)]
    mod failure_policy_tests {
        use super::may_ignore_unsupported;

        #[test]
        fn unsupported_backend_only_degrades_when_non_strict() {
            for error in [38, 95] {
                assert!(!may_ignore_unsupported(error, true));
                assert!(may_ignore_unsupported(error, false));
            }
            assert!(!may_ignore_unsupported(1, false));
        }
    }
}

#[cfg(target_os = "linux")]
use landlock::apply_landlock;

/// Linux:内核是否支持 Landlock(非破坏性探测,见
/// `landlock::kernel_supports_landlock`)。常驻进程可安全调用。
#[cfg(target_os = "linux")]
pub fn landlock_kernel_supported() -> bool {
    landlock::kernel_supports_landlock()
}

/// 非 Linux:Seatbelt 路径不在此探针范围,恒视为可用。
#[cfg(not(target_os = "linux"))]
pub fn landlock_kernel_supported() -> bool {
    true
}

/// 环境能力探测(Linux):在**调用进程内**应用 Landlock(写类 handled)
/// 并验证之后仍能 spawn `/bin/true`。
///
/// 用途:embedder 的测试守卫。注意调用后当前进程已被 restrict(landlock
/// domain 由子进程继承)—— 只应在**即将退出的短命子进程**(如测试二进制
/// 的自重跑入口)中调用,不要在常驻 agent 进程里调用。
///
/// 返回 `true` = restrict 成功且 exec 可用(环境健全);`false` = 任一步
/// 失败(内核不支持 / runner 环境异常)。两类失败对 agent 运行时都表现
/// 为「沙箱降级 / spawn 受限」,由调用方决定策略。
#[cfg(target_os = "linux")]
pub fn probe_landlock_exec() -> bool {
    // cwd 作为放行目录(探针语义,与具体 workspace 无关)。
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(_) => return false,
    };
    // 先 spawn 后 restrict:基线可用性。
    if std::process::Command::new("/bin/true").status().is_err() {
        return false;
    }
    // restrict:复用 apply_landlock(失败 = 环境不支持 → 视作不可用)。
    if apply_landlock(&cwd, &[], true).is_err() {
        return false;
    }
    // restrict 成功后 exec 仍须可用(部分受限 runner 上会 EACCES)。
    std::process::Command::new("/bin/true").status().is_ok()
}

#[cfg(not(target_os = "linux"))]
/// 非 Linux 恒可用(Seatbelt 路径不在本探针范围)。
pub fn probe_landlock_exec() -> bool {
    true
}

// ── 向后兼容:旧的 stub 类型别名 ────────────────────────────────────
//
// v0 导出的 `OsSandboxStubStatus` / `wrap_command_stub` 可能在其他 crate
// 被引用。保留别名让旧代码继续编译,内部映射到新 API。
#[doc(hidden)]
pub type OsSandboxStubStatus = OsSandboxStatus;

/// v1.x:解析 `ZHUYE_SANDBOX_OS_LEVEL` 的开关语义(纯函数)。
///
/// 显式 `""` / `"0"` / `"false"` / `"no"` / `"off"` 才关闭;其余值
/// (含 `"1"` / `"true"` / 任意未识别值)视为开启 —— 与 v1.2 "设了就开"
/// 的向后兼容语义一致。
pub fn parse_env_os_level(raw: &str) -> bool {
    !matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

/// v1.x:env > config > 基线 三段优先级(纯函数,不碰进程 env)。
///
/// - env 已设置(`Some`)→ env 说了算(逃生口:运维可临时覆盖配置);
/// - env 未设置 → 尊重 `[sandbox].os_level` 表态(`None` = 未表态);
/// - 两者都未表态 → 基线开启(`OsSandbox::default()` = enabled + strict)。
pub fn resolve_enabled(env_raw: Option<&str>, cfg_os_level: Option<bool>) -> bool {
    if let Some(raw) = env_raw {
        return parse_env_os_level(raw);
    }
    cfg_os_level.unwrap_or(true)
}

#[cfg(test)]
mod resolve_tests {
    use super::*;

    #[test]
    fn parse_env_os_level_only_explicit_off_disables() {
        for off in ["", "0", "false", "no", "off", " off ", "FALSE"] {
            assert!(!parse_env_os_level(off), "{off:?} 应判为关闭");
        }
        for on in ["1", "true", "yes", "on", "anything-else"] {
            assert!(parse_env_os_level(on), "{on:?} 应判为开启");
        }
    }

    #[test]
    fn env_beats_config_beats_baseline() {
        // env 优先:逃生口可临时覆盖配置(关的 env 压掉开的 config,反之亦然)。
        assert!(!resolve_enabled(Some("0"), Some(true)));
        assert!(resolve_enabled(Some("1"), Some(false)));
        // env 未表态 → 尊重 config。
        assert!(resolve_enabled(None, Some(true)));
        assert!(!resolve_enabled(None, Some(false)));
        // 两者都未表态 → 基线开启。
        assert!(resolve_enabled(None, None));
    }

    #[test]
    fn resolve_carries_writable_dirs_and_network() {
        let s = OsSandbox::resolve(Some(true), vec![PathBuf::from("/tmp/extra")], true);
        assert!(s.enabled);
        assert_eq!(s.writable_dirs, vec![PathBuf::from("/tmp/extra")]);
        assert!(s.allow_network);
        // 关闭时沙箱整体透传,writable / network 设置无意义(不受限)。
        let off = OsSandbox::resolve(Some(false), vec![PathBuf::from("/x")], true);
        assert!(!off.enabled);
    }

    #[test]
    fn resolve_respects_env_escape_hatch() {
        // env 显式关闭时,即使 config 要求开也关(纯函数口径)。
        let on = resolve_enabled(Some("0"), Some(true));
        assert!(!on);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_sandbox_never_reports_explicit_paths_as_protected() {
        let workspace = std::env::temp_dir().join("zhuye-disabled-workspace");
        let outside = std::env::temp_dir().join("zhuye-disabled-target");
        let sandbox = OsSandbox::passthrough().with_protected_paths([outside], true);
        assert!(matches!(
            sandbox.unprotected_paths(&workspace)[0].verdict,
            PathProtectionVerdict::Unsupported { .. }
        ));
        assert!(sandbox.enforce_protected_paths(&workspace).is_err());
    }

    /// 敏感路径报告:落在可写根之外的路径本就被默认拒绝(等价只读)。
    #[test]
    fn protected_path_outside_writable_root_is_enforced_by_default() {
        let workspace = std::env::temp_dir().join("zhuye-pp-outside-ws");
        let outside = std::env::temp_dir().join("zhuye-pp-outside-target");
        let sandbox = OsSandbox::enabled(Vec::new()).with_protected_paths([outside], false);
        let report = sandbox.path_protection_report(&workspace);
        let target = report
            .iter()
            .find(|item| item.explicit)
            .expect("显式受保护路径必须在报告里");
        assert_eq!(
            target.verdict,
            PathProtectionVerdict::EnforcedByDefault,
            "{target:?}"
        );
        assert!(sandbox.unprotected_paths(&workspace).is_empty());
    }

    /// 可写根**内**的嵌套路径:macOS Seatbelt 能强制只读,Linux Landlock
    /// 不能(规则按位合并,父级写权会传播)—— 结论必须如实分平台。
    #[test]
    fn nested_protected_path_verdict_is_platform_specific() {
        let workspace = std::env::temp_dir().join("zhuye-pp-nested-ws");
        let nested = workspace.join(".git");
        let sandbox = OsSandbox::enabled(Vec::new()).with_protected_paths([nested.clone()], false);
        let report = sandbox.path_protection_report(&workspace);
        let target = report
            .iter()
            .find(|item| item.explicit)
            .expect("显式受保护路径必须在报告里");
        match sandbox.status() {
            OsSandboxStatus::Seatbelt => {
                assert_eq!(
                    target.verdict,
                    PathProtectionVerdict::Enforced,
                    "{target:?}"
                );
            }
            OsSandboxStatus::Landlock => {
                assert!(
                    matches!(target.verdict, PathProtectionVerdict::Unsupported { .. }),
                    "{target:?}"
                );
                // 默认(lenient)只报告不阻断;宿主强制要求时必须失败关闭。
                assert!(sandbox.enforce_protected_paths(&workspace).is_ok());
                let strict = sandbox.with_protected_paths([nested], true);
                let failure = strict
                    .enforce_protected_paths(&workspace)
                    .expect_err("平台无法强制时必须失败关闭");
                assert!(failure.contains("无法强制"), "{failure}");
            }
            _ => {}
        }
    }

    /// `strict_paths` 关闭时:平台做不到只报告不阻断(不让会话不可用)。
    #[test]
    fn strict_paths_off_reports_without_blocking() {
        let workspace = std::env::temp_dir().join("zhuye-pp-lenient-ws");
        let nested = workspace.join(".git");
        let sandbox = OsSandbox::enabled(Vec::new()).with_protected_paths([nested], false);
        assert!(sandbox.enforce_protected_paths(&workspace).is_ok());
    }

    /// 默认元数据条目(`.git` / `.agents` / `.aws`)总是出现在报告里,
    /// 让宿主看到真实状态(而不是只看到「沙箱已启用」)。无会话快照时
    /// `.git` 恒受保护,与其是否存在无关。
    #[test]
    fn default_entries_appear_in_report() {
        let workspace = std::env::temp_dir().join("zhuye-pp-default-ws");
        let sandbox = OsSandbox::enabled(Vec::new());
        let report = sandbox.path_protection_report(&workspace);
        for name in DEFAULT_PROTECTED_ENTRIES {
            assert!(
                report
                    .iter()
                    .any(|item| { item.resolved.file_name().is_some_and(|value| value == name) }),
                "报告缺少默认条目 {name}: {report:?}"
            );
        }
    }

    /// `.git` 归属随会话快照(报告口径,平台无关):无快照恒保护;快照下
    /// 全新 workspace 不保护,且**不随 `git init` 翻转**(下一条命令就要写
    /// `.git`);下一个会话开始时仓库已存在,恢复保护;快照经 Clone 共享,
    /// 同一会话的各条命令看到同一份判定。
    #[test]
    fn git_protection_follows_session_snapshot_in_report() {
        let workspace = std::env::temp_dir().join("zhuye-pp-git-snapshot-ws");
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::create_dir_all(&workspace);
        let has_git = |sandbox: &OsSandbox| {
            sandbox
                .path_protection_report(&workspace)
                .iter()
                .any(|item| {
                    item.resolved
                        .file_name()
                        .is_some_and(|value| value == ".git")
                })
        };

        // 无会话快照(调用方没有会话上下文):恒保护,与是否存在无关。
        let bare = OsSandbox::enabled(Vec::new());
        assert!(has_git(&bare), "无快照时 .git 必须恒受保护");

        let mut session = OsSandbox::enabled(Vec::new());
        session.git_snapshot = Some(GitProtectionSnapshot::new());
        assert!(
            !has_git(&session),
            "全新 workspace 的 .git 属于 agent,不得报告为受保护"
        );
        let _ = std::fs::create_dir_all(workspace.join(".git"));
        assert!(
            !has_git(&session),
            "快照一旦判定不得随文件系统翻转(init 之后下一条命令就要写 .git)"
        );
        let cloned = session;
        assert!(
            !has_git(&cloned),
            "快照经 Clone 共享,同一会话的各条命令必须看到同一份判定"
        );

        let mut next_session = OsSandbox::enabled(Vec::new());
        next_session.git_snapshot = Some(GitProtectionSnapshot::new());
        assert!(
            has_git(&next_session),
            "下一个会话开始时仓库已存在,必须恢复保护"
        );
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn default_is_enabled_and_strict() {
        // v1.3:OsSandbox::default() 必须 enabled + strict,以满足
        // "默认要求 OS sandbox" 的严格安全基线。
        let s = OsSandbox::default();
        assert!(s.enabled, "default 必须 enabled");
        assert!(s.strict, "default 必须 strict(fail-closed)");
        // status 应反映 backend(Seatbelt / Landlock / NoBackend),
        // 永远不可能是 Disabled(default 已 enabled)。
        assert_ne!(s.status(), OsSandboxStatus::Disabled);
    }

    #[test]
    fn passthrough_helper_disables_sandbox() {
        let s = OsSandbox::passthrough();
        assert!(!s.enabled);
        assert!(!s.strict);
        assert_eq!(s.status(), OsSandboxStatus::Disabled);
        assert!(!s.is_active());
        assert!(s.status_line().contains("未启用"));
    }

    #[test]
    fn from_env_defaults_on_and_strict() {
        // 子进程隔离环境变量,避免与并行测试互相干扰。
        const CHILD: &str = "ZHUYE_TEST_SANDBOX_DEFAULT_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let s = OsSandbox::from_env();
            assert!(s.enabled, "未设置环境变量时必须启用沙箱");
            assert!(s.strict, "未设置环境变量时必须启用严格模式");
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "os_sandbox::tests::from_env_defaults_on_and_strict",
            ])
            .env(CHILD, "1")
            .env_remove("ZHUYE_SANDBOX_OS_LEVEL")
            .env_remove("ZHUYE_SANDBOX_STRICT")
            .status()
            .unwrap();
        assert!(status.success(), "沙箱默认值子进程测试失败");
    }

    #[test]
    fn enforce_or_fail_strict_no_backend_returns_err() {
        // v1.3 核心 fail-closed 不变式:strict + enabled + 后端不可用
        // → 返回 Err;BashTool 据此抛 ToolError::SandboxUnavailable。
        let s = OsSandbox {
            enabled: true,
            writable_dirs: Vec::new(),
            allow_network: false,
            strict: true,
            protected_paths: Vec::new(),
            strict_paths: false,
            git_snapshot: None,
            backend: OsSandboxStatus::NoBackend,
        };
        assert!(s.enforce_or_fail().is_err());
    }

    // ── 0.1 回归:Landlock handled 位集合 ────────────────────────────

    /// 回归(0.1):handled 位集合必须是纯写类 `0x1ff2`。
    ///
    /// 原实现写作 `0xffe`,与自身注释列出的位不符:多含 READ_FILE(0x4) /
    /// READ_DIR(0x8)(→ workspace 外的读被内核拒,execve 时动态链接器读不到
    /// /usr/lib,表现为 spawn EACCES),又漏了 MAKE_SYM(0x1000)(→ workspace
    /// 外的符号链接创建完全不受限)。该常量只在 Linux 编译,故用条件编译
    /// 引用;非 Linux 上退化为对字面量的等价断言。
    #[test]
    #[cfg(target_os = "linux")]
    fn landlock_handled_access_is_write_only() {
        assert_eq!(landlock::ACCESS_FS_WRITE_ONLY, 0x1ff2);
        // 不得含 EXECUTE / READ_*(否则 execve 失败)。
        assert_eq!(
            landlock::ACCESS_FS_WRITE_ONLY & landlock::ACCESS_FS_EXECUTE,
            0
        );
        assert_eq!(
            landlock::ACCESS_FS_WRITE_ONLY & landlock::ACCESS_FS_READ_FILE,
            0
        );
        assert_eq!(
            landlock::ACCESS_FS_WRITE_ONLY & landlock::ACCESS_FS_READ_DIR,
            0
        );
        // 必须含 MAKE_SYM(符号链接创建也要受限)。
        assert_ne!(
            landlock::ACCESS_FS_WRITE_ONLY & landlock::ACCESS_FS_MAKE_SYM,
            0
        );
    }

    /// 网络状态应区分 seccomp 断网、显式联网与未启用沙箱。
    #[test]
    fn status_line_reports_network_honestly_per_backend() {
        // Landlock 配合 seccomp 阻止创建和使用套接字。
        let ll = OsSandbox {
            enabled: true,
            writable_dirs: Vec::new(),
            allow_network: false,
            strict: true,
            protected_paths: Vec::new(),
            strict_paths: false,
            git_snapshot: None,
            backend: OsSandboxStatus::Landlock,
        };
        let line = ll.status_line();
        assert!(line.contains("已请求 Linux Landlock"), "{line}");
        assert!(line.contains("启动成功后生效"), "{line}");
        assert!(line.contains("网络:请求禁止创建和连接套接字"), "{line}");
        assert!(line.contains("敏感读:不受沙箱约束"), "{line}");

        let connected = OsSandbox {
            allow_network: true,
            ..ll.clone()
        };
        assert!(connected.status_line().contains("网络:不受沙箱约束"));

        let disabled = OsSandbox {
            enabled: false,
            ..ll.clone()
        };
        assert!(disabled.status_line().contains("网络:不受沙箱约束"));
        assert!(disabled.status_line().contains("敏感读:不受沙箱约束"));

        let unavailable = OsSandbox {
            backend: OsSandboxStatus::NoBackend,
            ..ll
        };
        assert!(unavailable.status_line().contains("网络:不适用(无法执行)"));
        assert!(
            unavailable
                .status_line()
                .contains("敏感读:不适用(无法执行)")
        );

        // Seatbelt 真拦出站,allow_network = false 时称「仅本机」。
        let sb = OsSandbox {
            enabled: true,
            writable_dirs: Vec::new(),
            allow_network: false,
            strict: true,
            protected_paths: Vec::new(),
            strict_paths: false,
            git_snapshot: None,
            backend: OsSandboxStatus::Seatbelt,
        };
        assert!(sb.status_line().contains("网络:仅本机"));

        // Seatbelt + allow_network = true → 允许。
        let sb_net = OsSandbox {
            allow_network: true,
            ..sb
        };
        assert!(sb_net.status_line().contains("网络:允许"));
    }

    /// 回归(0.1):strict 模式下 `enforce_or_fail` 不得因 Linux 上
    /// `detect_backend()` 的乐观返回值而放过"内核无 Landlock"的环境。
    /// 内核探测不可用时必须 Err(否则 strict 的 fail-closed 是空话)。
    #[test]
    #[cfg(target_os = "linux")]
    fn enforce_or_fail_strict_probes_kernel_support() {
        // 不直接断言 Ok/Err(取决于运行环境的内核),只断言:
        // 若内核探测失败 → 必须 Err。这里通过一个后端为 Landlock 的实例
        // 交叉验证两条路径的一致性。
        let s = OsSandbox {
            enabled: true,
            writable_dirs: Vec::new(),
            allow_network: false,
            strict: true,
            protected_paths: Vec::new(),
            strict_paths: false,
            git_snapshot: None,
            backend: OsSandboxStatus::Landlock,
        };
        let supported = landlock_kernel_supported();
        let result = s.enforce_or_fail();
        assert_eq!(
            result.is_ok(),
            supported,
            "enforce_or_fail 必须与内核探测结果一致(supported={supported})"
        );
        if !supported {
            assert!(result.unwrap_err().contains("Landlock"));
        }
    }

    #[test]
    fn enforce_or_fail_strict_with_backend_returns_ok() {
        // macOS / Linux / Windows(F24 Job Object)上 strict + 有 backend →
        // Ok,BashTool 走 sandbox 路径。
        let mut s = OsSandbox::enabled(Vec::new());
        s.strict = true;
        if matches!(
            s.status(),
            OsSandboxStatus::Seatbelt
                | OsSandboxStatus::Landlock
                | OsSandboxStatus::WindowsJobObject
        ) {
            assert!(s.enforce_or_fail().is_ok());
        } else {
            // 后端不可用(老 macOS / 非 Linux)→ 也应返回 Err。
            assert!(s.enforce_or_fail().is_err());
        }
    }

    #[test]
    fn enforce_or_fail_non_strict_respects_platform_policy() {
        let s = OsSandbox {
            enabled: true,
            writable_dirs: Vec::new(),
            allow_network: false,
            strict: false,
            protected_paths: Vec::new(),
            strict_paths: false,
            git_snapshot: None,
            backend: OsSandboxStatus::NoBackend,
        };
        assert_eq!(s.enforce_or_fail().is_err(), cfg!(target_os = "linux"));
        let s = OsSandbox::passthrough();
        assert!(s.enforce_or_fail().is_ok());
    }

    #[test]
    fn status_line_includes_net_and_sensitive_indicators() {
        // v1.3:状态行必须包含网络策略 + 敏感读边界指示,便于运维审计。
        let s = OsSandbox::default();
        let line = s.status_line();
        assert!(line.contains("网络:"), "status_line 缺网络指示: {line}");
        assert!(line.contains("敏感读:"), "status_line 缺敏感读指示: {line}");
        assert!(
            line.contains("strict="),
            "status_line 缺 strict 标志: {line}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn enabled_detects_seatbelt() {
        // 测试机上 sandbox-exec 通常存在。
        let s = OsSandbox::enabled(Vec::new());
        assert!(matches!(
            s.status(),
            OsSandboxStatus::Seatbelt | OsSandboxStatus::NoBackend
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_profile_denies_system_writes() {
        let s = OsSandbox::enabled(Vec::new());
        let prof = s.seatbelt_profile(Path::new("/Users/x/project"));
        // 关键不变式:profile 必须显式 deny 系统目录写 + deny default。
        assert!(prof.contains("(deny default)"), "profile must deny default");
        assert!(prof.contains("(deny file-write* (subpath \"/usr\"))"));
        assert!(prof.contains("(deny file-write* (subpath \"/bin\"))"));
        assert!(prof.contains("(deny file-write* (subpath \"/System\"))"));
        // workspace 必须在 allow file-write 列表。
        assert!(prof.contains("/Users/x/project"));
        // 网络默认只放本机:localhost / Unix socket 有 allow,无整段
        // allow,也无整段 deny(其余网络由 deny default 兜底)。
        assert!(prof.contains("(allow network-outbound (remote ip \"localhost:*\"))"));
        assert!(prof.contains("(allow network-outbound (remote unix-socket))"));
        assert!(!prof.contains("(allow network*)"));
        assert!(!prof.contains("(deny network*)"));
        // 元数据只读:workspace 顶层 .git / .zhuye 拒写(无会话快照 →
        // `.git` 恒保护;快照语义见 git_deny_rule_follows_session_snapshot)。
        assert!(prof.contains("(deny file-write* (subpath \"/Users/x/project/.git\"))"));
        assert!(prof.contains("(deny file-write* (subpath \"/Users/x/project/.zhuye\"))"));
        // 进程执行不限目录(子进程继承沙箱)。
        assert!(prof.contains("(allow process-exec)\n"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_profile_full_network_and_extra_dirs() {
        let mut s = OsSandbox::enabled(vec![PathBuf::from("/Users/x/extra")]);
        s.allow_network = true;
        let prof = s.seatbelt_profile(Path::new("/Users/x/p"));
        assert!(prof.contains("(allow network*)"));
        // 额外可写目录的顶层 .git 同样拒写(无会话快照 → 恒保护)。
        assert!(prof.contains("(deny file-write* (subpath \"/Users/x/extra/.git\"))"));
    }

    /// `.git` 归属随会话快照(profile 口径 —— Seatbelt 真正消费的 deny
    /// 列表)。无快照恒 deny;快照下全新 workspace 不 deny;快照下会话
    /// 开始前已存在的仓库照常 deny。`.agents` / `.zhuye` 不受快照影响。
    #[cfg(target_os = "macos")]
    #[test]
    fn git_deny_rule_follows_session_snapshot() {
        let workspace = std::env::temp_dir().join("zhuye-pp-git-profile-ws");
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::create_dir_all(&workspace);
        // profile 里的路径经 canonicalize + serde_json 序列化(带引号,
        // 引号同时防前缀误配),断言标记串用同一口径。
        let ws = workspace.canonicalize().unwrap();
        let deny_git = format!("(deny file-write* (subpath \"{}/.git\"))", ws.display());
        let deny_agents = format!("(subpath \"{}/.agents\")", ws.display());

        let bare = OsSandbox::enabled(Vec::new());
        assert!(bare.seatbelt_profile(&workspace).contains(&deny_git));

        let mut session = OsSandbox::enabled(Vec::new());
        session.git_snapshot = Some(GitProtectionSnapshot::new());
        let fresh = session.seatbelt_profile(&workspace);
        assert!(!fresh.contains(&deny_git), "全新 workspace 不得 deny .git");
        assert!(fresh.contains(&deny_agents), ".agents 必须保持无条件 deny");
        // `git init` 之后重建 profile(每条命令都会重建):仍不得 deny。
        let _ = std::fs::create_dir_all(workspace.join(".git"));
        assert!(
            !session.seatbelt_profile(&workspace).contains(&deny_git),
            "init 之后下一条命令就要写 .git,不得因它已存在而翻转"
        );

        let mut next_session = OsSandbox::enabled(Vec::new());
        next_session.git_snapshot = Some(GitProtectionSnapshot::new());
        assert!(
            next_session
                .seatbelt_profile(&workspace)
                .contains(&deny_git),
            "下一个会话开始时仓库已存在,必须恢复 deny"
        );
        let _ = std::fs::remove_dir_all(&workspace);
    }

    /// v1.3 回归:`.zhuye` 三明治 —— 整树放行(spill / plan / worktrees /
    /// uploads 等引擎托管工作数据),提权子项(config / sessions / cron…)
    /// 在其后显式 deny。枚举豁免已被 CI 证伪(A23 / plan / spill 三连误伤)。
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_profile_zhuye_sandwich_allows_working_data_denies_privileged() {
        let s = OsSandbox::enabled(vec![PathBuf::from("/Users/x/extra")]);
        let prof = s.seatbelt_profile(Path::new("/Users/x/p"));
        // workspace 根:deny(.zhuye) → allow(.zhuye) → deny 提权子项,
        // 顺序即语义(Seatbelt 后匹配优先)。
        let deny_zhuye = prof
            .find("(deny file-write* (subpath \"/Users/x/p/.zhuye\"))")
            .expect("缺少 .zhuye 整树 deny");
        let allow_zhuye = prof
            .find("(allow file-write* (subpath \"/Users/x/p/.zhuye\"))")
            .expect("缺少 .zhuye 整树 allow(工作数据)");
        assert!(allow_zhuye > deny_zhuye, "allow 必须在 deny 之后覆盖");
        let deny_sessions = prof
            .find("(deny file-write* (subpath \"/Users/x/p/.zhuye/sessions\"))")
            .expect("缺少提权子项 deny(sessions)");
        assert!(
            deny_sessions > allow_zhuye,
            "子项 deny 必须在整树 allow 之后"
        );
        // 额外目录同样成对出现。
        assert!(prof.contains("(allow file-write* (subpath \"/Users/x/extra/.zhuye\"))"));
        assert!(prof.contains("(deny file-write* (subpath \"/Users/x/extra/.zhuye/sessions\"))"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_profile_allow_network_when_configured() {
        let mut s = OsSandbox::enabled(Vec::new());
        s.allow_network = true;
        let prof = s.seatbelt_profile(Path::new("/Users/x/p"));
        assert!(prof.contains("(allow network*)"));
    }

    /// v1.3:Seatbelt profile 必须显式 deny 敏感读路径(凭据 / SSH /
    /// 钥匙串等)。Seatbelt 中 deny 永远优先 allow。
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_profile_denies_sensitive_reads() {
        let s = OsSandbox::enabled(Vec::new());
        let prof = s.seatbelt_profile(Path::new("/Users/x/project"));
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let home = home.canonicalize().unwrap_or(home);
        for suffix in [".ssh", ".aws", ".gnupg", "Library/Keychains"] {
            let path = serde_json::to_string(&home.join(suffix).to_string_lossy()).unwrap();
            assert!(
                prof.contains(&format!("(deny file-read* (subpath {path}))")),
                "未保护 {suffix}: {prof}"
            );
        }
        assert!(!prof.contains("/Users/*/"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_argv_wraps_with_sandbox_exec() {
        let s = OsSandbox::enabled(Vec::new());
        let (prog, args) = s
            .seatbelt_argv(Path::new("/Users/x/p"), "rm -rf /")
            .unwrap();
        assert_eq!(prog, "/usr/bin/sandbox-exec");
        assert_eq!(args[0], "-f");
        assert!(args[1].ends_with(".sb"), "profile path should be .sb file");
        assert_eq!(args[2], "--");
        assert_eq!(args[3], "sh");
        assert_eq!(args[4], "-c");
        assert_eq!(args[5], "rm -rf /");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_argv_temp_profiles_are_unique_per_call() {
        // 回归:同一进程内两次调用必须生成不同的 .sb 临时文件。旧实现
        // 共用 `zhuye-sandbox-<pid>.sb`,并发调用在 fs::write 截断重写
        // 窗口内互相踩踏(sandbox-exec 读到半截 profile 编译失败,子进程
        // 不启动),曾致 rm_rf_root_is_blocked_in_sandbox 并发跑必挂。
        let s = OsSandbox::enabled(Vec::new());
        let (_, a) = s.seatbelt_argv(Path::new("/tmp"), "true").unwrap();
        let (_, b) = s.seatbelt_argv(Path::new("/tmp"), "true").unwrap();
        assert_ne!(a[1], b[1], "profile temp file must be unique per call");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rm_rf_root_is_blocked_in_sandbox() {
        // gap doc 核心验证用例:sandbox 内 `rm -rf /` 必须被阻止。
        // 直接跑 sandbox-exec(测试机有该 binary),尝试删一个系统路径
        // `/private/var/db/.zhuye-sandbox-test-<rand>` 之外的真实系统路径
        // 较危险;改测「写到 /usr/local/.zhuye-sandbox-test 应被拒」,
        // 等价证明 file-write* deny 生效。
        let s = OsSandbox::enabled(Vec::new());
        let (prog, args) = s
            .seatbelt_argv(Path::new("/tmp"), "touch /usr/local/.zhuye-sandbox-blocked 2>/dev/null; test -f /usr/local/.zhuye-sandbox-blocked && echo WROTE || echo BLOCKED")
            .unwrap();
        let out = std::process::Command::new(&prog)
            .args(&args)
            .output()
            .expect("sandbox-exec must run");
        let stdout = String::from_utf8_lossy(&out.stdout);
        // /usr 在 deny 列表 → 写失败 → 输出 BLOCKED。
        // (若测试以 root 跑,seatbelt 仍生效;若沙箱未起作用会输出 WROTE。)
        assert!(
            stdout.contains("BLOCKED"),
            "write to /usr must be blocked by sandbox; got stdout: {stdout}"
        );
    }

    /// 敏感路径保护真机验证(macOS Seatbelt):可写根内的 `.git` 不可写,
    /// 同一可写根的普通文件照常可写。Linux 上 Landlock 无法表达
    /// 「可写根内撤销嵌套路径写权」(规则按位合并),由
    /// `nested_protected_path_verdict_is_platform_specific` 断言该差异。
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_protects_metadata_paths_inside_writable_root() {
        let workspace =
            std::env::temp_dir().join(format!("zhuye-sandbox-meta-{}", std::process::id()));
        std::fs::create_dir_all(workspace.join(".git")).expect("建工作区");
        let s = OsSandbox::enabled(Vec::new());
        let script = format!(
            "cd {ws} && (echo x > .git/hook.sh) 2>/dev/null; (echo x > plain.txt) 2>/dev/null; \
             test -f .git/hook.sh && echo GIT_WROTE || echo GIT_BLOCKED; \
             test -f plain.txt && echo PLAIN_OK || echo PLAIN_FAILED",
            ws = workspace.display()
        );
        let (prog, args) = s.seatbelt_argv(&workspace, &script).unwrap();
        let out = std::process::Command::new(&prog)
            .args(&args)
            .output()
            .expect("sandbox-exec 必须可运行");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("GIT_BLOCKED"),
            ".git 必须不可写;stdout: {stdout}"
        );
        assert!(
            stdout.contains("PLAIN_OK"),
            "普通文件必须可写;stdout: {stdout}"
        );
        let _ = std::fs::remove_dir_all(&workspace);
    }

    /// 在 Seatbelt 里跑一条命令,返回(是否成功,stdout + stderr)。
    #[cfg(target_os = "macos")]
    fn run_in_seatbelt(sandbox: &OsSandbox, workspace: &Path, script: &str) -> (bool, String) {
        let (prog, args) = sandbox
            .seatbelt_argv(workspace, script)
            .expect("seatbelt argv");
        let out = std::process::Command::new(prog)
            .args(&args)
            .output()
            .expect("sandbox-exec 必须可运行");
        // BashTool 在命令结束后删除临时 profile;测试同样不留。
        let _ = std::fs::remove_file(&args[1]);
        (
            out.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    }

    /// 真机回归(桌面 sess_99d9bf0a):agent 在全新 workspace 里分几条命令
    /// `git init` → `git add` → `git commit`,每条命令各自重建 profile,
    /// 必须全部成功。此前 init 之后下一条命令起 `.git` 变只读,commit
    /// 永远拿不到 index.lock;更早则连 `git init` 都不可能。会话开始前
    /// 已存在的仓库不受影响(对照见下一用例)。
    #[cfg(target_os = "macos")]
    #[test]
    fn agent_can_init_add_and_commit_across_commands_in_fresh_workspace() {
        let workspace =
            std::env::temp_dir().join(format!("zhuye-sandbox-git-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&workspace);
        std::fs::create_dir_all(&workspace).expect("建工作区");
        std::fs::write(workspace.join("README.md"), "hello\n").expect("写文件");
        let mut s = OsSandbox::enabled(Vec::new());
        s.git_snapshot = Some(GitProtectionSnapshot::new());
        let cd = format!("cd '{}' && ", workspace.display());
        let steps = [
            format!("{cd}git init -b main"),
            format!("{cd}git add README.md"),
            format!(
                "{cd}git -c user.name=t -c user.email=t@example.com \
                 -c commit.gpgsign=false commit -m init"
            ),
            format!("{cd}git rev-parse HEAD"),
        ];
        for step in &steps {
            let (ok, output) = run_in_seatbelt(&s, &workspace, step);
            assert!(ok, "沙箱内 git 命令必须成功: {step}\n{output}");
        }
        let _ = std::fs::remove_dir_all(&workspace);
    }

    /// 对照:会话开始前已存在的仓库,即使挂了会话快照,`.git` 仍不可写
    /// (快照只认会话开始前的归属,不因 agent 在会话内动了什么而让步)。
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_keeps_preexisting_git_readonly_with_session_snapshot() {
        let workspace =
            std::env::temp_dir().join(format!("zhuye-sandbox-git-existing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&workspace);
        std::fs::create_dir_all(workspace.join(".git")).expect("建工作区");
        let mut s = OsSandbox::enabled(Vec::new());
        s.git_snapshot = Some(GitProtectionSnapshot::new());
        let script = format!(
            "cd '{ws}' && (echo x > .git/hook.sh) 2>/dev/null; \
             test -f .git/hook.sh && echo GIT_WROTE || echo GIT_BLOCKED",
            ws = workspace.display()
        );
        let (_, output) = run_in_seatbelt(&s, &workspace, &script);
        assert!(
            output.contains("GIT_BLOCKED"),
            "已有仓库的 .git 必须不可写;输出: {output}"
        );
        let _ = std::fs::remove_dir_all(&workspace);
    }

    /// Linux:可写根**外**的写入被内核拒绝(敏感路径落在可写根外时的
    /// 真实强制);可写根**内**的嵌套路径无法强制 —— 报告必须如实标注
    /// `Unsupported`,不得假装保护已生效。
    #[cfg(target_os = "linux")]
    #[test]
    fn landlock_denies_writes_outside_root_and_reports_nested_gap() {
        use std::io;
        use std::os::unix::process::CommandExt;

        const CHILD: &str = "ZHUYE_LANDLOCK_OUTSIDE_TEST_CHILD";
        if let Some(target) = std::env::var_os(CHILD) {
            let target = std::ffi::CString::new(target.to_str().unwrap()).unwrap();
            let result = unsafe {
                libc::open(
                    target.as_ptr(),
                    libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
                    0o644,
                )
            };
            assert!(result < 0, "可写根外的写入必须被拒绝");
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::EACCES)
            );
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("protected.txt");
        let mut sandbox = OsSandbox::enabled(Vec::new());
        sandbox.allow_network = true;
        let mut setup = sandbox.landlock_pre_exec(workspace.path().to_owned());
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .arg("--exact")
            .arg("os_sandbox::tests::landlock_denies_writes_outside_root_and_reports_nested_gap")
            .arg("--nocapture")
            .env(CHILD, &target);
        unsafe {
            child.pre_exec(move || setup());
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "子进程断言失败: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!target.exists(), "可写根外的文件不应被创建");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn landlock_apply_does_not_panic() {
        // Landlock 在测试机内核不支持时应降级(Ok),支持时也应 Ok。
        let ws = std::env::temp_dir();
        let r = apply_landlock(&ws, &[], false);
        // 测试进程已应用 landlock 后可能影响后续测试,故只验证不 panic。
        let _ = r;
    }

    // ── 回归:Landlock 必须放行 /dev/null 写 ────────────────────────────

    /// 子进程入口:`ZHUYE_SANDBOX_DEVNULL_CHILD=1` 时应用 Landlock,
    /// 再尝试以写模式打开 /dev/null —— 成功 exit(0),被拒 exit(3)。
    ///
    /// 必须在**子进程**里做:Landlock 一旦 restrict 就无法撤销,会污染
    /// 整个测试进程(父进程)。
    #[test]
    #[cfg(target_os = "linux")]
    fn landlock_devnull_write_child() {
        if std::env::var("ZHUYE_SANDBOX_DEVNULL_CHILD").as_deref() != Ok("1") {
            return;
        }
        let ws = std::env::temp_dir();
        if apply_landlock(&ws, &[], true).is_err() {
            std::process::exit(2); // 环境不支持 Landlock → 父进程按 skip 处理
        }
        match std::fs::OpenOptions::new().write(true).open("/dev/null") {
            Ok(_) => std::process::exit(0),
            Err(_) => std::process::exit(3),
        }
    }

    /// 回归:Landlock 下 `/dev/null` 必须可写。
    ///
    /// 此前只有 macOS Seatbelt profile 放行 `/dev/null`
    /// (`(allow file-write* (path "/dev/null"))`),Landlock 漏了 ——
    /// Linux 上 shell 的 `cmd 2>/dev/null` 重定向直接失败(sh 报
    /// "cannot create /dev/null: Permission denied" 并**中止整条命令**),
    /// bash 内建工具的持久 cwd 恢复因此被误判成"目录已不存在"
    /// (CI 实证:zhuye-tools 的 `cwd_persists_across_calls` 失败)。
    #[test]
    #[cfg(target_os = "linux")]
    fn landlock_allows_devnull_write() {
        if !landlock_kernel_supported() {
            eprintln!("skip: 内核不支持 Landlock");
            return;
        }
        let exe = std::env::current_exe().expect("current exe");
        let status = std::process::Command::new(exe)
            .args([
                "--exact",
                "os_sandbox::tests::landlock_devnull_write_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("ZHUYE_SANDBOX_DEVNULL_CHILD", "1")
            .status()
            .expect("spawn child");
        // exit(2) = 环境不支持(不算失败);exit(3) = /dev/null 被拒(回归)。
        assert_ne!(status.code(), Some(3), "/dev/null 写必须被 Landlock 放行");
    }

    #[test]
    fn detect_backend_returns_valid_variant() {
        let b = detect_backend();
        // F24 起 Windows 具备 Job Object 后端,合法变体随之扩充。
        assert!(matches!(
            b,
            OsSandboxStatus::Seatbelt
                | OsSandboxStatus::Landlock
                | OsSandboxStatus::WindowsJobObject
                | OsSandboxStatus::NoBackend
        ));
    }
}
