//! # 引擎杂项(F63 / F64 / F65)
//!
//! 三组小而实的改进,合成一个模块:
//!
//! ## F63 引擎杂项 A
//!
//! - [`DryRunPlan`] —— `--dry-run` 先看会做什么再真跑;
//! - [`TurnDeadline`] —— 回合截止时间预留(FYI footer);
//! - [`SystemReminderEnvelope`] —— 系统提醒信封(XML 结构化通知);
//! - 迟到的 allow 不执行([`LateApproval`])。
//!
//! ## F64 引擎杂项 B
//!
//! - [`SudoCredential`] —— sudo 密码独立管理(不进普通凭据池);
//! - [`NotificationDelivery`] —— 通知投递栈(多通道 + 去重);
//! - [`PermissionPrompt`] —— macOS 系统权限申请引导。
//!
//! ## F65 工程纪律
//!
//! - [`AdrEntry`] —— 架构决策记录(ADR);
//! - [`UserFacingError`] —— 错误信息人话化;
//! - [`ParityDisclosure`] —— CLI↔GUI 行为差异明示。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ── F63:dery-run ─────────────────────────────────────────────────────────

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

    pub fn add(mut self, action: PlannedAction) -> DryRunPlan {
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

// ── F64:sudo / 通知 / 权限引导 ────────────────────────────────────────────

/// sudo 凭据(独立管理)。
///
/// 与普通 provider 凭据**分开**:sudo 密码不能进 CredentialPool(那个池
/// 会按 provider 轮换,把 sudo 密码发给模型 provider 是灾难)。
#[derive(Clone, PartialEq, Eq)]
pub struct SudoCredential {
    /// 密码(内存中,不落盘)。
    password: String,
    /// 有效期截止(UNIX 秒)。
    valid_until: Option<u64>,
}

impl SudoCredential {
    pub fn new(password: impl Into<String>) -> SudoCredential {
        SudoCredential {
            password: password.into(),
            valid_until: None,
        }
    }

    /// 设置有效期。
    pub fn with_expiry(mut self, until: u64) -> SudoCredential {
        self.valid_until = Some(until);
        self
    }

    /// 在 `now` 时刻是否有效。
    pub fn is_valid_at(&self, now: u64) -> bool {
        match self.valid_until {
            Some(until) => now < until,
            None => true,
        }
    }

    /// 取密码(调用方用于 askpass;不应记日志)。
    pub fn reveal(&self) -> &str {
        &self.password
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.password.is_empty()
    }
}

impl std::fmt::Debug for SudoCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SudoCredential")
            .field("password", &"<redacted>")
            .field("valid_until", &self.valid_until)
            .finish()
    }
}

/// 通知通道。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyChannel {
    /// 系统通知(桌面)。
    System,
    /// 终端(TUI 内)。
    Terminal,
    /// Webhook。
    Webhook,
    /// 声音。
    Sound,
}

/// 通知投递栈。
#[derive(Debug, Default)]
pub struct NotificationDelivery {
    /// 启用的通道。
    enabled: Vec<NotifyChannel>,
    /// 已投递的通知指纹(去重)。
    delivered: BTreeMap<String, u64>,
    /// 去重窗口(秒)。
    dedup_window_secs: u64,
}

impl NotificationDelivery {
    pub fn new(channels: Vec<NotifyChannel>, dedup_window_secs: u64) -> NotificationDelivery {
        NotificationDelivery {
            enabled: channels,
            delivered: BTreeMap::new(),
            dedup_window_secs,
        }
    }

    /// 投递通知。返回实际投递的通道(空 = 被去重或无可投递通道)。
    ///
    /// `fingerprint` 是通知内容的稳定指纹(同内容在窗口内只投一次)。
    pub fn deliver(&mut self, fingerprint: &str, now: u64) -> Vec<NotifyChannel> {
        if let Some(last) = self.delivered.get(fingerprint)
            && now.saturating_sub(*last) < self.dedup_window_secs
        {
            return Vec::new(); // 窗口内重复,丢弃
        }
        self.delivered.insert(fingerprint.to_string(), now);
        // 清理过期去重记录,防无界增长。
        self.delivered
            .retain(|_, at| now.saturating_sub(*at) < self.dedup_window_secs * 4);
        self.enabled.clone()
    }

