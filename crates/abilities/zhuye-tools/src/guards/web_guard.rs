//! # 搜索与网络增强(F38)
//!
//! 两件事:
//!
//! 1. **多后端搜索**([`SearchBackend`])—— 不绑死一家。免费后端
//!    (DuckDuckGo / SearXNG)可换,支持 failover 链与代理配置。
//! 2. **SSRF 加固**([`is_ssrf_risk`])—— 抓网页前挡住内网地址。
//!
//! ## SSRF 为什么是真实风险
//!
//! agent 抓 `http://169.254.169.254/latest/meta-data/` 就能拿到云实例
//! 的临时凭据。这不只是"访问内网" —— 是**凭据泄漏**。所以必须挡:
//!
//! - 回环(`127.0.0.0/8` / `::1`);
//! - 链路本地(`169.254.0.0/16`,含 AWS/GCP 元数据端点);
//! - 私网(`10/8` / `172.16/12` / `192.168/16` / `fc00::/7`);
//! - **特殊用途段**(`198.18.0.0/15` 基准测试段、`0.0.0.0/8`);
//! - IPv6 特殊段(IPv4-mapped、`64:ff9b::/96` NAT64)。
//!
//! ## 为什么按**字面 IP** 判定不够
//!
//! 域名可以解析到内网(attacker.com → 127.0.0.1)。完整防护需要
//! 「解析后校验」([`check_resolved`])。本模块提供两级:
//!
//! - [`is_ssrf_risk`] —— 字面地址快速检查(挡直接 IP 访问);
//! - [`check_resolved`] —— 解析后检查(挡 DNS rebinding)。

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// 搜索后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchBackend {
    /// DuckDuckGo(免 key)。
    DuckDuckGo,
    /// SearXNG 实例(自建 / 公共)。
    SearXNG,
    /// Brave Search(需 key)。
    Brave,
    /// Tavily(需 key,面向 agent)。
    Tavily,
    /// 自定义 OpenAI 兼容搜索端点。
    Custom,
}

impl SearchBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            SearchBackend::DuckDuckGo => "duckduckgo",
            SearchBackend::SearXNG => "searxng",
            SearchBackend::Brave => "brave",
            SearchBackend::Tavily => "tavily",
            SearchBackend::Custom => "custom",
        }
    }

    pub fn parse(s: &str) -> Option<SearchBackend> {
        match s.trim().to_ascii_lowercase().as_str() {
            "duckduckgo" | "ddg" => Some(SearchBackend::DuckDuckGo),
            "searxng" => Some(SearchBackend::SearXNG),
            "brave" => Some(SearchBackend::Brave),
            "tavily" => Some(SearchBackend::Tavily),
            "custom" => Some(SearchBackend::Custom),
            _ => None,
        }
    }

    /// 是否需要 API key。
    pub fn needs_api_key(self) -> bool {
        matches!(
            self,
            SearchBackend::Brave | SearchBackend::Tavily | SearchBackend::Custom
        )
    }

    /// 是否免 key(可直接用)。
    pub fn is_keyless(self) -> bool {
        !self.needs_api_key()
    }
}

/// 后端配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendConfig {
    pub backend: SearchBackend,
    /// 自定义端点(SearXNG / Custom 必填)。
    #[serde(default)]
    pub endpoint: Option<String>,
    /// API key(需 key 的后端必填)。
    #[serde(default)]
    pub api_key: Option<String>,
}

impl BackendConfig {
    pub fn new(backend: SearchBackend) -> BackendConfig {
        BackendConfig {
            backend,
            endpoint: None,
            api_key: None,
        }
    }

    /// 配置是否完整可用。
    pub fn is_usable(&self) -> bool {
        match self.backend {
            SearchBackend::DuckDuckGo => true,
            SearchBackend::SearXNG => self.endpoint.as_ref().is_some_and(|e| !e.trim().is_empty()),
            SearchBackend::Brave | SearchBackend::Tavily => {
                self.api_key.as_ref().is_some_and(|k| !k.trim().is_empty())
            }
            SearchBackend::Custom => {
                self.endpoint.as_ref().is_some_and(|e| !e.trim().is_empty())
                    && self.api_key.as_ref().is_some_and(|k| !k.trim().is_empty())
            }
        }
    }
}

