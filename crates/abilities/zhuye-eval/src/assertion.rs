//! # 确定性断言库(F20)
//!
//! 每条断言都是**纯函数**:输入 agent 输出(文本 + 工具调用序列),
//! 输出 pass/fail + 诊断。无模型、无网络、无副作用 —— 这是评测可重复的
//! 前提。
//!
//! ## 为什么不用"打分"而用"断言"
//!
//! 打分(0–100)看起来信息量大,但阈值是拍脑袋定的,改一次阈值历史
//! 数据全废。断言是**可证伪的**:「必须包含 `Result::Ok`」这种要求
//! 要么满足要么不满足,回归时不会因为阈值漂移产生假警报。
//!
//! 需要"质量分"的场景交给 [`crate::judge`],而不是把断言做成打分。

use serde::{Deserialize, Serialize};

use crate::EvalError;

/// 单条断言。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Assertion {
    /// 输出必须包含该子串(大小写敏感)。
    Contains { text: String },
    /// 输出必须**不**包含该子串。
    NotContains { text: String },
    /// 输出必须匹配该正则。
    Matches { pattern: String },
    /// 输出必须等于该字符串(去首尾空白后)。
    Equals { text: String },
    /// 输出长度下限(字符数)。
    MinChars { n: usize },
    /// 输出长度上限(字符数)。
    MaxChars { n: usize },
    /// 必须调用了这些工具(按顺序为子序列)。
    ToolSequence { tools: Vec<String> },
    /// 不得调用任何工具。
    NoTools,
    /// 工具调用次数上限。
    MaxToolCalls { n: usize },
}

impl Assertion {
    /// 断言类型的人读名(报告用)。
    pub fn kind_label(&self) -> &'static str {
        match self {
            Assertion::Contains { .. } => "contains",
            Assertion::NotContains { .. } => "not_contains",
            Assertion::Matches { .. } => "matches",
            Assertion::Equals { .. } => "equals",
            Assertion::MinChars { .. } => "min_chars",
            Assertion::MaxChars { .. } => "max_chars",
            Assertion::ToolSequence { .. } => "tool_sequence",
            Assertion::NoTools => "no_tools",
            Assertion::MaxToolCalls { .. } => "max_tool_calls",
        }
    }

    /// 执行断言。
    ///
    /// `text` 是 agent 最终输出;`tools` 是按调用顺序的工具名。
    pub fn evaluate(&self, text: &str, tools: &[String]) -> AssertionResult {
        match self {
            Assertion::Contains { text: needle } => {
                let ok = text.contains(needle.as_str());
                AssertionResult {
                    kind: self.kind_label().to_string(),
                    passed: ok,
                    detail: if ok {
                        format!("包含 {needle:?}")
                    } else {
                        format!("缺少 {needle:?}")
                    },
                }
            }
            Assertion::NotContains { text: needle } => {
                let ok = !text.contains(needle.as_str());
                AssertionResult {
                    kind: self.kind_label().to_string(),
                    passed: ok,
                    detail: if ok {
                        format!("未包含 {needle:?}")
                    } else {
                        format!("不应包含 {needle:?}")
                    },
                }
            }
            Assertion::Matches { pattern } => {
                // 正则编译失败 → 断言本身非法,记 fail 并给出原因。
                match regex_lite_match(pattern, text) {
                    Ok(true) => AssertionResult {
                        kind: self.kind_label().to_string(),
                        passed: true,
                        detail: format!("匹配 /{pattern}/"),
                    },
                    Ok(false) => AssertionResult {
                        kind: self.kind_label().to_string(),
                        passed: false,
                        detail: format!("不匹配 /{pattern}/"),
                    },
                    Err(e) => AssertionResult {
                        kind: self.kind_label().to_string(),
                        passed: false,
                        detail: format!("非法正则 /{pattern}/: {e}"),
                    },
                }
            }
            Assertion::Equals { text: expected } => {
                let ok = text.trim() == expected.trim();
                AssertionResult {
                    kind: self.kind_label().to_string(),
                    passed: ok,
                    detail: if ok {
                        "内容相等".to_string()
                    } else {
                        format!(
                            "期望 {:?},实际 {:?}",
                            expected.trim(),
                            truncate(text.trim(), 80)
                        )
                    },
                }
            }
            Assertion::MinChars { n } => {
                let len = text.chars().count();
                AssertionResult {
                    kind: self.kind_label().to_string(),
                    passed: len >= *n,
                    detail: format!("{len} 字符(下限 {n})"),
                }
            }
            Assertion::MaxChars { n } => {
                let len = text.chars().count();
                AssertionResult {
                    kind: self.kind_label().to_string(),
                    passed: len <= *n,
                    detail: format!("{len} 字符(上限 {n})"),
                }
            }
            Assertion::ToolSequence { tools: expected } => {
                let ok = is_subsequence(expected, tools);
                AssertionResult {
                    kind: self.kind_label().to_string(),
                    passed: ok,
                    detail: if ok {
                        format!("工具序列含 {:?}", expected)
                    } else {
                        format!("工具序列 {:?} 不含子序列 {:?}", tools, expected)
                    },
                }
            }
            Assertion::NoTools => {
                let ok = tools.is_empty();
                AssertionResult {
                    kind: self.kind_label().to_string(),
                    passed: ok,
                    detail: if ok {
                        "未调用工具".to_string()
                    } else {
                        format!("调用了 {} 个工具:{tools:?}", tools.len())
                    },
                }
            }
            Assertion::MaxToolCalls { n } => {
                let ok = tools.len() <= *n;
                AssertionResult {
                    kind: self.kind_label().to_string(),
                    passed: ok,
                    detail: format!("{} 次工具调用(上限 {n})", tools.len()),
                }
            }
        }
    }

    /// 构造期校验(未知字段 / 空模式)。
    pub fn validate(&self) -> Result<(), EvalError> {
        match self {
            Assertion::Matches { pattern } => regex_lite_match(pattern, "")
                .map(|_| ())
                .map_err(|e| EvalError::InvalidAssertion(format!("matches /{pattern}/: {e}"))),
            Assertion::Contains { text } | Assertion::NotContains { text } if text.is_empty() => {
                Err(EvalError::InvalidAssertion(
                    "contains/not_contains 的 text 不能为空".to_string(),
                ))
            }
            Assertion::ToolSequence { tools } if tools.is_empty() => Err(
                EvalError::InvalidAssertion("tool_sequence 不能为空列表".to_string()),
            ),
            _ => Ok(()),
        }
    }
}

