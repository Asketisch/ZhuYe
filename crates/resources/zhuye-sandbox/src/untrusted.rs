//! `untrusted` — v1.9 S2:不可信来源强制隔离闸门。
//!
//! 核心不变式(**与 openhuman 对齐,模块文档即规则**):
//!
//! > **模型/流程写的命令,永不因"运维关掉沙箱"而裸跑。**
//!
//! ZhuYe 的 `OsSandbox` 是全局开关:运维设 `ZHUYE_SANDBOX_OS_LEVEL=0`
//! 或 `[sandbox].os_level = false` 后,所有命令裸跑。这对用户亲笔的
//! 命令(终端里人敲的)是合理授权,但对**模型经工具调用产出的命令**
//! 是逃逸口 —— 提示注入后的模型可以借"关沙箱"的运维开关任意写盘。
//!
//! ## 规则
//!
//! 1. 每次 shell spawn 都带一个 [`CommandOrigin`]:
//!    - [`CommandOrigin::UserAuthored`] —— 用户亲笔(可信);
//!    - 其余([`CommandOrigin::ModelAuthored`] / [`CommandOrigin::StoredText`]
//!      / [`CommandOrigin::External`])—— 不可信。
//!    注:调度任务里存的命令文本,按**创建者**定 origin —— 用户亲手
//!    写的 schedule 条目是 `UserAuthored`;模型经工具创建的是
//!    `ModelAuthored`。审批通过不改变 authorship:批准的是"允许执行",
//!    不改变"这条文本是谁写的"。
//! 2. 可信来源 → 维持全局开关语义(旧行为,`TrustedPassthrough`)。
//! 3. 不可信来源:
//!    - 沙箱后端可用 → **强制沙箱执行**(`ForceSandbox`),全局
//!      关闭开关对它无效;
//!    - 后端不可用 → **拒绝执行**(`Reject`),绝不裸跑(fail-closed)。
//! 4. 唯一逃生口:`ZHUYE_UNTRUSTED_ALLOW_BARE=1` —— 显式点名"允许
//!    不可信命令裸跑"的独立开关,与通用沙箱开关分离且可审计
//!    (`BareByOverride`)。它**不是** `ZHUYE_SANDBOX_OS_LEVEL=0` 的
//!    副作用。
//!
//! ## 与 `os_sandbox` 的分工
//!
//! `os_sandbox` 回答"怎么包"(Seatbelt / Landlock / profile);
//! 本模块回答"**允不允许不包**"。BashTool 在包命令之前先问闸门。

use crate::os_sandbox::{OsSandboxStatus, detect_backend};

/// shell spawn 文本的来源(authorship,不是审批状态)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOrigin {
    /// 用户亲笔敲的命令(可信)。
    UserAuthored,
    /// 模型经工具调用产出(不可信 —— 即使模型看起来"只是转述"用户)。
    ModelAuthored,
    /// 早前存进调度 / 配置的文本(不可信:写入口可能被模型污染)。
    StoredText,
    /// 外部内容(MCP 输出 / 网页 / 文件)程序化生成(不可信)。
    External,
}

impl CommandOrigin {
    /// 只有用户亲笔可信。审批通过不改变 authorship。
    pub fn is_trusted(self) -> bool {
        matches!(self, CommandOrigin::UserAuthored)
    }
}

/// 闸门裁决。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// 可信来源:维持全局开关语义(沙箱启用则包,关闭则允许裸跑)。
    TrustedPassthrough,
    /// 不可信来源 + 后端可用:**必须**沙箱执行,全局关闭开关无效。
    ForceSandbox,
    /// 不可信来源 + 后端不可用:拒绝执行(fail-closed)。
    Reject { reason: String },
    /// `ZHUYE_UNTRUSTED_ALLOW_BARE=1` 显式逃生口被打开:允许裸跑,
    /// 但调用方应把该决定写进审计/日志 —— 这是显式的危险豁免。
    BareByOverride,
}

/// 不可信来源闸门。
///
/// 构造时读一次 `ZHUYE_UNTRUSTED_ALLOW_BARE`;`decide` 是纯函数,
/// 可并发调用。
pub struct UntrustedGate {
    allow_bare_override: bool,
}

impl Default for UntrustedGate {
    fn default() -> Self {
        Self::from_env()
    }
}