/// failover 链。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailoverChain {
    pub backends: Vec<BackendConfig>,
}

impl FailoverChain {
    pub fn new(backends: Vec<BackendConfig>) -> FailoverChain {
        FailoverChain { backends }
    }

    /// 默认链:免 key 优先。
    pub fn default_chain() -> FailoverChain {
        FailoverChain::new(vec![
            BackendConfig::new(SearchBackend::DuckDuckGo),
            BackendConfig {
                endpoint: Some("http://127.0.0.1:8080".to_string()),
                ..BackendConfig::new(SearchBackend::SearXNG)
            },
        ])
    }

    /// 可用的后端(按顺序)。
    pub fn usable(&self) -> Vec<&BackendConfig> {
        self.backends.iter().filter(|b| b.is_usable()).collect()
    }

    /// 首选可用后端。
    pub fn primary(&self) -> Option<&BackendConfig> {
        self.usable().into_iter().next()
    }

    /// 按顺序尝试的回退列表(含全部可用项)。
    pub fn fallbacks(&self) -> Vec<SearchBackend> {
        self.usable().into_iter().map(|b| b.backend).collect()
    }
}

/// 代理配置。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// HTTP 代理 URL。
    #[serde(default)]
    pub http_proxy: Option<String>,
    /// HTTPS 代理 URL。
    #[serde(default)]
    pub https_proxy: Option<String>,
    /// 不走代理的域名(逗号分隔,`NO_PROXY` 语义)。
    #[serde(default)]
    pub no_proxy: Vec<String>,
    /// 是否使用系统 CA(企业 MITM 场景)。
    #[serde(default)]
    pub use_system_ca: bool,
}

impl ProxyConfig {
    /// 某主机是否应走代理。
    pub fn should_proxy(&self, host: &str) -> bool {
        if self.http_proxy.is_none() && self.https_proxy.is_none() {
            return false;
        }
        let host = host.trim().to_ascii_lowercase();
        // NO_PROXY 匹配(后缀匹配,`.example.com` 或 `example.com`)。
        for entry in &self.no_proxy {
            let entry = entry.trim().trim_start_matches('.').to_ascii_lowercase();
            if entry.is_empty() {
                continue;
            }
            if host == entry || host.ends_with(&format!(".{entry}")) {
                return false;
            }
        }
        true
    }
}

/// 是否为 SSRF 风险地址(字面 IP)。
pub fn is_ssrf_risk(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            v4.is_loopback()            // 127.0.0.0/8
                || v4.is_private()      // 10/8, 172.16/12, 192.168/16
                || v4.is_link_local()   // 169.254.0.0/16(含云元数据)
                || v4.is_broadcast()    // 255.255.255.255
                || v4.is_unspecified()  // 0.0.0.0
                || v4.is_documentation()
                // 198.18.0.0/15 基准测试段(常被用作"内网别名")
                || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
                // 100.64.0.0/10 CGNAT
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                // 192.0.0.0/24 IETF 协议分配
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
                // 198.51.100.0/24 / 203.0.113.0/24 已由 is_documentation 覆盖
                // 240.0.0.0/4 保留
                || octets[0] >= 240
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // IPv4-mapped(::ffff:a.b.c.d)—— 拆出 IPv4 再判
                || v6.to_ipv4_mapped().is_some_and(|v4| is_ssrf_risk(IpAddr::V4(v4)))
                // 链路本地 fe80::/10
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // 唯一本地 fc00::/7
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // NAT64 64:ff9b::/96 —— 可映射到任意 IPv4
                || (v6.segments()[0] == 0x0064
                    && v6.segments()[1] == 0xff9b
                    && v6.segments()[2] == 0
                    && v6.segments()[3] == 0
                    && v6.segments()[4] == 0
                    && v6.segments()[5] == 0)
                // 站点本地 fec0::/10(deprecated 但仍可能路由)
                || (v6.segments()[0] & 0xffc0) == 0xfec0
        }
    }
}

