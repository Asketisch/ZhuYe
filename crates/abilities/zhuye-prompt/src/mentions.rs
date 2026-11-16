//! # Composer mention / picker(F69)
//!
//! 输入框里 `@文件` / `#工单` / `!PR` 自动带上引用上下文,省得 agent
//! 每次自己找文件。
//!
//! ## 三类 token
//!
//! | 前缀 | 含义 | 解析方式 |
//! |---|---|---|
//! | `@path` | 文件 / 目录 | 本地 glob 匹配 |
//! | `#123` | issue | `gh issue view`(尽力而为) |
//! | `!123` | PR | `gh pr view`(尽力而为) |
//!
//! ## 为什么解析要在「发送前」而不是「发送后」
//!
//! 发送后解析意味着模型要自己找文件 —— 那正是 F69 要消除的成本。
//! 发送前把 `@src/lib.rs` 展开成文件内容注入,模型直接读到。
//!
//! ## 为什么 `#` / `!` 是「尽力而为」
//!
//! issue / PR 需要 `gh` CLI + 网络 + 仓库权限。任何一环缺失时,mention
//! 应**降级为纯文本**(保留 `#123` 字样)而不是让发送失败。

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// mention 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MentionKind {
    /// `@path` —— 文件 / 目录。
    File,
    /// `#N` —— issue。
    Issue,
    /// `!N` —— PR。
    PullRequest,
    /// `@mcp:resource` —— MCP 资源。
    McpResource,
}

impl MentionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MentionKind::File => "file",
            MentionKind::Issue => "issue",
            MentionKind::PullRequest => "pull_request",
            MentionKind::McpResource => "mcp_resource",
        }
    }
}

/// 一个 mention token。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mention {
    pub kind: MentionKind,
    /// 原文(含前缀)。
    pub raw: String,
    /// 解析后的目标(`src/lib.rs` / `123` / `mcp:fs://x`)。
    pub target: String,
    /// 在原文中的字节区间。
    pub start: usize,
    pub end: usize,
}

impl Mention {
    /// 是否成功解析(文件存在 / issue 可取)。
    pub fn is_resolved(&self) -> bool {
        !self.target.trim().is_empty()
    }
}

/// 解析输入框文本里的全部 mention。
///
/// 规则:
///
/// - `@` 后紧跟非空白、非 `@#!` 的字符序列 → 文件;
/// - `@mcp:` 开头 → MCP 资源;
/// - `#` 后跟数字 → issue;
/// - `!` 后跟数字 → PR;
/// - 其它情况不算 mention(避免把邮箱 `a@b.com` 当文件)。
pub fn parse_mentions(text: &str) -> Vec<Mention> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        // 邮箱保护:`a@b.com` 的 `@` 前面紧邻字母数字 → 不算 mention。
        let preceded_by_word =
            i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        match b {
            b'@' if !preceded_by_word => {
                let start = i;
                let mut j = i + 1;
                while j < bytes.len()
                    && !bytes[j].is_ascii_whitespace()
                    && !matches!(bytes[j], b'@' | b'#' | b'!')
                {
                    j += 1;
                }
                if j > i + 1 {
                    let target = &text[i + 1..j];
                    let kind = if target.starts_with("mcp:") {
                        MentionKind::McpResource
                    } else {
                        MentionKind::File
                    };
                    out.push(Mention {
                        kind,
                        raw: text[start..j].to_string(),
                        target: target.to_string(),
                        start,
                        end: j,
                    });
                }
                i = j.max(i + 1);
            }
            b'#' | b'!' if !preceded_by_word => {
                let start = i;
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j > i + 1 {
                    let kind = if b == b'#' {
                        MentionKind::Issue
                    } else {
                        MentionKind::PullRequest
                    };
                    out.push(Mention {
                        kind,
                        raw: text[start..j].to_string(),
                        target: text[i + 1..j].to_string(),
                        start,
                        end: j,
                    });
                }
                i = j.max(i + 1);
            }
            _ => i += 1,
        }
    }
    out
}

/// picker 候选(输入 `@` 后的补全列表)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickerCandidate {
    /// 显示文本。
    pub label: String,
    /// 插入文本(含前缀)。
    pub insert: String,
    /// 类型。
    pub kind: MentionKind,
    /// 次要说明(路径 / 标题)。
    #[serde(default)]
    pub detail: String,
}

/// 按前缀过滤候选。
pub fn filter_candidates<'a>(
    candidates: &'a [PickerCandidate],
    query: &str,
    limit: usize,
) -> Vec<&'a PickerCandidate> {
    let q = query.to_lowercase();
    let mut hits: Vec<&PickerCandidate> = candidates
        .iter()
        .filter(|c| c.label.to_lowercase().contains(&q) || c.detail.to_lowercase().contains(&q))
        .collect();
    // 前缀匹配优先(更符合直觉)。
    hits.sort_by_key(|c| {
        let label = c.label.to_lowercase();
        (!label.starts_with(&q), label.clone())
    });
    hits.truncate(limit);
    hits
}

