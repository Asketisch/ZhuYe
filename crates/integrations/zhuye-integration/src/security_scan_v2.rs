//! # 全库安全扫描与编排长尾(F60)
//!
//! 两件事:
//!
//! 1. **全库安全扫描**([`SecurityScan`])—— matcher → AI triage →
//!    revalidate → SARIF。找硬编码密钥 / 危险命令 / agent 自身可被利用的
//!    面。
//! 2. **共识投票**([`ConsensusVote`])—— 同一问题 N 个模型分头做再投票,
//!    提高关键决策的可靠性。
//!
//! ## 为什么扫描要分四段
//!
//! 直接 matcher 会报大量误报(测试里的假 key、文档里的示例命令)。
//! 四段流水线:
//!
//! 1. **matcher** —— 规则快速初筛(高召回,低精度);
//! 2. **triage** —— 模型判断"这是真问题还是示例"(可选);
//! 3. **revalidate** —— 对疑似项再验一次(上下文确认);
//! 4. **SARIF** —— 输出标准格式,可导入 GitHub Code Scanning。
//!
//! ## 为什么共识投票值得
//!
//! 单个模型判"这段代码有安全漏洞"可能错。N 个模型独立判、取多数,
//! 能显著降低误报 —— 代价是 N 倍成本,所以只用于**关键**决策。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 扫描发现严重级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Note,
    Warning,
    Error,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Note => "note",
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }

    /// SARIF level。
    pub fn sarif_level(self) -> &'static str {
        self.as_str()
    }
}

/// 扫描发现状态(流水线阶段)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingState {
    /// matcher 初筛。
    Matched,
    /// triage 后确认为真问题。
    Confirmed,
    /// triage 后判为误报。
    Dismissed,
    /// revalidate 后降级。
    Downgraded,
}

/// 一条扫描发现。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityFinding {
    /// 稳定 id(`rule:path:line`)。
    pub id: String,
    /// 规则码(如 `hardcoded-secret` / `dangerous-command`)。
    pub rule: String,
    pub severity: Severity,
    /// 文件路径。
    pub path: String,
    /// 行号(1 起)。
    pub line: usize,
    /// 消息。
    pub message: String,
    pub state: FindingState,
    /// triage 理由(可选)。
    #[serde(default)]
    pub triage_reason: Option<String>,
}

impl SecurityFinding {
    /// 构造 matcher 初筛结果。
    pub fn matched(
        rule: impl Into<String>,
        severity: Severity,
        path: impl Into<String>,
        line: usize,
        message: impl Into<String>,
    ) -> SecurityFinding {
        let rule = rule.into();
        let path = path.into();
        let id = format!("{rule}:{path}:{line}");
        SecurityFinding {
            id,
            rule,
            severity,
            path,
            line,
            message: message.into(),
            state: FindingState::Matched,
            triage_reason: None,
        }
    }

    /// 是否仍应报告(confirmed / matched 且非 dismissed)。
    pub fn is_reportable(&self) -> bool {
        !matches!(self.state, FindingState::Dismissed)
    }

    /// 是否为 Error 级且未被驳回。
    pub fn is_blocking(&self) -> bool {
        self.severity == Severity::Error && self.is_reportable()
    }
}

/// 扫描流水线。
#[derive(Debug, Default)]
pub struct SecurityScan {
    findings: Vec<SecurityFinding>,
}

impl SecurityScan {
    pub fn new() -> SecurityScan {
        SecurityScan::default()
    }

    /// matcher 阶段:加入初筛结果。
    pub fn add_matched(&mut self, finding: SecurityFinding) {
        // 去重:同 id 只保留一条。
        if !self.findings.iter().any(|f| f.id == finding.id) {
            self.findings.push(finding);
        }
    }

