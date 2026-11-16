//! # 网络出口管控(F23)
//!
//! `allow_network` 是粗粒度开关:开了就全放,关了全禁。真实需求介于两者
//! 之间 —— 「只许访问 github.com 和 crates.io,其它一律拒」。
//!
//! ## 三档
//!
//! - [`EgressPolicy::DenyAll`] —— 全禁(等价 `allow_network = false`);
//! - [`EgressPolicy::AllowAll`] —— 全放(等价 `allow_network = true`);
//! - [`EgressPolicy::Allowlist`] —— 只放白名单里的域名(含子域)。
//!
//! ## 匹配规则
//!
//! 白名单项 `github.com` 匹配 `github.com` 与 `api.github.com`
//! (**子域自动放行**,这是用户直觉);但不匹配 `notgithub.com`
//! (后缀必须落在 `.` 边界上)。`*.github.com` 显式写法等价于
//! `github.com` 的子域语义。
//!
//! ## 为什么要在**解析前**判定
//!
//! DNS 可以返回任意 IP,所以「解析后按 IP 放行」会被 DNS rebinding 绕过。
//! 本模块按**主机名**判定,并要求调用方在解析前调用 —— 配合
//! `web_fetch` 的 SSRF guard(F38)形成两层。

use serde::{Deserialize, Serialize};

/// 出口策略。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum EgressPolicy {
    /// 全禁(默认 —— 与 `allow_network = false` 一致)。
    #[default]
    DenyAll,
    /// 全放。
    AllowAll,
    /// 只放白名单。
    Allowlist {
        /// 域名(可含 `*.` 前缀)。
        domains: Vec<String>,
    },
}

/// 判定结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressDecision {
    pub allowed: bool,
    /// 人读理由(进审计 / 错误信息)。
    pub reason: String,
}

impl EgressDecision {
    fn allow(reason: impl Into<String>) -> EgressDecision {
        EgressDecision {
            allowed: true,
            reason: reason.into(),
        }
    }

    fn deny(reason: impl Into<String>) -> EgressDecision {
        EgressDecision {
            allowed: false,
            reason: reason.into(),
        }
    }
}

impl EgressPolicy {
    /// 从 `(allow_network, allowlist)` 构造 —— 兼容既有布尔配置。
    ///
    /// 语义:`allow_network = false` → 全禁(白名单不生效,保守);
    /// `allow_network = true` + 空白名单 → 全放;`allow_network = true` +
    /// 非空白名单 → 只放白名单。这样「开了网络但配了白名单」会**收紧**
    /// 而不是放宽 —— 配置错误时偏向安全侧。
    pub fn from_config(allow_network: bool, allowlist: Vec<String>) -> EgressPolicy {
        if !allow_network {
            EgressPolicy::DenyAll
        } else if allowlist.is_empty() {
            EgressPolicy::AllowAll
        } else {
            EgressPolicy::Allowlist { domains: allowlist }
        }
    }

    /// 判定能否访问 `host`。
    ///
    /// `host` 应是**已规范化**的主机名(小写、无端口、无 scheme)。
    pub fn check(&self, host: &str) -> EgressDecision {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() {
            return EgressDecision::deny("empty host");
        }
        match self {
            EgressPolicy::DenyAll => {
                EgressDecision::deny("egress denied: network is disabled for this sandbox")
            }
            EgressPolicy::AllowAll => EgressDecision::allow("egress allowed: network fully open"),
            EgressPolicy::Allowlist { domains } => {
                if domains.iter().any(|d| domain_matches(d, &host)) {
                    EgressDecision::allow(format!("egress allowed: {host} matches allowlist"))
                } else {
                    EgressDecision::deny(format!(
                        "egress denied: {host} not in allowlist ({} entries)",
                        domains.len()
                    ))
                }
            }
        }
    }

    /// 是否允许任意网络(用于快速路径判断)。
    pub fn is_open(&self) -> bool {
        matches!(self, EgressPolicy::AllowAll)
    }

    /// 是否完全禁网。
    pub fn is_closed(&self) -> bool {
        matches!(self, EgressPolicy::DenyAll)
    }

    /// 白名单条目数(`DenyAll`/`AllowAll` 为 0)。
    pub fn allowlist_len(&self) -> usize {
        match self {
            EgressPolicy::Allowlist { domains } => domains.len(),
            _ => 0,
        }
    }
}

/// 白名单项是否匹配主机。
///
/// - `github.com` 匹配 `github.com` 与任意子域(`api.github.com`);
/// - `*.github.com` 同上;
/// - **不**匹配 `notgithub.com`(后缀必须在 `.` 边界)。
fn domain_matches(entry: &str, host: &str) -> bool {
    let entry = entry.trim().trim_end_matches('.').to_ascii_lowercase();
    let entry = entry.strip_prefix("*.").unwrap_or(&entry);
    if entry.is_empty() {
        return false;
    }
    if host == entry {
        return true;
    }
    // 子域:host 以 `.<entry>` 结尾。
    host.len() > entry.len() + 1
        && host.ends_with(entry)
        && host.as_bytes()[host.len() - entry.len() - 1] == b'.'
}

