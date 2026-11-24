//! `zhuye-sandbox` — OS 沙箱扩展。
//!
//! v1.2 P0-1:`os_sandbox` 从 stub 升级为真实沙箱(macOS Seatbelt +
//! Linux Landlock)。
//! v1.9 S2:`untrusted` 不可信来源强制隔离闸门 —— 模型/流程写的
//! 命令永不因全局沙箱开关关闭而裸跑。

pub mod os_sandbox;
pub mod untrusted;

pub use os_sandbox::{
    DEFAULT_PROTECTED_ENTRIES, GitProtectionSnapshot, OsSandbox, OsSandboxStatus,
    OsSandboxStubStatus, PathProtection, PathProtectionVerdict, detect_backend,
    probe_landlock_exec,
};
pub use untrusted::{CommandOrigin, GateDecision, UntrustedGate};

/// F25:CoW 隔离后端(平台选择 + 差异契约 + 合回策略)。
pub mod cow;
/// F23:网络出口管控(域名白名单 / 默认全禁 / 子域匹配)。
pub mod egress;
/// F24:Windows 沙箱策略层(AppContainer / Job Object / ACL / 进程树)。
pub mod windows;
/// F24:Windows FFI 执行层(spawn / job / kill / ACL 的 Win32 落地;
/// 非 Windows 提供_UNSUPPORTED stub,调用方回退平台原生后端)。
pub mod windows_ffi;
pub use cow::{
    ChangeKind, ChangeSet, CowBackend, CowWorkspace, FileChange, MergeStrategy, diff_hashes,
};
pub use egress::{EgressDecision, EgressPolicy, host_of};
pub use windows::{
    AppContainerCapability, JobLimits, KillPlan, WindowsSandbox, WindowsSandboxConfig,
    WindowsSandboxError, WindowsSandboxPlan,
};
pub use windows_ffi::{
    SpawnError, Spawned, build_command_line, build_env_block, enumerate_process_tree, kill_tree,
    spawn_sandboxed,
};