/// 从文件列表构造候选(按路径排序,上限截断)。
pub fn file_candidates(paths: &[String], limit: usize) -> Vec<PickerCandidate> {
    let mut sorted: Vec<&String> = paths.iter().collect();
    sorted.sort();
    sorted
        .into_iter()
        .take(limit)
        .map(|p| PickerCandidate {
            label: p.rsplit('/').next().unwrap_or(p).to_string(),
            insert: format!("@{p}"),
            kind: MentionKind::File,
            detail: p.clone(),
        })
        .collect()
}

/// 上下文装配结果。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssembledContext {
    /// 装配好的上下文块(注入到消息前)。
    pub blocks: Vec<ContextBlock>,
    /// 未能解析的 mention(降级为纯文本)。
    pub unresolved: Vec<String>,
}

/// 一个上下文块。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBlock {
    pub kind: MentionKind,
    pub target: String,
    /// 内容(文件正文 / issue 标题 + 正文)。
    pub content: String,
    /// 是否被截断。
    #[serde(default)]
    pub truncated: bool,
}

impl AssembledContext {
    /// 是否有内容。
    pub fn has_blocks(&self) -> bool {
        !self.blocks.is_empty()
    }

    /// 是否有未解析项。
    pub fn has_unresolved(&self) -> bool {
        !self.unresolved.is_empty()
    }

    /// 渲染成注入文本。
    pub fn render(&self) -> String {
        if self.blocks.is_empty() {
            return String::new();
        }
        let mut out = String::from("<referenced-context>\n");
        for block in &self.blocks {
            out.push_str(&format!("\n### {} {}\n", block.kind.as_str(), block.target));
            out.push_str(&block.content);
            if block.truncated {
                out.push_str("\n…(内容已截断)");
            }
            out.push('\n');
        }
        out.push_str("</referenced-context>\n");
        out
    }
}

