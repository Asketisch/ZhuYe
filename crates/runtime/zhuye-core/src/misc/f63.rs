//! # F63 引擎杂项 A —— 干跑预览、回合截止、系统提醒与迟到审批
//!
//! - [`PlannedAction`] / [`DryRunPlan`] —— `--dry-run` 先看会做什么再真跑；
//! - [`TurnDeadline`] —— 回合截止时间预留(FYI footer)；
//! - [`SystemReminderEnvelope`] —— 系统提醒信封(XML 结构化通知)；
//! - 迟到的 allow 不执行([`LateApproval`])。

use serde::{Deserialize, Serialize};

/// 一个将要执行的动作(干跑预览)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedAction {
    /// 动作类型(`write` / `bash` / `network` / `delete`)。
    pub kind: String,
    /// 目标(文件路径 / 命令)。
    pub target: String,
    /// 是否会被拒绝(权限 / 沙箱预判)。
    pub would_be_denied: bool,
    /// 说明。
    #[serde(default)]
    pub note: String,
}

/// 干跑计划。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRunPlan {
    pub actions: Vec<PlannedAction>,
}

impl DryRunPlan {
    pub fn new() -> DryRunPlan {
        DryRunPlan::default()
    }

    pub fn push(mut self, action: PlannedAction) -> DryRunPlan {
        self.actions.push(action);
        self
    }

    /// 会被拒绝的动作。
    pub fn denied(&self) -> Vec<&PlannedAction> {
        self.actions.iter().filter(|a| a.would_be_denied).collect()
    }

    /// 渲染成预览文本。
    pub fn render(&self) -> String {
        if self.actions.is_empty() {
            return "dry-run: 无动作".to_string();
        }
        let mut out = format!("dry-run: 将执行 {} 个动作\n", self.actions.len());
        for a in &self.actions {
            let mark = if a.would_be_denied { "✗" } else { "✓" };
            out.push_str(&format!("  {mark} {} {}\n", a.kind, a.target));
            if !a.note.is_empty() {
                out.push_str(&format!("     {}\n", a.note));
            }
        }
        let denied = self.denied().len();
        if denied > 0 {
            out.push_str(&format!("注意:{denied} 个动作会被拒绝\n"));
        }
        out
    }

    /// 是否可安全执行(无拒绝项)。
    pub fn is_safe(&self) -> bool {
        self.denied().is_empty()
    }
}

/// 回合截止时间。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnDeadline {
    /// 截止时间戳(UNIX 秒)。
    pub deadline_at: u64,
    /// 预留余量(秒)—— 留出时间收尾,避免硬截断。
    pub reserve_secs: u64,
}

impl TurnDeadline {
    pub fn new(deadline_at: u64, reserve_secs: u64) -> TurnDeadline {
        TurnDeadline {
            deadline_at,
            reserve_secs,
        }
    }

    /// 在 `now` 时刻是否应开始收尾。
    pub fn should_wrap_up(&self, now: u64) -> bool {
        now + self.reserve_secs >= self.deadline_at
    }

    /// 是否已超时。
    pub fn is_expired(&self, now: u64) -> bool {
        now >= self.deadline_at
    }

    /// 剩余秒数(含预留)。
    pub fn remaining_secs(&self, now: u64) -> u64 {
        self.deadline_at.saturating_sub(now)
    }

    /// FYI footer 文本(附在输出末尾)。
    pub fn footer(&self, now: u64) -> Option<String> {
        if !self.should_wrap_up(now) {
            return None;
        }
        Some(format!(
            "[回合即将结束:剩余 {} 秒,请收尾]",
            self.remaining_secs(now)
        ))
    }
}

/// 系统提醒信封。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemReminderEnvelope {
    /// 提醒类型(`budget` / `deadline` / `hook` / `safety`)。
    pub kind: String,
    /// 正文。
    pub content: String,
}

impl SystemReminderEnvelope {
    pub fn new(kind: impl Into<String>, content: impl Into<String>) -> SystemReminderEnvelope {
        SystemReminderEnvelope {
            kind: kind.into(),
            content: content.into(),
        }
    }