/// 单条断言结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssertionResult {
    /// 断言类型标签(owned,便于 serde 往返)。
    pub kind: String,
    pub passed: bool,
    pub detail: String,
}

/// 极简正则匹配。
///
/// 不引入 `regex` 依赖(本 crate 保持叶级):支持 `^` / `$` / `.*` / 字面量
/// 与 `\d` / `\w` / `\s`。复杂正则请改用 `Contains` 或把断言交给 judge。
fn regex_lite_match(pattern: &str, text: &str) -> Result<bool, String> {
    if pattern.is_empty() {
        return Err("empty pattern".to_string());
    }
    let mut pat = pattern;
    let anchored_start = pat.starts_with('^');
    if anchored_start {
        pat = &pat[1..];
    }
    let anchored_end = pat.ends_with('$');
    if anchored_end {
        pat = &pat[..pat.len() - 1];
    }
    // 支持唯一通配 `.*`
    if let Some((left, right)) = pat.split_once(".*") {
        let left_hit = if left.is_empty() {
            Some(0)
        } else {
            find_literal(text, left)
        };
        let Some(l) = left_hit else {
            return Ok(false);
        };
        if anchored_start && l != 0 {
            return Ok(false);
        }
        let rest = &text[l + left.len()..];
        if right.is_empty() {
            // `prefix.*`:`$` 锚定要求后面不再有内容。
            return Ok(!anchored_end || rest.is_empty());
        }
        if anchored_end {
            // `.*suffix$`:必须以 suffix 结尾。
            return Ok(rest.ends_with(right));
        }
        return Ok(find_literal(rest, right).is_some());
    }
    // 无通配:纯字面量(转义字符按字面处理)。
    let needle = unescape(pat);
    let found = find_literal(text, &needle);
    match (anchored_start, anchored_end) {
        (true, true) => Ok(text == needle),
        (true, false) => Ok(text.starts_with(&needle)),
        (false, true) => Ok(text.ends_with(&needle)),
        (false, false) => Ok(found.is_some()),
    }
}

fn find_literal(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.find(needle)
}

