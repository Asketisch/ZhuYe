//! `ask_mode` —— 询问档（`PermissionMode::Prompt`）的工具判定共享逻辑。
//!
//! 询问档语义（桌面「询问」权限档，v1.x 语义升级）：
//! - 只读工具 + **Safe 级 bash** → 自动放行（不再逐工具弹审批）；
//! - **写类工具**（write / edit / delete_file / notebook_edit / ast 改写）
//!   → 直接拒绝：「询问模式下不能修改文件」；
//! - **Risky / Dangerous bash** → 直接拒绝（拦截删除等危险操作，reason
//!   带命令便于观测）；
//! - 其余未知工具（MCP / 自定义等）→ [`AskDecision::FallThrough`]，
//!   落回审批层逐个询问 —— 保留「询问」的兜底语义。
//!
//! 消费方：
//! - `zhuye-tools` 审批门（`ApprovalGate`）在会话模式为 Prompt 时据此
//!   决定 放行 / 拒绝 / 落 modal；
//! - 本模块不注册 hook（判定结果由审批门消费），与 `PlanModeGate` 的
//!   hook 形态不同。
//!
//! 只读白名单复用 [`super::plan_mode_gate::default_read_only_allowlist`]
//! （单一事实源，避免两处漂移）。

use crate::bash_classify::{BashCommandClass, classify_command};

/// 询问档对一次工具调用的判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskDecision {
    /// 自动放行（审批门据此免弹窗批准）。
    Allow,
    /// 直接拒绝（reason 面向模型，说明档位边界）。
    Deny(String),
    /// 不表态：落回审批层逐个询问（MCP / 自定义工具等）。
    FallThrough,
}

/// 写类工具（询问档硬边界：不能修改文件）。
const ASK_MODE_WRITE_TOOLS: [&str; 4] = ["write", "edit", "delete_file", "notebook_edit"];

/// 判定询问档下一次工具调用。
///
/// `tool` 为工具名，`args` 为原始参数（bash 分级需要读 `cmd` /
/// `command` / `cmdline` 别名；ast 分级需要读 `action`）。
pub fn classify_ask_mode_tool(tool: &str, args: &serde_json::Value) -> AskDecision {
    // 只读白名单（与 PlanModeGate 共用同一份清单）。
    if super::plan_mode_gate::default_read_only_allowlist()
        .iter()
        .any(|t| t == tool)
    {
        return AskDecision::Allow;
    }
    // bash 按命令分级：Safe 放行，Risky/Dangerous 拦截（含删除等危险操作）。
    if tool == "bash" {
        let cmd = zhuye_protocol::resolve_bash_command(args).unwrap_or("");
        return match classify_command(cmd) {
            BashCommandClass::Safe => AskDecision::Allow,
            class => AskDecision::Deny(format!("询问模式下拦截危险操作({:?} 级别): {cmd}", class)),
        };
    }
    // ast per-action 路由：只读动作放行，改写动作属于修改文件。
    if tool == "ast" {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        return match action {
            "list_languages" | "search" => AskDecision::Allow,
            _ => AskDecision::Deny("询问模式下不能修改文件（ast 改写被拦截）".into()),
        };
    }
    // 写类工具：询问档硬边界。
    if ASK_MODE_WRITE_TOOLS.iter().any(|t| *t == tool) {
        return AskDecision::Deny("询问模式下不能修改文件".into());
    }
    // 其余（MCP / 自定义 / 未知）：落回审批层逐个询问。
    AskDecision::FallThrough
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decide(tool: &str, args: serde_json::Value) -> AskDecision {
        classify_ask_mode_tool(tool, &args)
    }

    /// 只读白名单工具放行。
    #[test]
    fn read_only_tools_are_allowed() {
        for tool in [
            "read",
            "grep",
            "glob",
            "echo",
            "web_search",
            "web_fetch",
            "ask_user",
        ] {
            assert_eq!(decide(tool, json!({})), AskDecision::Allow, "{tool} 应放行");
        }
    }

    /// Safe bash 放行；Risky/Dangerous bash 拦截且 reason 带命令。
    #[test]
    fn bash_is_classified_by_command() {
        assert_eq!(
            decide("bash", json!({"command": "git status"})),
            AskDecision::Allow
        );
        assert_eq!(
            decide("bash", json!({"command": "ls -la"})),
            AskDecision::Allow
        );
        let deny = decide("bash", json!({"command": "rm -rf /tmp/x"}));
        let AskDecision::Deny(reason) = deny else {
            panic!("rm 应被拦截");
        };
        assert!(
            reason.contains("rm -rf /tmp/x"),
            "reason 应带命令: {reason}"
        );
        assert!(reason.contains("询问模式"), "reason 应说明档位: {reason}");
    }

    /// 写类工具拒绝；ast 只读动作放行、改写动作拒绝。
    #[test]
    fn write_tools_are_denied() {
        for tool in ["write", "edit", "delete_file", "notebook_edit"] {
            let d = decide(tool, json!({"path": "/tmp/a"}));
            let AskDecision::Deny(reason) = d else {
                panic!("{tool} 应被拒绝");
            };
            assert!(
                reason.contains("不能修改文件"),
                "{tool} reason 应说明边界: {reason}"
            );
        }
        assert_eq!(
            decide("ast", json!({"action": "list_languages"})),
            AskDecision::Allow
        );
        assert!(matches!(
            decide("ast", json!({"action": "replace"})),
            AskDecision::Deny(_)
        ));
    }

    /// 未知工具（MCP 等）不表态，落回审批层。
    #[test]
    fn unknown_tools_fall_through() {
        assert_eq!(
            decide("mcp__demo__query", json!({})),
            AskDecision::FallThrough
        );
        assert_eq!(
            decide("call_researcher", json!({})),
            AskDecision::FallThrough
        );
    }
}
