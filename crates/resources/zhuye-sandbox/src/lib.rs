//! `zhuye-sandbox` — OS 沙箱扩展。
//!
//! v1.2 P0-1:`os_sandbox` 从 stub 升级为真实沙箱(macOS Seatbelt +
//! Linux Landlock)。
//! v1.9 S2:`untrusted` 不可信来源强制隔离闸门 —— 模型/流程写的
//! 命令永不因全局沙箱开关关闭而裸跑。

pub mod os_sandbox;
pub mod untrusted;

pub use os_sandbox::{
    OsSandbox, OsSandboxStatus, OsSandboxStubStatus, detect_backend, probe_landlock_exec,
};
pub use untrusted::{CommandOrigin, GateDecision, UntrustedGate};