    /// triage 阶段:确认或驳回。
    pub fn triage(&mut self, id: &str, confirmed: bool, reason: impl Into<String>) -> bool {
        if let Some(f) = self.findings.iter_mut().find(|f| f.id == id) {
            f.state = if confirmed {
                FindingState::Confirmed
            } else {
                FindingState::Dismissed
            };
            f.triage_reason = Some(reason.into());
            return true;
        }
        false
    }

    /// revalidate 阶段:降级严重级。
    pub fn downgrade(&mut self, id: &str, new_severity: Severity) -> bool {
        if let Some(f) = self.findings.iter_mut().find(|f| f.id == id) {
            f.severity = new_severity;
            f.state = FindingState::Downgraded;
            return true;
        }
        false
    }

    /// 全部发现。
    pub fn all(&self) -> &[SecurityFinding] {
        &self.findings
    }

    /// 应报告的发现。
    pub fn reportable(&self) -> Vec<&SecurityFinding> {
        self.findings.iter().filter(|f| f.is_reportable()).collect()
    }

    /// 阻断项(Error 级未驳回)。
    pub fn blocking(&self) -> Vec<&SecurityFinding> {
        self.findings.iter().filter(|f| f.is_blocking()).collect()
    }

    /// 按严重级统计。
    pub fn counts(&self) -> BTreeMap<&'static str, usize> {
        let mut out = BTreeMap::new();
        for f in self.reportable() {
            *out.entry(f.severity.as_str()).or_insert(0) += 1;
        }
        out
    }

    /// 是否可放行(无阻断项)。
    pub fn is_clean(&self) -> bool {
        self.blocking().is_empty()
    }

    pub fn len(&self) -> usize {
        self.findings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.findings.is_empty()
    }

    /// 导出 SARIF 2.1.0(简化)。
    pub fn to_sarif(&self, tool_name: &str) -> serde_json::Value {
        let results: Vec<serde_json::Value> = self
            .reportable()
            .iter()
            .map(|f| {
                serde_json::json!({
                    "ruleId": f.rule,
                    "level": f.severity.sarif_level(),
                    "message": { "text": f.message },
                    "locations": [{
                        "physicalLocation": {
                            "artifactLocation": { "uri": f.path },
                            "region": { "startLine": f.line }
                        }
                    }]
                })
            })
            .collect();
        serde_json::json!({
            "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
            "version": "2.1.0",
            "runs": [{
                "tool": {
                    "driver": {
                        "name": tool_name,
                        "informationUri": "https://github.com/Asketisch/ZhuYe"
                    }
                },
                "results": results
            }]
        })
    }
}

/// 硬编码密钥 matcher(简化:按模式名匹配行)。
///
/// 真实实现会用 `zhuye-sanitize` 的 pattern;这里给逻辑层。
pub fn match_hardcoded_secret(path: &str, content: &str) -> Vec<SecurityFinding> {
    const MARKERS: [(&str, &str); 4] = [
        ("sk-ant-", "hardcoded-secret"),
        ("AKIA", "hardcoded-aws-key"),
        ("ghp_", "hardcoded-github-token"),
        ("-----BEGIN PRIVATE KEY", "hardcoded-private-key"),
    ];
    let mut out = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        for (marker, rule) in MARKERS {
            if line.contains(marker) {
                out.push(SecurityFinding::matched(
                    rule,
                    Severity::Error,
                    path,
                    line_no + 1,
                    format!("疑似硬编码凭据({marker})"),
                ));
            }
        }
    }
    out
}

/// 危险命令 matcher。
pub fn match_dangerous_command(path: &str, content: &str) -> Vec<SecurityFinding> {
    const PATTERNS: [(&str, &str); 3] = [
        ("rm -rf /", "dangerous-command"),
        ("curl | sh", "remote-code-execution"),
        ("eval $(", "dynamic-execution"),
    ];
    let mut out = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        for (pattern, rule) in PATTERNS {
            if line.contains(pattern) {
                out.push(SecurityFinding::matched(
                    rule,
                    Severity::Warning,
                    path,
                    line_no + 1,
                    format!("危险命令模式({pattern})"),
                ));
            }
        }
    }
    out
}

