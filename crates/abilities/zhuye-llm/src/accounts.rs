//! # OAuth 登录与账号池(F31)
//!
//! 像正常软件一样「登录账号」用订阅额度(Claude Pro / Codex 等),告别
//! 手搞 API key。
//!
//! ## 三块
//!
//! 1. **Device flow**([`DeviceFlowState`])—— 无浏览器环境的登录方式:
//!    用户拿到 code 去网页确认,宿主轮询拿 token。
//! 2. **账号池**([`AccountPool`])—— 多账号轮换:会话亲和(同一会话
//!    尽量用同一账号,保住 prompt cache)+ 配额排名(接近上限的降权)+
//!    自动暂停恢复。
//! 3. **订阅凭据桥**([`SubscriptionCredential`])—— 读 `~/.claude/
//!    .credentials.json` 这类外部工具的凭据,直接复用其订阅。
//!
//! ## 为什么「会话亲和」很重要
//!
//! 多账号轮换最容易忽略的一点:同一会话中途换账号 → prompt cache 全失效
//! (缓存绑定账号),成本立刻翻倍。会话亲和让「同一会话固定用同一账号,
//! 只在它不可用时才切换」。
//!
//! ## 为什么账号要「自动暂停」
//!
//! 订阅额度用尽后继续请求会得到 429。与其每次失败再试下一个,不如**记住**
//! 这个账号已耗尽,在重置时间前跳过它。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 登录方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginMethod {
    /// Device flow(无浏览器)。
    DeviceFlow,
    /// 回环回调(有浏览器)。
    LoopbackCallback,
    /// 直接粘贴 token。
    PasteToken,
}

/// Device flow 状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceFlowState {
    /// 设备码(轮询用)。
    pub device_code: String,
    /// 用户码(给用户看,去网页输入)。
    pub user_code: String,
    /// 验证 URL。
    pub verification_uri: String,
    /// 轮询间隔(秒)。
    pub interval_secs: u64,
    /// 过期时间(UNIX 秒)。
    pub expires_at: u64,
    /// 当前阶段。
    pub stage: DeviceFlowStage,
}

/// Device flow 阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceFlowStage {
    /// 等用户去网页确认。
    AwaitingUser,
    /// 已拿到 token。
    Authorized,
    /// 用户拒绝。
    Denied,
    /// 过期。
    Expired,
}

impl DeviceFlowState {
    /// 构造。
    pub fn new(
        device_code: impl Into<String>,
        user_code: impl Into<String>,
        verification_uri: impl Into<String>,
        interval_secs: u64,
        expires_at: u64,
    ) -> DeviceFlowState {
        DeviceFlowState {
            device_code: device_code.into(),
            user_code: user_code.into(),
            verification_uri: verification_uri.into(),
            interval_secs,
            expires_at,
            stage: DeviceFlowStage::AwaitingUser,
        }
    }

    /// 是否应停止轮询。
    pub fn is_terminal(&self) -> bool {
        !matches!(self.stage, DeviceFlowStage::AwaitingUser)
    }

    /// 在 `now` 时刻是否已过期。
    pub fn is_expired_at(&self, now: u64) -> bool {
        now >= self.expires_at
    }

    /// 给用户的提示文案。
    pub fn user_instruction(&self) -> String {
        format!(
            "请在浏览器打开 {} 并输入代码 {}",
            self.verification_uri, self.user_code
        )
    }

    /// 轮询后更新阶段。
    pub fn advance(&mut self, stage: DeviceFlowStage) {
        self.stage = stage;
    }
}

/// 一个账号。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    /// 账号 id。
    pub id: String,
    /// provider(anthropic / openai / …)。
    pub provider: String,
    /// 显示名(邮箱 / 昵称)。
    pub label: String,
    /// 是否启用。
    pub enabled: bool,
    /// 已用额度占比(0.0–1.0)。
    pub usage_ratio: f64,
    /// 暂停到何时(UNIX 秒;`None` = 未暂停)。
    #[serde(default)]
    pub paused_until: Option<u64>,
    /// 是否为订阅账号(区别于 API key)。
    #[serde(default)]
    pub subscription: bool,
}

impl Account {
    pub fn new(
        id: impl Into<String>,
        provider: impl Into<String>,
        label: impl Into<String>,
    ) -> Account {
        Account {
            id: id.into(),
            provider: provider.into(),
            label: label.into(),
            enabled: true,
            usage_ratio: 0.0,
            paused_until: None,
            subscription: false,
        }
    }