/// 处理 `\d` / `\w` / `\s` 的极简转义(其余按字面)。
fn unescape(pat: &str) -> String {
    let mut out = String::with_capacity(pat.len());
    let mut chars = pat.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('d') => out.push_str("0"),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `needle` 是否是 `haystack` 的子序列(保持顺序,允许中间插别的)。
fn is_subsequence(needle: &[String], haystack: &[String]) -> bool {
    let mut it = haystack.iter();
    needle.iter().all(|want| it.any(|got| got == want))
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn contains_and_not_contains() {
        let a = Assertion::Contains { text: "Ok".into() };
        assert!(a.evaluate("Result::Ok(1)", &[]).passed);
        assert!(!a.evaluate("Err", &[]).passed);

        let n = Assertion::NotContains {
            text: "panic".into(),
        };
        assert!(n.evaluate("all good", &[]).passed);
        assert!(!n.evaluate("will panic", &[]).passed);
    }

    #[test]
    fn equals_trims_whitespace() {
        let a = Assertion::Equals {
            text: "hello".into(),
        };
        assert!(a.evaluate("  hello\n", &[]).passed);
        assert!(!a.evaluate("hello world", &[]).passed);
    }

    #[test]
    fn char_bounds() {
        assert!(Assertion::MinChars { n: 3 }.evaluate("abc", &[]).passed);
        assert!(!Assertion::MinChars { n: 4 }.evaluate("abc", &[]).passed);
        assert!(Assertion::MaxChars { n: 3 }.evaluate("abc", &[]).passed);
        assert!(!Assertion::MaxChars { n: 2 }.evaluate("abc", &[]).passed);
    }

    #[test]
    fn tool_sequence_is_subsequence() {
        let a = Assertion::ToolSequence {
            tools: tools(&["read", "edit"]),
        };
        assert!(a.evaluate("", &tools(&["read", "grep", "edit"])).passed);
        assert!(!a.evaluate("", &tools(&["edit", "read"])).passed);
        assert!(!a.evaluate("", &tools(&["read"])).passed);
    }

    #[test]
    fn no_tools_and_max_calls() {
        assert!(Assertion::NoTools.evaluate("", &[]).passed);
        assert!(!Assertion::NoTools.evaluate("", &tools(&["bash"])).passed);
        assert!(
            Assertion::MaxToolCalls { n: 2 }
                .evaluate("", &tools(&["a", "b"]))
                .passed
        );
        assert!(
            !Assertion::MaxToolCalls { n: 1 }
                .evaluate("", &tools(&["a", "b"]))
                .passed
        );
    }

    #[test]
    fn regex_lite_basics() {
        assert!(regex_lite_match("^Result", "Result::Ok").unwrap());
        assert!(!regex_lite_match("^Result", "xResult").unwrap());
        assert!(regex_lite_match("Ok$", "Result::Ok").unwrap());
        assert!(regex_lite_match("a.*b", "aXXXb").unwrap());
        assert!(!regex_lite_match("a.*b", "bXXXa").unwrap());
        assert!(regex_lite_match("^a.*z$", "abcz").unwrap());
        // `abczz` 也以 z 结尾 → `^a.*z$` 匹配(`.*` 吞掉倒数第二个 z)。
        assert!(regex_lite_match("^a.*z$", "abczz").unwrap());
        // 不以 z 结尾 → 不匹配。
        assert!(!regex_lite_match("^a.*z$", "abczx").unwrap());
    }

    #[test]
    fn matches_assertion_reports_bad_regex() {
        let a = Assertion::Matches { pattern: "".into() };
        let r = a.evaluate("x", &[]);
        assert!(!r.passed);
        assert!(r.detail.contains("非法正则"), "{}", r.detail);
    }

    #[test]
    fn validate_rejects_empty_fields() {
        assert!(
            Assertion::Contains {
                text: String::new()
            }
            .validate()
            .is_err()
        );
        assert!(
            Assertion::ToolSequence { tools: vec![] }
                .validate()
                .is_err()
        );
        assert!(
            Assertion::Matches { pattern: "".into() }
                .validate()
                .is_err()
        );
        assert!(Assertion::Contains { text: "x".into() }.validate().is_ok());
    }

    #[test]
    fn assertions_serialize_with_tag() {
        let json =
            serde_json::to_string(&Assertion::Contains { text: "x".into() }).expect("serialize");
        assert!(json.contains("\"kind\":\"contains\""), "{json}");
        let back: Assertion = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, Assertion::Contains { text: "x".into() });
    }

    #[test]
    fn unicode_char_counting() {
        // 字符数按 Unicode 计,不是字节数。
        assert!(Assertion::MaxChars { n: 3 }.evaluate("中文测", &[]).passed);
        assert!(!Assertion::MaxChars { n: 2 }.evaluate("中文测", &[]).passed);
    }
}