    /// 已投递通知数。
    pub fn delivered_count(&self) -> usize {
        self.delivered.len()
    }

    /// 是否有某通道。
    pub fn has_channel(&self, channel: NotifyChannel) -> bool {
        self.enabled.contains(&channel)
    }
}

/// 系统权限申请(macOS 引导)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionPrompt {
    /// 权限名(`accessibility` / `screen_recording` / `full_disk_access`)。
    pub permission: String,
    /// 为什么需要(给用户看的理由)。
    pub rationale: String,
    /// 系统设置深链。
    pub settings_url: String,
    /// 是否已授予。
    pub granted: bool,
}

impl PermissionPrompt {
    pub fn new(
        permission: impl Into<String>,
        rationale: impl Into<String>,
        settings_url: impl Into<String>,
    ) -> PermissionPrompt {
        PermissionPrompt {
            permission: permission.into(),
            rationale: rationale.into(),
            settings_url: settings_url.into(),
            granted: false,
        }
    }

    /// 引导文案。
    pub fn guide_text(&self) -> String {
        if self.granted {
            return format!("{} 已授权", self.permission);
        }
        format!(
            "需要「{}」权限:{}\n请在系统设置中开启:{}",
            self.permission, self.rationale, self.settings_url
        )
    }
}

/// macOS 常见权限深链。
pub fn macos_permission_urls() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "accessibility",
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
        ),
        (
            "screen_recording",
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
        ),
        (
            "full_disk_access",
            "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles",
        ),
    ]
}

// ── F65:ADR / 错误人话化 / 行为差异 ──────────────────────────────────────

/// ADR 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdrStatus {
    Proposed,
    Accepted,
    Deprecated,
    Superseded,
}

impl AdrStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AdrStatus::Proposed => "proposed",
            AdrStatus::Accepted => "accepted",
            AdrStatus::Deprecated => "deprecated",
            AdrStatus::Superseded => "superseded",
        }
    }
}

/// 一条架构决策记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdrEntry {
    /// 编号(`"0001"`)。
    pub number: String,
    pub title: String,
    pub status: AdrStatus,
    /// 背景(为什么需要决策)。
    pub context: String,
    /// 决策内容。
    pub decision: String,
    /// 后果(正面 + 负面)。
    #[serde(default)]
    pub consequences: Vec<String>,
    /// 被谁取代。
    #[serde(default)]
    pub superseded_by: Option<String>,
}

impl AdrEntry {
    /// 渲染成 Markdown(标准 ADR 格式)。
    pub fn render(&self) -> String {
        let mut out = format!("# {}. {}\n\n", self.number, self.title);
        out.push_str(&format!("- 状态:{}\n\n", self.status.as_str()));
        out.push_str(&format!("## 背景\n\n{}\n\n", self.context));
        out.push_str(&format!("## 决策\n\n{}\n\n", self.decision));
        if !self.consequences.is_empty() {
            out.push_str("## 后果\n\n");
            for c in &self.consequences {
                out.push_str(&format!("- {c}\n"));
            }
            out.push('\n');
        }
        if let Some(by) = &self.superseded_by {
            out.push_str(&format!("> 已被 ADR-{by} 取代\n"));
        }
        out
    }
}

/// 人话错误。
///
/// 三层:用户看到的(人话)、开发者看的(技术细节)、下一步(可操作建议)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserFacingError {
    /// 用户可读的一句话。
    pub message: String,
    /// 技术细节(折叠显示)。
    #[serde(default)]
    pub technical: Option<String>,
    /// 建议动作。
    #[serde(default)]
    pub next_steps: Vec<String>,
}