/// 解析后检查(挡 DNS rebinding)。
///
/// `resolved` 是 DNS 解析出的全部地址;**任一**是风险地址即拒绝
/// (攻击者可能返回多个地址,其中一个是内网)。
pub fn check_resolved(resolved: &[IpAddr]) -> Result<(), String> {
    if resolved.is_empty() {
        return Err("DNS 解析无结果".to_string());
    }
    for ip in resolved {
        if is_ssrf_risk(*ip) {
            return Err(format!("目标解析到内网/保留地址 {ip},已拒绝(SSRF 防护)"));
        }
    }
    Ok(())
}

/// URL 参数脱敏:把 query 里的敏感参数值抹掉(日志 / 审计用)。
///
/// 命中参数名:`token` / `key` / `secret` / `password` / `sig` / `auth`。
pub fn redact_url_params(url: &str) -> String {
    const SENSITIVE: [&str; 6] = ["token", "key", "secret", "password", "sig", "auth"];
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let redacted: Vec<String> = query
        .split('&')
        .map(|pair| {
            let Some((name, _value)) = pair.split_once('=') else {
                return pair.to_string();
            };
            let lower = name.to_ascii_lowercase();
            if SENSITIVE.iter().any(|s| lower.contains(s)) {
                format!("{name}=[REDACTED]")
            } else {
                pair.to_string()
            }
        })
        .collect();
    format!("{base}?{}", redacted.join("&"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn backend_roundtrip() {
        for b in [
            SearchBackend::DuckDuckGo,
            SearchBackend::SearXNG,
            SearchBackend::Brave,
            SearchBackend::Tavily,
            SearchBackend::Custom,
        ] {
            assert_eq!(SearchBackend::parse(b.as_str()), Some(b));
        }
        assert_eq!(SearchBackend::parse("ddg"), Some(SearchBackend::DuckDuckGo));
        assert_eq!(SearchBackend::parse("bogus"), None);
    }

    #[test]
    fn keyless_backends() {
        assert!(SearchBackend::DuckDuckGo.is_keyless());
        assert!(SearchBackend::SearXNG.is_keyless());
        assert!(!SearchBackend::Brave.is_keyless());
        assert!(SearchBackend::Tavily.needs_api_key());
    }

    #[test]
    fn backend_config_usability() {
        assert!(BackendConfig::new(SearchBackend::DuckDuckGo).is_usable());
        assert!(
            !BackendConfig::new(SearchBackend::SearXNG).is_usable(),
            "缺 endpoint"
        );
        assert!(
            BackendConfig {
                endpoint: Some("http://x".into()),
                ..BackendConfig::new(SearchBackend::SearXNG)
            }
            .is_usable()
        );
        assert!(
            !BackendConfig::new(SearchBackend::Brave).is_usable(),
            "缺 key"
        );
        assert!(
            BackendConfig {
                api_key: Some("k".into()),
                ..BackendConfig::new(SearchBackend::Brave)
            }
            .is_usable()
        );
    }

    #[test]
    fn failover_chain_primary_and_fallbacks() {
        let chain = FailoverChain::default_chain();
        let primary = chain.primary().expect("primary");
        assert_eq!(primary.backend, SearchBackend::DuckDuckGo);
        let fallbacks = chain.fallbacks();
        assert_eq!(fallbacks.len(), 2);
        assert_eq!(fallbacks[0], SearchBackend::DuckDuckGo);
    }

    #[test]
    fn failover_chain_skips_unusable() {
        let chain = FailoverChain::new(vec![
            BackendConfig::new(SearchBackend::Brave), // 无 key,不可用
            BackendConfig::new(SearchBackend::DuckDuckGo),
        ]);
        assert_eq!(
            chain.primary().map(|b| b.backend),
            Some(SearchBackend::DuckDuckGo)
        );
    }

    #[test]
    fn ssrf_blocks_loopback_and_private() {
        for ip in [
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254", // 云元数据端点
            "0.0.0.0",
        ] {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(is_ssrf_risk(addr), "{ip} 应被拦");
        }
    }

    #[test]
    fn ssrf_blocks_special_ranges() {
        for ip in [
            "198.18.0.1", // 基准测试段
            "198.19.255.254",
            "100.64.0.1", // CGNAT
            "192.0.0.1",  // IETF 协议分配
            "240.0.0.1",  // 保留
            "255.255.255.255",
        ] {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(is_ssrf_risk(addr), "{ip} 应被拦");
        }
    }

    #[test]
    fn ssrf_allows_public() {
        for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34"] {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(!is_ssrf_risk(addr), "{ip} 应放行");
        }
    }

    #[test]
    fn ssrf_blocks_ipv6_special() {
        for ip in [
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "::ffff:127.0.0.1", // IPv4-mapped 回环
            "::ffff:10.0.0.1",  // IPv4-mapped 私网
            "64:ff9b::1",       // NAT64
        ] {
            let addr: IpAddr = ip.parse().unwrap();
            assert!(is_ssrf_risk(addr), "{ip} 应被拦");
        }
    }

    #[test]
    fn ssrf_allows_public_ipv6() {
        let addr: IpAddr = "2001:4860:4860::8888".parse().unwrap();
        assert!(!is_ssrf_risk(addr), "Google DNS 应放行");
    }

    #[test]
    fn check_resolved_rejects_any_private() {
        // 多地址里有一个内网 → 整体拒绝(DNS rebinding 防护)。
        let resolved: Vec<IpAddr> = vec![
            "93.184.216.34".parse().unwrap(),
            "127.0.0.1".parse().unwrap(),
        ];
        let err = check_resolved(&resolved).unwrap_err();
        assert!(err.contains("SSRF"), "{err}");
    }

    #[test]
    fn check_resolved_accepts_all_public() {
        let resolved: Vec<IpAddr> = vec!["8.8.8.8".parse().unwrap(), "1.1.1.1".parse().unwrap()];
        assert!(check_resolved(&resolved).is_ok());
    }

    #[test]
    fn check_resolved_rejects_empty() {
        assert!(check_resolved(&[]).is_err());
    }

    #[test]
    fn proxy_should_proxy() {
        let cfg = ProxyConfig {
            http_proxy: Some("http://proxy:8080".into()),
            no_proxy: vec!["example.com".into(), ".internal".into()],
            ..Default::default()
        };
        assert!(cfg.should_proxy("api.github.com"));
        assert!(!cfg.should_proxy("example.com"), "NO_PROXY 精确");
        assert!(!cfg.should_proxy("api.example.com"), "NO_PROXY 子域");
        assert!(!cfg.should_proxy("svc.internal"), "NO_PROXY 点前缀");
    }

    #[test]
    fn proxy_without_config_never_proxies() {
        let cfg = ProxyConfig::default();
        assert!(!cfg.should_proxy("anything.com"));
    }

    #[test]
    fn redact_url_params_masks_sensitive() {
        let url = "https://api.example.com/v1?token=secret123&q=hello&api_key=abc";
        let redacted = redact_url_params(url);
        assert!(!redacted.contains("secret123"), "{redacted}");
        assert!(!redacted.contains("abc"), "{redacted}");
        assert!(redacted.contains("q=hello"), "普通参数保留:{redacted}");
        assert!(redacted.contains("token=[REDACTED]"), "{redacted}");
    }

    #[test]
    fn redact_url_without_query() {
        let url = "https://example.com/path";
        assert_eq!(redact_url_params(url), url);
    }

    #[test]
    fn proxy_config_serializes() {
        let cfg = ProxyConfig {
            https_proxy: Some("http://p:8080".into()),
            use_system_ca: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).expect("serialize");
        let back: ProxyConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, cfg);
    }

    #[test]
    fn ipv4_mapped_extraction() {
        // 确认 to_ipv4_mapped 路径有效。
        let v6 = Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x7f00, 0x0001);
        assert!(is_ssrf_risk(IpAddr::V6(v6)));
        let public = Ipv4Addr::new(8, 8, 8, 8);
        let v6_public = Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x0808, 0x0808);
        assert!(!is_ssrf_risk(IpAddr::V6(v6_public)));
        let _ = public;
    }
}