/// 装配上下文。
///
/// `read_file` / `fetch_issue` / `fetch_pr` 是注入的解析回调 ——
/// 返回 `None` 表示不可用(降级为纯文本)。
pub fn assemble_context<F, I, P>(
    mentions: &[Mention],
    max_file_bytes: usize,
    read_file: F,
    fetch_issue: I,
    fetch_pr: P,
) -> AssembledContext
where
    F: Fn(&str) -> Option<String>,
    I: Fn(&str) -> Option<String>,
    P: Fn(&str) -> Option<String>,
{
    let mut ctx = AssembledContext::default();
    let mut seen: BTreeSet<(MentionKind, String)> = BTreeSet::new();
    for mention in mentions {
        // 去重:同一目标只装配一次。
        if !seen.insert((mention.kind, mention.target.clone())) {
            continue;
        }
        let resolved = match mention.kind {
            MentionKind::File => read_file(&mention.target),
            MentionKind::Issue => fetch_issue(&mention.target),
            MentionKind::PullRequest => fetch_pr(&mention.target),
            MentionKind::McpResource => None, // MCP 资源由上层解析
        };
        match resolved {
            Some(content) => {
                let (content, truncated) = if content.len() > max_file_bytes {
                    (
                        content.chars().take(max_file_bytes).collect::<String>(),
                        true,
                    )
                } else {
                    (content, false)
                };
                ctx.blocks.push(ContextBlock {
                    kind: mention.kind,
                    target: mention.target.clone(),
                    content,
                    truncated,
                });
            }
            None => ctx.unresolved.push(mention.raw.clone()),
        }
    }
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_file_mention() {
        let mentions = parse_mentions("看下 @src/lib.rs 这个文件");
        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].kind, MentionKind::File);
        assert_eq!(mentions[0].target, "src/lib.rs");
    }

    #[test]
    fn parse_multiple_mentions() {
        let mentions = parse_mentions("@a.rs 和 @b.rs 还有 #42");
        assert_eq!(mentions.len(), 3);
        assert_eq!(mentions[0].target, "a.rs");
        assert_eq!(mentions[1].target, "b.rs");
        assert_eq!(mentions[2].kind, MentionKind::Issue);
        assert_eq!(mentions[2].target, "42");
    }

    #[test]
    fn parse_pr_mention() {
        let mentions = parse_mentions("修一下 !123");
        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].kind, MentionKind::PullRequest);
        assert_eq!(mentions[0].target, "123");
    }

    #[test]
    fn parse_mcp_resource() {
        let mentions = parse_mentions("@mcp:fs://notes/today");
        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].kind, MentionKind::McpResource);
    }

    #[test]
    fn email_not_treated_as_mention() {
        let mentions = parse_mentions("联系 alice@example.com 询问");
        assert!(mentions.is_empty(), "邮箱不应被当 mention:{mentions:?}");
    }

    #[test]
    fn bare_at_sign_ignored() {
        let mentions = parse_mentions("@ 没有目标");
        assert!(mentions.is_empty());
    }

    #[test]
    fn hash_without_digits_ignored() {
        let mentions = parse_mentions("#标题 不是 issue");
        assert!(mentions.is_empty());
    }

    #[test]
    fn mention_byte_ranges() {
        let text = "前 @a.rs 后";
        let mentions = parse_mentions(text);
        let m = &mentions[0];
        assert_eq!(&text[m.start..m.end], "@a.rs");
    }

    #[test]
    fn filter_candidates_by_query() {
        let candidates = vec![
            PickerCandidate {
                label: "lib.rs".into(),
                insert: "@src/lib.rs".into(),
                kind: MentionKind::File,
                detail: "src/lib.rs".into(),
            },
            PickerCandidate {
                label: "main.rs".into(),
                insert: "@src/main.rs".into(),
                kind: MentionKind::File,
                detail: "src/main.rs".into(),
            },
        ];
        let hits = filter_candidates(&candidates, "lib", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].label, "lib.rs");
    }

    #[test]
    fn filter_candidates_prefix_priority() {
        let candidates = vec![
            PickerCandidate {
                label: "xlib.rs".into(),
                insert: "@xlib.rs".into(),
                kind: MentionKind::File,
                detail: "".into(),
            },
            PickerCandidate {
                label: "lib.rs".into(),
                insert: "@lib.rs".into(),
                kind: MentionKind::File,
                detail: "".into(),
            },
        ];
        let hits = filter_candidates(&candidates, "lib", 10);
        assert_eq!(hits[0].label, "lib.rs", "前缀匹配优先");
    }

    #[test]
    fn filter_candidates_limit() {
        let candidates: Vec<PickerCandidate> = (0..10)
            .map(|i| PickerCandidate {
                label: format!("f{i}.rs"),
                insert: format!("@f{i}.rs"),
                kind: MentionKind::File,
                detail: String::new(),
            })
            .collect();
        assert_eq!(filter_candidates(&candidates, "f", 3).len(), 3);
    }

    #[test]
    fn file_candidates_sorted() {
        let paths = vec!["z.rs".to_string(), "a.rs".to_string()];
        let cands = file_candidates(&paths, 10);
        assert_eq!(cands[0].label, "a.rs");
        assert_eq!(cands[0].insert, "@a.rs");
    }

    #[test]
    fn assemble_resolves_files() {
        let mentions = parse_mentions("@a.rs");
        let ctx = assemble_context(
            &mentions,
            1000,
            |path| {
                if path == "a.rs" {
                    Some("fn main() {}".to_string())
                } else {
                    None
                }
            },
            |_| None,
            |_| None,
        );
        assert!(ctx.has_blocks());
        assert!(!ctx.has_unresolved());
        let rendered = ctx.render();
        assert!(rendered.contains("<referenced-context>"), "{rendered}");
        assert!(rendered.contains("fn main()"), "{rendered}");
    }

    #[test]
    fn assemble_unresolved_degrades() {
        let mentions = parse_mentions("@ghost.rs");
        let ctx = assemble_context(&mentions, 1000, |_| None, |_| None, |_| None);
        assert!(!ctx.has_blocks());
        assert!(ctx.has_unresolved());
        assert_eq!(ctx.unresolved[0], "@ghost.rs");
    }

    #[test]
    fn assemble_truncates_large_file() {
        let mentions = parse_mentions("@big.rs");
        let big = "x".repeat(500);
        let ctx = assemble_context(&mentions, 100, |_| Some(big.clone()), |_| None, |_| None);
        let block = &ctx.blocks[0];
        assert!(block.truncated);
        assert!(block.content.chars().count() <= 100);
        assert!(ctx.render().contains("已截断"));
    }

    #[test]
    fn assemble_dedups_same_target() {
        let mentions = parse_mentions("@a.rs @a.rs");
        let ctx = assemble_context(
            &mentions,
            1000,
            |_| Some("content".to_string()),
            |_| None,
            |_| None,
        );
        assert_eq!(ctx.blocks.len(), 1, "同目标只装配一次");
    }

    #[test]
    fn assemble_issue_and_pr() {
        let mentions = parse_mentions("#1 !2");
        let ctx = assemble_context(
            &mentions,
            1000,
            |_| None,
            |n| Some(format!("issue {n}")),
            |n| Some(format!("pr {n}")),
        );
        assert_eq!(ctx.blocks.len(), 2);
        assert!(ctx.blocks.iter().any(|b| b.content == "issue 1"));
        assert!(ctx.blocks.iter().any(|b| b.content == "pr 2"));
    }

    #[test]
    fn mention_serializes() {
        let m = Mention {
            kind: MentionKind::File,
            raw: "@a.rs".into(),
            target: "a.rs".into(),
            start: 0,
            end: 5,
        };
        let json = serde_json::to_string(&m).expect("serialize");
        let back: Mention = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, m);
    }
}