/// 投票裁决。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Vote {
    Yes,
    No,
    Abstain,
}

/// 共识投票结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsensusVote {
    /// 各模型投票。
    pub votes: Vec<Vote>,
    /// 最终裁决(多数)。
    pub decision: bool,
    /// 是否达到共识阈值。
    pub consensus: bool,
}

impl ConsensusVote {
    /// 统计投票。`threshold` 是"需要多少比例赞成才算通过"。
    ///
    /// `Abstain` 不计入分母(弃权不影响结论)。
    pub fn tally(votes: Vec<Vote>, threshold: f64) -> ConsensusVote {
        let yes = votes.iter().filter(|v| **v == Vote::Yes).count();
        let no = votes.iter().filter(|v| **v == Vote::No).count();
        let total = yes + no;
        let decision = total > 0 && (yes as f64 / total as f64) >= threshold;
        // 共识:没有弃权,或赞成/反对差异足够明显。
        let consensus =
            total > 0 && (yes == 0 || no == 0 || (yes.max(no) as f64 / total as f64) >= 0.8);
        ConsensusVote {
            votes,
            decision,
            consensus,
        }
    }

    /// 赞成票数。
    pub fn yes_count(&self) -> usize {
        self.votes.iter().filter(|v| **v == Vote::Yes).count()
    }

    /// 反对票数。
    pub fn no_count(&self) -> usize {
        self.votes.iter().filter(|v| **v == Vote::No).count()
    }

    /// 弃权票数。
    pub fn abstain_count(&self) -> usize {
        self.votes.iter().filter(|v| **v == Vote::Abstain).count()
    }

    /// 人读摘要。
    pub fn summary(&self) -> String {
        format!(
            "投票 {} 赞成 / {} 反对 / {} 弃权 → {}({})",
            self.yes_count(),
            self.no_count(),
            self.abstain_count(),
            if self.decision { "通过" } else { "否决" },
            if self.consensus {
                "有共识"
            } else {
                "无共识"
            }
        )
    }
}

/// 就绪度评分(F60 置信度门)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadinessScore {
    /// 各项得分(0.0–1.0)。
    pub criteria: BTreeMap<String, f64>,
    /// 权重(同 keys)。
    pub weights: BTreeMap<String, f64>,
}

impl ReadinessScore {
    pub fn new() -> ReadinessScore {
        ReadinessScore {
            criteria: BTreeMap::new(),
            weights: BTreeMap::new(),
        }
    }

    /// 加一项(得分 + 权重)。
    pub fn with(mut self, name: impl Into<String>, score: f64, weight: f64) -> ReadinessScore {
        let name = name.into();
        self.criteria.insert(name.clone(), score.clamp(0.0, 1.0));
        self.weights.insert(name, weight);
        self
    }

    /// 加权总分。
    pub fn total(&self) -> f64 {
        let mut weighted = 0.0;
        let mut total_weight = 0.0;
        for (name, score) in &self.criteria {
            let weight = self.weights.get(name).copied().unwrap_or(1.0);
            weighted += score * weight;
            total_weight += weight;
        }
        if total_weight <= 0.0 {
            0.0
        } else {
            weighted / total_weight
        }
    }

    /// 是否达到门槛。
    pub fn passes(&self, threshold: f64) -> bool {
        self.total() >= threshold
    }