    /// 渲染成 XML 结构化通知。
    pub fn render(&self) -> String {
        format!(
            "<system-reminder kind=\"{}\">{}</system-reminder>",
            escape_xml(&self.kind),
            escape_xml(&self.content)
        )
    }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 迟到的审批。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LateApproval {
    /// 审批请求发出时间。
    pub requested_at: u64,
    /// 决策到达时间。
    pub decided_at: u64,
    /// 超时阈值(秒)。
    pub ttl_secs: u64,
}

impl LateApproval {
    /// 决策是否已过期(迟到 → 不执行)。
    ///
    /// 这是 fail-closed 的一部分:挂起太久的审批即使被批准,也不该执行 ——
    /// 上下文早已变化。
    pub fn is_late(&self) -> bool {
        self.decided_at.saturating_sub(self.requested_at) > self.ttl_secs
    }

    /// 是否应执行。
    pub fn should_execute(&self) -> bool {
        !self.is_late()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_renders_and_flags_denied() {
        let plan = DryRunPlan::new()
            .push(PlannedAction {
                kind: "write".into(),
                target: "src/lib.rs".into(),
                would_be_denied: false,
                note: String::new(),
            })
            .push(PlannedAction {
                kind: "delete".into(),
                target: "~/.ssh".into(),
                would_be_denied: true,
                note: "自保护路径".into(),
            });
        assert_eq!(plan.denied().len(), 1);
        assert!(!plan.is_safe());
        let rendered = plan.render();
        assert!(rendered.contains("将执行 2 个动作"), "{rendered}");
        assert!(rendered.contains("1 个动作会被拒绝"), "{rendered}");
        assert!(rendered.contains("自保护路径"));
    }

    #[test]
    fn dry_run_empty() {
        let plan = DryRunPlan::new();
        assert!(plan.is_safe());
        assert!(plan.render().contains("无动作"));
    }

    #[test]
    fn turn_deadline_wrap_up() {
        let d = TurnDeadline::new(1000, 60);
        assert!(!d.should_wrap_up(900), "还有 100s,预留 60s 未触发");
        assert!(d.should_wrap_up(940), "剩 60s 应开始收尾");
        assert!(d.should_wrap_up(1000));
        assert!(d.is_expired(1000));
        assert_eq!(d.remaining_secs(900), 100);
    }

    #[test]
    fn turn_deadline_footer() {
        let d = TurnDeadline::new(1000, 60);
        assert!(d.footer(900).is_none(), "未到收尾不出 footer");
        let footer = d.footer(950).expect("footer");
        assert!(footer.contains("剩余 50 秒"), "{footer}");
    }

    #[test]
    fn system_reminder_xml() {
        let r = SystemReminderEnvelope::new("budget", "剩余 token 不足 10%");
        let rendered = r.render();
        assert!(rendered.starts_with("<system-reminder"), "{rendered}");
        assert!(rendered.contains("kind=\"budget\""));
        assert!(rendered.contains("剩余 token 不足"));
    }

    #[test]
    fn system_reminder_escapes_xml() {
        let r = SystemReminderEnvelope::new("hook", "<script>alert(1)</script> & more");
        let rendered = r.render();
        assert!(!rendered.contains("<script>"), "必须转义:{rendered}");
        assert!(rendered.contains("&lt;script&gt;"), "{rendered}");
        assert!(rendered.contains("&amp;"), "{rendered}");
    }

    #[test]
    fn late_approval_detection() {
        let fresh = LateApproval {
            requested_at: 100,
            decided_at: 110,
            ttl_secs: 60,
        };
        assert!(!fresh.is_late());
        assert!(fresh.should_execute());

        let late = LateApproval {
            requested_at: 100,
            decided_at: 200,
            ttl_secs: 60,
        };
        assert!(late.is_late(), "超过 TTL 即迟到");
        assert!(!late.should_execute(), "迟到不执行");
    }

    #[test]
    fn planned_action_serializes() {
        let a = PlannedAction {
            kind: "write".into(),
            target: "x".into(),
            would_be_denied: false,
            note: "n".into(),
        };
        let json = serde_json::to_string(&a).expect("serialize");
        let back: PlannedAction = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, a);
    }
}