impl UserFacingError {
    pub fn new(message: impl Into<String>) -> UserFacingError {
        UserFacingError {
            message: message.into(),
            technical: None,
            next_steps: Vec::new(),
        }
    }

    pub fn with_technical(mut self, detail: impl Into<String>) -> UserFacingError {
        self.technical = Some(detail.into());
        self
    }

    pub fn with_next_step(mut self, step: impl Into<String>) -> UserFacingError {
        self.next_steps.push(step.into());
        self
    }

    /// 渲染(CLI 用)。
    pub fn render(&self) -> String {
        let mut out = format!("错误:{}\n", self.message);
        if let Some(t) = &self.technical {
            out.push_str(&format!("详情:{t}\n"));
        }
        if !self.next_steps.is_empty() {
            out.push_str("可以试试:\n");
            for s in &self.next_steps {
                out.push_str(&format!("  - {s}\n"));
            }
        }
        out
    }

    /// 是否含技术细节(UI 决定是否折叠)。
    pub fn has_technical(&self) -> bool {
        self.technical.is_some()
    }
}

/// CLI↔GUI 行为差异明示。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParityDisclosure {
    /// 功能名。
    pub feature: String,
    /// CLI 行为。
    pub cli_behavior: String,
    /// GUI 行为。
    pub gui_behavior: String,
    /// 差异原因(为什么不能一致)。
    #[serde(default)]
    pub reason: Option<String>,
}

impl ParityDisclosure {
    /// 是否有差异。
    pub fn differs(&self) -> bool {
        self.cli_behavior != self.gui_behavior
    }