    /// 最低分项(改进优先级)。
    pub fn weakest(&self) -> Option<(&str, f64)> {
        self.criteria
            .iter()
            .min_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(k, v)| (k.as_str(), *v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardcoded_secret_detected() {
        let content = "line1\nkey = sk-ant-api03-abcdefghij\nline3";
        let findings = match_hardcoded_secret("src/lib.rs", content);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, "hardcoded-secret");
        assert_eq!(findings[0].line, 2);
        assert_eq!(findings[0].severity, Severity::Error);
    }

    #[test]
    fn multiple_secret_markers() {
        let content = "AKIAIOSFODNN7EXAMPLE\nghp_abcdefghijklmnopqrstuvwxyz0123";
        let findings = match_hardcoded_secret("a.rs", content);
        assert_eq!(findings.len(), 2);
        assert!(findings.iter().any(|f| f.rule == "hardcoded-aws-key"));
        assert!(findings.iter().any(|f| f.rule == "hardcoded-github-token"));
    }

    #[test]
    fn dangerous_command_detected() {
        let content = "safe\nrm -rf / --no-preserve-root";
        let findings = match_dangerous_command("script.sh", content);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, "dangerous-command");
        assert_eq!(findings[0].severity, Severity::Warning);
    }

    #[test]
    fn scan_dedups_findings() {
        let mut scan = SecurityScan::new();
        let f = SecurityFinding::matched("r", Severity::Error, "p", 1, "m");
        scan.add_matched(f.clone());
        scan.add_matched(f);
        assert_eq!(scan.len(), 1);
    }

    #[test]
    fn triage_dismisses_false_positive() {
        let mut scan = SecurityScan::new();
        scan.add_matched(SecurityFinding::matched(
            "hardcoded-secret",
            Severity::Error,
            "test.rs",
            10,
            "test fixture",
        ));
        let id = scan.all()[0].id.clone();
        assert!(scan.triage(&id, false, "测试夹具里的假 key"));
        assert!(scan.is_clean(), "驳回后不阻断");
        assert!(scan.reportable().is_empty());
    }

    #[test]
    fn triage_confirms_real_issue() {
        let mut scan = SecurityScan::new();
        scan.add_matched(SecurityFinding::matched(
            "hardcoded-secret",
            Severity::Error,
            "src/lib.rs",
            10,
            "real key",
        ));
        let id = scan.all()[0].id.clone();
        assert!(scan.triage(&id, true, "真实凭据"));
        assert!(!scan.is_clean());
        assert_eq!(scan.blocking().len(), 1);
    }

    #[test]
    fn downgrade_reduces_severity() {
        let mut scan = SecurityScan::new();
        scan.add_matched(SecurityFinding::matched(
            "dangerous-command",
            Severity::Error,
            "a.sh",
            1,
            "m",
        ));
        let id = scan.all()[0].id.clone();
        assert!(scan.downgrade(&id, Severity::Note));
        assert!(scan.is_clean(), "降级到 Note 不阻断");
        assert_eq!(scan.reportable().len(), 1);
    }

    #[test]
    fn triage_unknown_id_false() {
        let mut scan = SecurityScan::new();
        assert!(!scan.triage("ghost", true, "x"));
        assert!(!scan.downgrade("ghost", Severity::Note));
    }

    #[test]
    fn counts_by_severity() {
        let mut scan = SecurityScan::new();
        scan.add_matched(SecurityFinding::matched("a", Severity::Error, "p", 1, "m"));
        scan.add_matched(SecurityFinding::matched(
            "b",
            Severity::Warning,
            "p",
            2,
            "m",
        ));
        scan.add_matched(SecurityFinding::matched(
            "c",
            Severity::Warning,
            "p",
            3,
            "m",
        ));
        let counts = scan.counts();
        assert_eq!(counts["error"], 1);
        assert_eq!(counts["warning"], 2);
    }