    /// 在 `now` 时刻是否可用。
    pub fn is_available_at(&self, now: u64) -> bool {
        if !self.enabled {
            return false;
        }
        match self.paused_until {
            Some(until) => now >= until,
            None => true,
        }
    }

    /// 剩余额度比例(0.0–1.0)。
    pub fn remaining_ratio(&self) -> f64 {
        (1.0 - self.usage_ratio).clamp(0.0, 1.0)
    }

    /// 暂停(到 `until`)。
    pub fn pause_until(&mut self, until: u64) {
        self.paused_until = Some(until);
    }
}

/// 账号池。
#[derive(Debug, Default)]
pub struct AccountPool {
    accounts: BTreeMap<String, Account>,
    /// 会话 → 账号 id(亲和绑定)。
    affinity: BTreeMap<String, String>,
}

impl AccountPool {
    pub fn new() -> AccountPool {
        AccountPool::default()
    }

    pub fn add(&mut self, account: Account) {
        self.accounts.insert(account.id.clone(), account);
    }

    pub fn get(&self, id: &str) -> Option<&Account> {
        self.accounts.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Account> {
        self.accounts.get_mut(id)
    }

    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    /// 为某会话选账号。
    ///
    /// 顺序:① 会话亲和(若绑定账号仍可用)→ ② 剩余额度最高的可用账号。
    ///
    /// `provider` 过滤:只在该 provider 的账号里选。
    pub fn select_for(&mut self, session_id: &str, provider: &str, now: u64) -> Option<&Account> {
        // ① 亲和
        if let Some(bound) = self.affinity.get(session_id).cloned()
            && let Some(account) = self.accounts.get(&bound)
            && account.provider == provider
            && account.is_available_at(now)
        {
            return self.accounts.get(&bound);
        }
        // ② 剩余额度最高
        let best = self
            .accounts
            .values()
            .filter(|a| a.provider == provider && a.is_available_at(now))
            .max_by(|a, b| {
                a.remaining_ratio()
                    .partial_cmp(&b.remaining_ratio())
                    .unwrap_or(std::cmp::Ordering::Equal)
                    // 同额度时按 id 稳定排序
                    .then_with(|| b.id.cmp(&a.id))
            })
            .map(|a| a.id.clone())?;
        // 建立新亲和
        self.affinity.insert(session_id.to_string(), best.clone());
        self.accounts.get(&best)
    }

    /// 可用账号(按剩余额度降序)。
    pub fn available(&self, provider: &str, now: u64) -> Vec<&Account> {
        let mut out: Vec<&Account> = self
            .accounts
            .values()
            .filter(|a| a.provider == provider && a.is_available_at(now))
            .collect();
        out.sort_by(|a, b| {
            b.remaining_ratio()
                .partial_cmp(&a.remaining_ratio())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        out
    }

    /// 某会话的亲和账号。
    pub fn affinity_of(&self, session_id: &str) -> Option<&str> {
        self.affinity.get(session_id).map(String::as_str)
    }

    /// 解除会话亲和(下次重新选)。
    pub fn unbind(&mut self, session_id: &str) -> bool {
        self.affinity.remove(session_id).is_some()
    }

    /// 把某账号标为耗尽(暂停到 `reset_at`)。
    pub fn exhaust(&mut self, id: &str, reset_at: u64) {
        if let Some(account) = self.accounts.get_mut(id) {
            account.usage_ratio = 1.0;
            account.pause_until(reset_at);
            // 解除所有指向它的亲和 —— 否则会话会反复选中不可用账号。
            self.affinity.retain(|_, v| v != id);
        }
    }

    /// 统计。
    pub fn counts(&self, now: u64) -> BTreeMap<&'static str, usize> {
        let mut out = BTreeMap::new();
        for a in self.accounts.values() {
            let key = if !a.enabled {
                "disabled"
            } else if a.is_available_at(now) {
                "available"
            } else {
                "paused"
            };
            *out.entry(key).or_insert(0) += 1;
        }
        out
    }
}

/// 订阅凭据(从外部工具读取)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionCredential {
    /// 来源(`"claude-code"` / `"codex"` / `"copilot"`)。
    pub source: String,
    /// 凭据文件路径。
    pub path: String,
    /// access token(读取后应立即存入 secrets,不落 config)。
    pub access_token: String,
    /// refresh token。
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// 过期时间。
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// 订阅档位(pro / max / …)。
    #[serde(default)]
    pub plan: Option<String>,
}

impl SubscriptionCredential {
    /// 在 `now` 时刻是否有效。
    pub fn is_valid_at(&self, now: u64) -> bool {
        match self.expires_at {
            Some(deadline) => now + 60 < deadline,
            None => true,
        }
    }