    /// 渲染一行说明。
    pub fn render(&self) -> String {
        if !self.differs() {
            return format!("{}:两端行为一致({})", self.feature, self.cli_behavior);
        }
        let reason = self.reason.as_deref().unwrap_or("未说明");
        format!(
            "{}:CLI {} · GUI {}(原因:{})",
            self.feature, self.cli_behavior, self.gui_behavior, reason
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── F63 ──

    #[test]
    fn dry_run_renders_and_flags_denied() {
        let plan = DryRunPlan::new()
            .add(PlannedAction {
                kind: "write".into(),
                target: "src/lib.rs".into(),
                would_be_denied: false,
                note: String::new(),
            })
            .add(PlannedAction {
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

    // ── F64 ──

    #[test]
    fn sudo_credential_validity() {
        let c = SudoCredential::new("hunter2").with_expiry(1000);
        assert!(c.is_valid_at(500));
        assert!(!c.is_valid_at(1000));
        assert_eq!(c.reveal(), "hunter2");
        assert!(!c.is_empty());
    }

    #[test]
    fn sudo_credential_debug_redacts() {
        let c = SudoCredential::new("hunter2");
        let debug = format!("{c:?}");
        assert!(!debug.contains("hunter2"), "Debug 不得泄漏密码:{debug}");
        assert!(debug.contains("redacted"), "{debug}");
    }

    #[test]
    fn notification_dedup() {
        let mut delivery = NotificationDelivery::new(vec![NotifyChannel::System], 60);
        assert_eq!(delivery.deliver("fp1", 100).len(), 1);
        assert!(delivery.deliver("fp1", 110).is_empty(), "窗口内重复应丢弃");
        assert_eq!(delivery.deliver("fp1", 200).len(), 1, "窗口外可再投");
    }

    #[test]
    fn notification_multi_channel() {
        let mut delivery =
            NotificationDelivery::new(vec![NotifyChannel::System, NotifyChannel::Sound], 60);
        let channels = delivery.deliver("fp", 100);
        assert_eq!(channels.len(), 2);
        assert!(delivery.has_channel(NotifyChannel::System));
        assert!(!delivery.has_channel(NotifyChannel::Webhook));
    }

    #[test]
    fn notification_cleanup_bounds_growth() {
        let mut delivery = NotificationDelivery::new(vec![NotifyChannel::Terminal], 10);
        for i in 0..100 {
            delivery.deliver(&format!("fp{i}"), i * 20);
        }
        assert!(
            delivery.delivered_count() <= 5,
            "旧记录应清理,实际 {}",
            delivery.delivered_count()
        );
    }

    #[test]
    fn permission_prompt_guide() {
        let mut p = PermissionPrompt::new(
            "accessibility",
            "需要控制其他应用窗口",
            "x-apple.systempreferences:...",
        );
        let text = p.guide_text();
        assert!(text.contains("accessibility"), "{text}");
        assert!(text.contains("需要控制其他应用窗口"), "{text}");
        assert!(text.contains("系统设置"), "{text}");
        p.granted = true;
        assert!(p.guide_text().contains("已授权"));
    }

    #[test]
    fn macos_urls_present() {
        let urls = macos_permission_urls();
        assert_eq!(urls.len(), 3);
        assert!(
            urls.iter()
                .all(|(_, u)| u.starts_with("x-apple.systempreferences"))
        );
    }

    // ── F65 ──

    #[test]
    fn adr_render() {
        let adr = AdrEntry {
            number: "0001".into(),
            title: "采用 tree-sitter 而非正则做符号编辑".into(),
            status: AdrStatus::Accepted,
            context: "正则改符号会误伤注释".into(),
            decision: "用 tree-sitter".into(),
            consequences: vec!["依赖变重".into(), "准确性大幅提升".into()],
            superseded_by: None,
        };
        let md = adr.render();
        assert!(md.contains("# 0001."), "{md}");
        assert!(md.contains("状态:accepted"), "{md}");
        assert!(md.contains("## 背景"), "{md}");
        assert!(md.contains("## 决策"), "{md}");
        assert!(md.contains("依赖变重"), "{md}");
    }

    #[test]
    fn adr_superseded_note() {
        let adr = AdrEntry {
            number: "0002".into(),
            title: "t".into(),
            status: AdrStatus::Superseded,
            context: "c".into(),
            decision: "d".into(),
            consequences: vec![],
            superseded_by: Some("0003".into()),
        };
        assert!(adr.render().contains("已被 ADR-0003 取代"));
    }

    #[test]
    fn user_facing_error_render() {
        let err = UserFacingError::new("无法连接到模型服务")
            .with_technical("reqwest: connection refused (os error 61)")
            .with_next_step("检查网络连接")
            .with_next_step("确认 API key 有效");
        let rendered = err.render();
        assert!(rendered.contains("无法连接到模型服务"), "{rendered}");
        assert!(rendered.contains("connection refused"), "{rendered}");
        assert!(rendered.contains("检查网络连接"), "{rendered}");
        assert!(rendered.contains("确认 API key"), "{rendered}");
        assert!(err.has_technical());
    }

    #[test]
    fn user_facing_error_minimal() {
        let err = UserFacingError::new("出错了");
        assert!(!err.has_technical());
        assert!(err.render().contains("出错了"));
    }

    #[test]
    fn parity_disclosure_differs() {
        let same = ParityDisclosure {
            feature: "x".into(),
            cli_behavior: "y".into(),
            gui_behavior: "y".into(),
            reason: None,
        };
        assert!(!same.differs());
        assert!(same.render().contains("行为一致"));

        let diff = ParityDisclosure {
            feature: "审批".into(),
            cli_behavior: "键盘 y/n".into(),
            gui_behavior: "点击按钮".into(),
            reason: Some("交互形态不同".into()),
        };
        assert!(diff.differs());
        let rendered = diff.render();
        assert!(rendered.contains("键盘 y/n"), "{rendered}");
        assert!(rendered.contains("点击按钮"), "{rendered}");
        assert!(rendered.contains("交互形态不同"), "{rendered}");
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

    #[test]
    fn adr_serializes() {
        let adr = AdrEntry {
            number: "1".into(),
            title: "t".into(),
            status: AdrStatus::Proposed,
            context: "c".into(),
            decision: "d".into(),
            consequences: vec![],
            superseded_by: None,
        };
        let json = serde_json::to_string(&adr).expect("serialize");
        let back: AdrEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, adr);
    }
}