    #[test]
    fn sarif_export_shape() {
        let mut scan = SecurityScan::new();
        scan.add_matched(SecurityFinding::matched(
            "hardcoded-secret",
            Severity::Error,
            "src/lib.rs",
            42,
            "key found",
        ));
        let sarif = scan.to_sarif("zhuye-security-scan");
        assert_eq!(sarif["version"], "2.1.0");
        assert_eq!(
            sarif["runs"][0]["tool"]["driver"]["name"],
            "zhuye-security-scan"
        );
        let results = sarif["runs"][0]["results"].as_array().expect("results");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["ruleId"], "hardcoded-secret");
        assert_eq!(results[0]["level"], "error");
        assert_eq!(
            results[0]["locations"][0]["physicalLocation"]["region"]["startLine"],
            42
        );
    }

    #[test]
    fn sarif_excludes_dismissed() {
        let mut scan = SecurityScan::new();
        scan.add_matched(SecurityFinding::matched("a", Severity::Error, "p", 1, "m"));
        let id = scan.all()[0].id.clone();
        scan.triage(&id, false, "fp");
        let sarif = scan.to_sarif("t");
        assert!(sarif["runs"][0]["results"].as_array().unwrap().is_empty());
    }

    #[test]
    fn consensus_majority_passes() {
        let vote = ConsensusVote::tally(vec![Vote::Yes, Vote::Yes, Vote::No], 0.6);
        assert!(vote.decision, "2/3 ≥ 0.6");
        assert!(!vote.consensus, "2/3 = 0.67 < 0.8 → 无共识(但多数通过)");
    }

    #[test]
    fn consensus_threshold_blocks() {
        let vote = ConsensusVote::tally(vec![Vote::Yes, Vote::No, Vote::No], 0.6);
        assert!(!vote.decision, "1/3 < 0.6");
    }

    #[test]
    fn consensus_abstain_excluded() {
        let vote = ConsensusVote::tally(vec![Vote::Yes, Vote::Yes, Vote::Abstain], 0.6);
        assert!(vote.decision, "2/2 赞成(弃权不计)");
        assert_eq!(vote.abstain_count(), 1);
        assert_eq!(vote.yes_count(), 2);
    }

    #[test]
    fn consensus_all_abstain() {
        let vote = ConsensusVote::tally(vec![Vote::Abstain, Vote::Abstain], 0.5);
        assert!(!vote.decision, "全弃权不通过");
        assert!(!vote.consensus);
    }

    #[test]
    fn consensus_unanimous() {
        let vote = ConsensusVote::tally(vec![Vote::Yes, Vote::Yes, Vote::Yes], 0.5);
        assert!(vote.decision);
        assert!(vote.consensus, "全票应有共识");
        assert!(vote.summary().contains("有共识"));
    }

    #[test]
    fn readiness_weighted_total() {
        let score = ReadinessScore::new()
            .with("tests", 1.0, 2.0)
            .with("docs", 0.5, 1.0);
        // (1.0*2 + 0.5*1) / 3 = 0.8333
        assert!((score.total() - 0.8333).abs() < 0.001);
        assert!(score.passes(0.8));
        assert!(!score.passes(0.9));
    }

    #[test]
    fn readiness_weakest() {
        let score = ReadinessScore::new()
            .with("tests", 0.9, 1.0)
            .with("docs", 0.3, 1.0);
        let (name, value) = score.weakest().expect("weakest");
        assert_eq!(name, "docs");
        assert!((value - 0.3).abs() < 1e-9);
    }

    #[test]
    fn readiness_empty_is_zero() {
        let score = ReadinessScore::new();
        assert_eq!(score.total(), 0.0);
        assert!(!score.passes(0.1));
        assert!(score.weakest().is_none());
    }

    #[test]
    fn finding_serializes() {
        let f = SecurityFinding::matched("r", Severity::Error, "p", 1, "m");
        let json = serde_json::to_string(&f).expect("serialize");
        let back: SecurityFinding = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, f);
    }

    #[test]
    fn consensus_vote_serializes() {
        let v = ConsensusVote::tally(vec![Vote::Yes, Vote::No], 0.5);
        let json = serde_json::to_string(&v).expect("serialize");
        let back: ConsensusVote = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, v);
    }
}