    /// 是否可刷新。
    pub fn can_refresh(&self) -> bool {
        self.refresh_token.is_some()
    }

    /// 脱敏展示(日志 / UI)。
    pub fn redacted(&self) -> String {
        let token = &self.access_token;
        let shown = if token.chars().count() <= 8 {
            "*".repeat(token.chars().count())
        } else {
            let head: String = token.chars().take(4).collect();
            let tail: String = token
                .chars()
                .rev()
                .take(4)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            format!("{head}…{tail}")
        };
        format!("{} ({}, plan={:?})", self.source, shown, self.plan)
    }
}

/// 已知的订阅凭据位置。
pub fn known_credential_paths() -> Vec<(&'static str, &'static str)> {
    vec![
        ("claude-code", "~/.claude/.credentials.json"),
        ("codex", "~/.codex/auth.json"),
        ("copilot", "~/.config/github-copilot/hosts.json"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str, provider: &str, usage: f64) -> Account {
        let mut a = Account::new(id, provider, id);
        a.usage_ratio = usage;
        a
    }

    #[test]
    fn device_flow_instruction() {
        let state =
            DeviceFlowState::new("dev123", "ABCD-EFGH", "https://example.com/device", 5, 1000);
        let instruction = state.user_instruction();
        assert!(instruction.contains("ABCD-EFGH"), "{instruction}");
        assert!(instruction.contains("example.com/device"));
    }

    #[test]
    fn device_flow_expiry() {
        let state = DeviceFlowState::new("d", "u", "v", 5, 100);
        assert!(!state.is_expired_at(50));
        assert!(state.is_expired_at(100));
        assert!(state.is_expired_at(200));
    }

    #[test]
    fn device_flow_terminal_stages() {
        let mut state = DeviceFlowState::new("d", "u", "v", 5, 100);
        assert!(!state.is_terminal(), "等待中不是终态");
        state.advance(DeviceFlowStage::Authorized);
        assert!(state.is_terminal());
        state.advance(DeviceFlowStage::Denied);
        assert!(state.is_terminal());
    }

    #[test]
    fn account_availability() {
        let mut a = account("a1", "anthropic", 0.3);
        assert!(a.is_available_at(100));
        a.pause_until(200);
        assert!(!a.is_available_at(100), "暂停中不可用");
        assert!(a.is_available_at(200), "到点恢复");
        a.enabled = false;
        assert!(!a.is_available_at(300), "禁用恒不可用");
    }

    #[test]
    fn remaining_ratio_clamped() {
        assert!((account("a", "p", 0.3).remaining_ratio() - 0.7).abs() < 1e-9);
        assert_eq!(account("a", "p", 1.5).remaining_ratio(), 0.0);
        assert_eq!(account("a", "p", -0.5).remaining_ratio(), 1.0);
    }

    #[test]
    fn pool_selects_highest_remaining() {
        let mut pool = AccountPool::new();
        pool.add(account("low", "anthropic", 0.8));
        pool.add(account("high", "anthropic", 0.1));
        let selected = pool.select_for("s1", "anthropic", 100).expect("select");
        assert_eq!(selected.id, "high", "剩余额度最高的胜出");
    }

    #[test]
    fn pool_filters_by_provider() {
        let mut pool = AccountPool::new();
        pool.add(account("anthropic-1", "anthropic", 0.1));
        pool.add(account("openai-1", "openai", 0.1));
        let selected = pool.select_for("s", "openai", 100).expect("select");
        assert_eq!(selected.id, "openai-1");
    }

    #[test]
    fn session_affinity_sticks() {
        let mut pool = AccountPool::new();
        pool.add(account("a", "p", 0.5));
        pool.add(account("b", "p", 0.1));
        // 首次选:b(剩余更多)
        let first = pool.select_for("s1", "p", 100).expect("first").id.clone();
        assert_eq!(first, "b");
        // a 的额度变多后,同会话仍用 b(亲和)
        pool.get_mut("a").unwrap().usage_ratio = 0.0;
        let second = pool.select_for("s1", "p", 100).expect("second").id.clone();
        assert_eq!(second, "b", "会话亲和保持,不因额度波动切换");
        assert_eq!(pool.affinity_of("s1"), Some("b"));
    }

    #[test]
    fn affinity_broken_when_account_unavailable() {
        let mut pool = AccountPool::new();
        pool.add(account("a", "p", 0.5));
        pool.add(account("b", "p", 0.1));
        let first = pool.select_for("s1", "p", 100).expect("first").id.clone();
        // 让选中账号耗尽
        pool.exhaust(&first, 500);
        let second = pool.select_for("s1", "p", 100).expect("second").id.clone();
        assert_ne!(second, first, "原账号耗尽后应切换");
    }

    #[test]
    fn exhaust_clears_affinity() {
        let mut pool = AccountPool::new();
        pool.add(account("a", "p", 0.5));
        pool.select_for("s1", "p", 100);
        assert_eq!(pool.affinity_of("s1"), Some("a"));
        pool.exhaust("a", 500);
        assert!(pool.affinity_of("s1").is_none(), "耗尽应清亲和");
        assert!(!pool.get("a").unwrap().is_available_at(100));
    }

    #[test]
    fn unbind_affinity() {
        let mut pool = AccountPool::new();
        pool.add(account("a", "p", 0.5));
        pool.select_for("s1", "p", 100);
        assert!(pool.unbind("s1"));
        assert!(!pool.unbind("s1"));
        assert!(pool.affinity_of("s1").is_none());
    }

    #[test]
    fn available_sorted_by_remaining() {
        let mut pool = AccountPool::new();
        pool.add(account("a", "p", 0.9));
        pool.add(account("b", "p", 0.1));
        pool.add(account("c", "p", 0.5));
        let avail = pool.available("p", 100);
        assert_eq!(avail[0].id, "b", "剩余最多在前");
        assert_eq!(avail[2].id, "a");
    }

    #[test]
    fn no_available_returns_none() {
        let mut pool = AccountPool::new();
        pool.add(account("a", "p", 0.5));
        pool.exhaust("a", 500);
        assert!(pool.select_for("s", "p", 100).is_none());
        // 到恢复时间后可用
        assert!(pool.select_for("s", "p", 500).is_some());
    }

    #[test]
    fn pool_counts() {
        let mut pool = AccountPool::new();
        pool.add(account("a", "p", 0.1));
        pool.add(account("b", "p", 0.1));
        let mut disabled = account("c", "p", 0.1);
        disabled.enabled = false;
        pool.add(disabled);
        pool.exhaust("b", 500);
        let counts = pool.counts(100);
        assert_eq!(counts["available"], 1);
        assert_eq!(counts["disabled"], 1);
        assert_eq!(counts["paused"], 1);
    }

    #[test]
    fn subscription_credential_validity() {
        let cred = SubscriptionCredential {
            source: "claude-code".into(),
            path: "~/.claude/.credentials.json".into(),
            access_token: "sk-ant-oat-abcdefghijklmn".into(),
            refresh_token: Some("refresh".into()),
            expires_at: Some(1000),
            plan: Some("max".into()),
        };
        assert!(cred.is_valid_at(500));
        assert!(!cred.is_valid_at(950), "留 60s 余量");
        assert!(cred.can_refresh());
    }

    #[test]
    fn subscription_credential_redaction() {
        let cred = SubscriptionCredential {
            source: "claude-code".into(),
            path: "p".into(),
            access_token: "sk-ant-oat-abcdefghijklmn".into(),
            refresh_token: None,
            expires_at: None,
            plan: Some("max".into()),
        };
        let redacted = cred.redacted();
        assert!(redacted.contains("claude-code"), "{redacted}");
        assert!(
            !redacted.contains("abcdefghijklmn"),
            "token 主体不得泄漏:{redacted}"
        );
        assert!(redacted.contains('…'), "{redacted}");
    }

    #[test]
    fn short_token_fully_masked() {
        let cred = SubscriptionCredential {
            source: "x".into(),
            path: "p".into(),
            access_token: "short".into(),
            refresh_token: None,
            expires_at: None,
            plan: None,
        };
        assert!(!cred.redacted().contains("short"));
    }

    #[test]
    fn known_paths_listed() {
        let paths = known_credential_paths();
        assert_eq!(paths.len(), 3);
        assert!(paths.iter().any(|(s, _)| *s == "claude-code"));
    }

    #[test]
    fn account_serializes() {
        let a = account("a", "p", 0.3);
        let json = serde_json::to_string(&a).expect("serialize");
        let back: Account = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, a);
    }
}