impl UntrustedGate {
    /// 从 env 构造:`ZHUYE_UNTRUSTED_ALLOW_BARE=1` 打开显式逃生口。
    pub fn from_env() -> Self {
        let allow = std::env::var("ZHUYE_UNTRUSTED_ALLOW_BARE")
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);
        Self::with_allow_bare(allow)
    }

    /// 显式指定逃生口(测试 / embedder 用)。
    pub fn with_allow_bare(allow: bool) -> Self {
        Self {
            allow_bare_override: allow,
        }
    }

    /// 沙箱后端在当前平台是否可用(与 `OsSandbox.enabled` 无关 ——
    /// 闸门问的是"能不能包",不是"全局开没开")。
    pub fn backend_available() -> bool {
        !matches!(detect_backend(), OsSandboxStatus::NoBackend)
    }

    /// 裁决一次 spawn。裁决只依赖两件事:文本来源(`origin`)与
    /// 当前平台后端可用性(`backend_available`,闸门自己探测,不受
    /// 全局 `enabled=false` 影响)。调用方的全局沙箱配置(`OsSandbox`)
    /// 不参与裁决 —— 它只决定 ForceSandbox 之后**怎么包**(那是
    /// `os_sandbox` 的职责);可信任 Passthrough 的后续行为也由调用方
    /// 按既有逻辑处理。
    pub fn decide(&self, origin: CommandOrigin) -> GateDecision {
        if origin.is_trusted() {
            return GateDecision::TrustedPassthrough;
        }
        if self.allow_bare_override {
            return GateDecision::BareByOverride;
        }
        if Self::backend_available() {
            GateDecision::ForceSandbox
        } else {
            GateDecision::Reject {
                reason: format!(
                    "不可信来源命令( origin={origin:?} )在当前平台无沙箱后端可用 \
                     (macOS 需 /usr/bin/sandbox-exec;Linux 需 Landlock 内核支持),\
                     按不可信来源强制隔离规则拒绝裸执行。\
                     这是 fail-closed 设计:即便运维关闭了全局沙箱开关,模型/流程\
                     写的命令也不允许裸跑。如确需豁免,设置 ZHUYE_UNTRUSTED_ALLOW_BARE=1。"
                ),
            }
        }
    }

    /// 状态行(TUI / banner / `/sandbox` 面板)。
    pub fn status_line(&self) -> String {
        if self.allow_bare_override {
            "untrusted-gate: ZHUYE_UNTRUSTED_ALLOW_BARE=1 —— 不可信命令允许裸跑(显式豁免,危险)".into()
        } else if Self::backend_available() {
            "untrusted-gate: 不可信命令强制沙箱(全局沙箱开关对其无效)".into()
        } else {
            "untrusted-gate: 不可信命令拒绝执行(无沙箱后端,fail-closed)".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_passthrough_regardless_of_switch() {
        // 可信来源永远 passthrough —— 裁决只看来源,不看全局开关
        // (开关由调用方在 Passthrough 之后按既有逻辑处理)。
        let gate = UntrustedGate::with_allow_bare(false);
        assert_eq!(
            gate.decide(CommandOrigin::UserAuthored),
            GateDecision::TrustedPassthrough
        );
    }

    #[test]
    fn untrusted_forces_sandbox_when_backend_available() {
        // S2:不可信来源 + 后端可用 → ForceSandbox。调用方(bash.rs)
        // 在此基础上把"全局开关已关"的配置重建为启用 —— 即全局开关
        // 对不可信来源无效(集成层由 bash untrusted_* 测试覆盖)。
        let gate = UntrustedGate::with_allow_bare(false);
        if UntrustedGate::backend_available() {
            assert_eq!(
                gate.decide(CommandOrigin::ModelAuthored),
                GateDecision::ForceSandbox
            );
        }
    }

    #[test]
    fn all_untrusted_origins_treated_equal() {
        let gate = UntrustedGate::with_allow_bare(false);
        for origin in [
            CommandOrigin::ModelAuthored,
            CommandOrigin::StoredText,
            CommandOrigin::External,
        ] {
            let d = gate.decide(origin);
            if UntrustedGate::backend_available() {
                assert_eq!(d, GateDecision::ForceSandbox, "{origin:?} 应强制沙箱");
            } else {
                assert!(matches!(d, GateDecision::Reject { .. }), "{origin:?} 应拒绝");
            }
        }
    }

    #[test]
    fn reject_reason_mentions_escape_hatch() {
        // 无后端平台的拒绝串必须点名逃生口 env,便于运维定位。
        let gate = UntrustedGate::with_allow_bare(false);
        let d = gate.decide(CommandOrigin::External);
        if let GateDecision::Reject { reason } = d {
            assert!(reason.contains("ZHUYE_UNTRUSTED_ALLOW_BARE"));
        }
        // 有后端平台不产生 Reject(本测试在无后端平台才有断言意义,
        // 有后端时上面 if 不进入,测试仍通过)。
    }

    #[test]
    fn explicit_override_allows_bare() {
        let gate = UntrustedGate::with_allow_bare(true);
        assert_eq!(
            gate.decide(CommandOrigin::ModelAuthored),
            GateDecision::BareByOverride
        );
        // 逃生口只影响不可信来源;可信来源本来就是 passthrough。
        assert_eq!(
            gate.decide(CommandOrigin::UserAuthored),
            GateDecision::TrustedPassthrough
        );
    }

    #[test]
    fn status_line_reflects_mode() {
        let off = UntrustedGate::with_allow_bare(false);
        let on = UntrustedGate::with_allow_bare(true);
        if UntrustedGate::backend_available() {
            assert!(off.status_line().contains("强制沙箱"));
        } else {
            assert!(off.status_line().contains("拒绝执行"));
        }
        assert!(on.status_line().contains("显式豁免"));
    }

    #[test]
    fn is_trusted_only_user() {
        assert!(CommandOrigin::UserAuthored.is_trusted());
        assert!(!CommandOrigin::ModelAuthored.is_trusted());
        assert!(!CommandOrigin::StoredText.is_trusted());
        assert!(!CommandOrigin::External.is_trusted());
    }

    #[test]
    fn gate_combines_with_resolved_sandbox_without_panic() {
        // 烟雾测试:闸门与 OsSandbox::resolve 组合不 panic(各平台);
        // 全局开关关闭时闸门对不可信来源仍给出非 Passthrough 裁决。
        let gate = UntrustedGate::from_env();
        let s = crate::os_sandbox::OsSandbox::resolve(
            Some(false),
            vec![std::path::PathBuf::from("/tmp")],
            false,
        );
        assert!(!s.enabled, "resolve(Some(false)) 应关闭全局沙箱");
        if !CommandOrigin::ModelAuthored.is_trusted() {
            let d = gate.decide(CommandOrigin::ModelAuthored);
            assert_ne!(d, GateDecision::TrustedPassthrough);
        }
    }
}
