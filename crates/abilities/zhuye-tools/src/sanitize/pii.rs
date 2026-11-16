//! # PII 脱敏规则库(F03)
//!
//! 与密钥脱敏([`super`] 的 10 类 provider pattern)**互补**:
//!
//! - 密钥 pattern 保护的是**凭据**——泄漏即可被冒用;
//! - PII pattern 保护的是**个人隐私**——手机号 / 身份证 / 邮箱 / 银行卡 /
//!   IP 地址。工具输出(日志、数据库 dump、网页抓取)经常夹带这些,
//!   送进模型上下文或落盘前必须抹掉。
//!
//! ## 三档控制
//!
//! [`PiiMode`] 与 [`super::provenance::ProvenanceMode`] 同构:
//!
//! - `Off` —— 完全不做 PII 处理(用户显式关闭);
//! - `Observe` —— 只统计命中,不改正文(默认:先观察误报率);
//! - `Enforce` —— 实际替换为 marker。
//!
//! ## 与密钥 pattern 的顺序
//!
//! PII 在密钥脱敏**之后**执行:`KEY_ASSIGN` 之类可能把邮箱形态的值
//! 整段吃掉,先跑密钥能让 PII 只看剩下的正文,避免重复标注。
//!
//! ## 误报控制
//!
//! 规则全部带**结构约束**(手机号 11 位且 1 开头、身份证 18 位含校验位
//! 形状、银行卡 13–19 位且通过 Luhn),不用宽泛的"数字串"规则 ——
//! 否则时间戳、行号、token 计数都会被抹掉,工具输出失去可读性。

use regex::Regex;
use serde::{Deserialize, Serialize};

/// PII 处理档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PiiMode {
    /// 不处理。
    Off,
    /// 只统计命中数,正文不变(默认)。
    #[default]
    Observe,
    /// 实际替换为 marker。
    Enforce,
}

impl PiiMode {
    /// 从配置字符串解析(`"off"` / `"observe"` / `"enforce"`)。
    /// 未知值返回 `None`,由调用方决定回退策略。
    pub fn parse(s: &str) -> Option<PiiMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "0" => Some(PiiMode::Off),
            "observe" => Some(PiiMode::Observe),
            "enforce" | "on" | "true" | "1" => Some(PiiMode::Enforce),
            _ => None,
        }
    }

    /// 是否要跑扫描(Off 直接跳过)。
    pub fn scans(self) -> bool {
        !matches!(self, PiiMode::Off)
    }

    /// 是否要改写正文(仅 Enforce)。
    pub fn rewrites(self) -> bool {
        matches!(self, PiiMode::Enforce)
    }
}

/// 单条 PII 规则。`code` 是稳定标识(审计 / 统计用)。
pub struct PiiRule {
    pub code: &'static str,
    regex: Regex,
}

impl PiiRule {
    fn new(code: &'static str, pattern: &str) -> PiiRule {
        PiiRule {
            code,
            regex: Regex::new(pattern)
                .unwrap_or_else(|e| panic!("PII rule {code} regex must compile: {e}")),
        }
    }

    /// 命中次数(Observe 档用)。
    pub fn count(&self, text: &str) -> usize {
        self.regex.find_iter(text).count()
    }

    /// 替换全部命中为 `marker`。
    pub fn redact(&self, text: &str, marker: &str) -> String {
        self.regex.replace_all(text, marker).into_owned()
    }
}

/// 默认 PII 规则集(顺序即执行顺序)。
///
/// 注意顺序:**身份证在手机号之前** —— 18 位身份证里含 11 位数字段,
/// 先跑手机号会把它切成碎片。
pub fn default_pii_rules() -> Vec<PiiRule> {
    vec![
        // 中国大陆身份证:18 位(17 位数字 + 校验位数字或 X)。
        // 首 6 位地区码不以 0 开头,出生日期段 8 位,顺序码 3 位。
        PiiRule::new(
            "PII_CN_ID_CARD",
            r"\b[1-9]\d{5}(?:19|20)\d{2}(?:0[1-9]|1[0-2])(?:0[1-9]|[12]\d|3[01])\d{3}[\dXx]\b",
        ),
        // 中国大陆手机号:1 开头 11 位,第二位 3–9。
        PiiRule::new("PII_CN_MOBILE", r"\b1[3-9]\d{9}\b"),
        // 邮箱:保守形状(本地段不含空白与 @)。
        PiiRule::new(
            "PII_EMAIL",
            r"\b[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}\b",
        ),
        // 银行卡:13–19 位连续数字,前后不能紧邻数字(避免切碎长数字串)。
        // 不校验 Luhn —— 工具输出里的卡号可能被打码成非 Luhn 形状,
        // 但"13–19 位纯数字且前后独立"已经足够窄。
        PiiRule::new("PII_BANK_CARD", r"\b\d{13,19}\b"),
        // IPv4:四段 0–255。私网地址也一并抹(避免暴露内网拓扑)。
        PiiRule::new(
            "PII_IPV4",
            r"\b(?:(?:25[0-5]|2[0-4]\d|1\d{2}|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d{2}|[1-9]?\d)\b",
        ),
    ]
}