/// 从 URL 里抽主机名(轻量,不引 url crate)。
///
/// 支持 `https://host:port/path` / `http://host/path` / `host:port`。
/// 解析失败返回 `None` —— 调用方应视为**拒绝**(无法判定即不放行)。
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.trim();
    // 去掉 scheme
    let rest = match rest.find("://") {
        Some(idx) => &rest[idx + 3..],
        None => rest,
    };
    // 到第一个 `/` / `?` / `#` 为止
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() {
        return None;
    }
    // 去掉 userinfo(`user:pass@host`)
    let authority = match authority.rfind('@') {
        Some(idx) => &authority[idx + 1..],
        None => authority,
    };
    // 去掉端口(IPv6 是 `[::1]:port`,单独处理)
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        // IPv6:`[::1]` 或 `[::1]:8080`
        let end = stripped.find(']')?;
        &stripped[..end]
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    if host.is_empty() {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowlist(domains: &[&str]) -> EgressPolicy {
        EgressPolicy::Allowlist {
            domains: domains.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn deny_all_blocks_everything() {
        let p = EgressPolicy::DenyAll;
        assert!(!p.check("github.com").allowed);
        assert!(!p.check("localhost").allowed);
        assert!(p.is_closed());
    }

    #[test]
    fn allow_all_permits_everything() {
        let p = EgressPolicy::AllowAll;
        assert!(p.check("anything.example").allowed);
        assert!(p.is_open());
    }

    #[test]
    fn allowlist_exact_match() {
        let p = allowlist(&["github.com"]);
        assert!(p.check("github.com").allowed);
        assert!(!p.check("gitlab.com").allowed);
    }

    #[test]
    fn allowlist_subdomain_match() {
        let p = allowlist(&["github.com"]);
        assert!(p.check("api.github.com").allowed);
        assert!(!p.check("raw.githubusercontent.com").allowed);
    }

    #[test]
    fn allowlist_rejects_suffix_confusion() {
        let p = allowlist(&["github.com"]);
        assert!(!p.check("notgithub.com").allowed, "后缀必须在点边界");
        assert!(!p.check("evil-github.com").allowed);
        assert!(!p.check("github.com.evil.example").allowed);
    }

    #[test]
    fn allowlist_wildcard_prefix_equivalent() {
        let p = allowlist(&["*.github.com"]);
        assert!(p.check("github.com").allowed);
        assert!(p.check("api.github.com").allowed);
        assert!(!p.check("notgithub.com").allowed);
    }

    #[test]
    fn allowlist_case_and_trailing_dot_normalized() {
        let p = allowlist(&["GitHub.COM"]);
        assert!(p.check("API.GITHUB.COM").allowed);
        assert!(p.check("github.com.").allowed);
    }

    #[test]
    fn empty_host_denied() {
        let p = allowlist(&["github.com"]);
        assert!(!p.check("").allowed);
        assert!(!p.check("   ").allowed);
    }

    #[test]
    fn from_config_deny_when_network_off() {
        // 网络关 + 有白名单 → 仍然全禁(保守,白名单不生效)。
        let p = EgressPolicy::from_config(false, vec!["github.com".into()]);
        assert!(p.is_closed());
        assert!(!p.check("github.com").allowed);
    }

    #[test]
    fn from_config_allow_all_when_no_list() {
        let p = EgressPolicy::from_config(true, vec![]);
        assert!(p.is_open());
    }

    #[test]
    fn from_config_allowlist_tightens() {
        // 网络开 + 有白名单 → 收紧为白名单(不是放宽)。
        let p = EgressPolicy::from_config(true, vec!["github.com".into()]);
        assert!(!p.is_open());
        assert!(p.check("github.com").allowed);
        assert!(!p.check("example.com").allowed);
    }

    #[test]
    fn host_of_https_url() {
        assert_eq!(host_of("https://github.com/a/b"), Some("github.com".into()));
        assert_eq!(
            host_of("https://API.GitHub.com/x"),
            Some("api.github.com".into())
        );
    }

    #[test]
    fn host_of_with_port() {
        assert_eq!(host_of("http://localhost:8080/x"), Some("localhost".into()));
    }

    #[test]
    fn host_of_with_userinfo() {
        assert_eq!(
            host_of("https://user:pass@example.com/x"),
            Some("example.com".into())
        );
    }

    #[test]
    fn host_of_ipv6() {
        assert_eq!(host_of("http://[::1]:8080/x"), Some("::1".into()));
        assert_eq!(
            host_of("http://[2001:db8::1]/x"),
            Some("2001:db8::1".into())
        );
    }

    #[test]
    fn host_of_bare_host() {
        assert_eq!(host_of("example.com/path"), Some("example.com".into()));
    }

    #[test]
    fn host_of_invalid() {
        assert_eq!(host_of(""), None);
        assert_eq!(host_of("https://"), None);
    }

    #[test]
    fn end_to_end_url_check() {
        let p = allowlist(&["github.com", "crates.io"]);
        let check = |url: &str| host_of(url).is_some_and(|h| p.check(&h).allowed);
        assert!(check("https://api.github.com/repos"));
        assert!(check("https://crates.io/api/v1"));
        assert!(!check("https://evil.example/steal"));
        assert!(!check("not-a-url"), "无法解析即拒绝");
    }

    #[test]
    fn policy_serializes() {
        let p = allowlist(&["github.com"]);
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(json.contains("allowlist"), "{json}");
        let back: EgressPolicy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, p);
    }

    #[test]
    fn allowlist_len() {
        assert_eq!(allowlist(&["a.com", "b.com"]).allowlist_len(), 2);
        assert_eq!(EgressPolicy::AllowAll.allowlist_len(), 0);
    }
}
