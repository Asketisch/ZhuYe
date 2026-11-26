//! # F64 引擎杂项 B —— sudo 凭据、通知投递与系统权限引导
//!
//! - [`SudoCredential`] —— sudo 密码独立管理(不进普通凭据池)；
//! - [`NotificationDelivery`] —— 通知投递栈(多通道 + 去重)；
//! - [`PermissionPrompt`] —— macOS 系统权限申请引导。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