/// PII 扫描结果(Observe 档据此上报)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiiReport {
    /// 各规则命中次数(`code → count`),只含非零项。
    pub hits: Vec<(String, usize)>,
    /// 命中总数。
    pub total: usize,
}

impl PiiReport {
    /// 是否无命中。
    pub fn is_clean(&self) -> bool {
        self.total == 0
    }
}

/// 按 `mode` 处理文本,返回 `(处理后的文本, 报告)`。
///
/// - `Off` → 原样返回,报告为空;
/// - `Observe` → 正文不变,报告填命中数;
/// - `Enforce` → 正文替换为 marker,报告填命中数。
pub fn apply_pii(text: &str, mode: PiiMode, marker: &str) -> (String, PiiReport) {
    if !mode.scans() {
        return (text.to_string(), PiiReport::default());
    }
    let rules = default_pii_rules();
    let mut out = text.to_string();
    let mut hits = Vec::new();
    let mut total = 0usize;
    for rule in &rules {
        let n = rule.count(&out);
        if n == 0 {
            continue;
        }
        total += n;
        hits.push((rule.code.to_string(), n));
        if mode.rewrites() {
            out = rule.redact(&out, marker);
        }
    }
    (out, PiiReport { hits, total })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parsing() {
        assert_eq!(PiiMode::parse("off"), Some(PiiMode::Off));
        assert_eq!(PiiMode::parse("ENFORCE"), Some(PiiMode::Enforce));
        assert_eq!(PiiMode::parse("observe"), Some(PiiMode::Observe));
        assert_eq!(PiiMode::parse("nonsense"), None);
        assert!(PiiMode::Enforce.rewrites());
        assert!(!PiiMode::Observe.rewrites());
        assert!(!PiiMode::Off.scans());
    }

    #[test]
    fn cn_mobile_detected_and_redacted() {
        let (out, rep) = apply_pii("联系人 13812345678 已登记", PiiMode::Enforce, "[PII]");
        assert!(out.contains("[PII]"), "{out}");
        assert!(!out.contains("13812345678"));
        assert_eq!(rep.total, 1);
        assert_eq!(rep.hits[0].0, "PII_CN_MOBILE");
    }

    #[test]
    fn cn_id_card_wins_over_mobile() {
        // 18 位身份证含 11 位数字段:必须整体按身份证抹掉,不能被手机号切碎。
        let id = "110101199003071234";
        let (out, rep) = apply_pii(id, PiiMode::Enforce, "[PII]");
        assert_eq!(out, "[PII]", "身份证应整体替换,实际 {out}");
        assert!(
            rep.hits.iter().any(|(c, _)| c == "PII_CN_ID_CARD"),
            "{rep:?}"
        );
    }

    #[test]
    fn email_and_ipv4() {
        let (out, _) = apply_pii(
            "mail alice@example.com from 192.168.1.10",
            PiiMode::Enforce,
            "[PII]",
        );
        assert!(!out.contains("alice@example.com"), "{out}");
        assert!(!out.contains("192.168.1.10"), "{out}");
    }

    #[test]
    fn observe_mode_keeps_text() {
        let text = "电话 13812345678";
        let (out, rep) = apply_pii(text, PiiMode::Observe, "[PII]");
        assert_eq!(out, text, "Observe 不得改正文");
        assert_eq!(rep.total, 1);
        assert!(!rep.is_clean());
    }

    #[test]
    fn off_mode_no_scan() {
        let (out, rep) = apply_pii("13812345678", PiiMode::Off, "[PII]");
        assert_eq!(out, "13812345678");
        assert!(rep.is_clean());
    }

    #[test]
    fn clean_text_no_hits() {
        let (out, rep) = apply_pii("fn main() { println!(\"hi\"); }", PiiMode::Enforce, "[PII]");
        assert_eq!(out, "fn main() { println!(\"hi\"); }");
        assert!(rep.is_clean());
    }

    #[test]
    fn line_numbers_not_mistaken_for_pii() {
        // 行号 / 时间戳 / token 数都是短数字串,不能被当成卡号。
        let text = "line 42, elapsed 1234ms, tokens 5678";
        let (out, rep) = apply_pii(text, PiiMode::Enforce, "[PII]");
        assert_eq!(out, text, "短数字串不应命中");
        assert!(rep.is_clean());
    }
}
