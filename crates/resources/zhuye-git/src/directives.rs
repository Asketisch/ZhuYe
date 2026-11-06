//! assistant markdown 里的 git 指令解析 —— 移植 Reflect-TUI
//! `tui_core/git_action_directives.rs`(267 行,纯 std)。
//!
//! 两种指令语法(属性一律**空格分隔**,值可带双引号):
//!
//! 1. **`::git-<action>{attrs}`**(行内指令,从可见文本中**剥离**,一行
//!    可有多条):`git-stage` / `git-commit` / `git-create-branch` /
//!    `git-push` / `git-create-pr`。`cwd` 必填,缺 `cwd` / `branch` 的
//!    指令整条丢弃且不留可见残片;按指令去重。代码块、行内代码、
//!    链接、HTML 代码及转义内容中的字面示例不解析为动作。
//! 2. **`::code-comment{...}` / `:::code-comment{...}`**(1-3 个冒号,
//!    **重写成可见 markdown** 而非剥离):属性 `title` / `body` / `file` /
//!    `start` / `end` / `priority`(形如 `"P2"`),渲染成
//!    `- [P2] {title} — {file}:{start}` + 缩进正文,保留指令后的文本。
//!
//! 注意:模块内有两份手写属性解析器(`parse_attributes` 与
//! `parse_code_comment_attributes`),行为**有意不同**(后者经
//! `parse_quoted_value` 支持 `\"` 转义)—— 这是被测试锁定的差异,不要
//! "顺手统一"。

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::Path;

use pulldown_cmark::{Event, Parser, Tag};

/// assistant 输出里的 git 动作指令。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum GitActionDirective {
    Stage {
        cwd: String,
    },
    Commit {
        cwd: String,
    },
    CreateBranch {
        cwd: String,
        branch: String,
    },
    Push {
        cwd: String,
        branch: String,
    },
    CreatePr {
        cwd: String,
        branch: String,
        url: Option<String>,
        is_draft: bool,
    },
}

impl GitActionDirective {
    /// 该指令作用的 cwd(供调用方按目录归组)。
    pub fn cwd(&self) -> &str {
        match self {
            Self::Stage { cwd }
            | Self::Commit { cwd }
            | Self::CreateBranch { cwd, .. }
            | Self::Push { cwd, .. }
            | Self::CreatePr { cwd, .. } => cwd,
        }
    }
}

/// 解析产物:剥离指令后的可见 markdown + 结构化 git 动作列表。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedAssistantMarkdown {
    pub visible_markdown: String,
    pub git_actions: Vec<GitActionDirective>,
}

impl ParsedAssistantMarkdown {
    /// 最后一个 CreateBranch 指令的 cwd(UI「新建分支后刷新」用)。
    pub fn last_created_branch_cwd(&self) -> Option<&str> {
        self.git_actions.iter().rev().find_map(|a| match a {
            GitActionDirective::CreateBranch { cwd, .. } => Some(cwd.as_str()),
            _ => None,
        })
    }
}

/// 解析一段 assistant markdown:`cwd` 用于把 code-comment 的 `file`
/// 相对化(与 TUI 版一致,传当前工作目录)。
pub fn parse_assistant_markdown(markdown: &str, cwd: &Path) -> ParsedAssistantMarkdown {
    let mut git_actions: Vec<GitActionDirective> = Vec::new();
    let mut seen: HashSet<GitActionDirective> = HashSet::new();
    let mut visible_lines: Vec<String> = Vec::new();
    let (protected, text_ranges) = markdown_ranges(markdown);
    let mut offset = 0;

    for segment in markdown.split_inclusive('\n') {
        let line = match segment.strip_suffix('\n') {
            Some(line) => line.strip_suffix('\r').unwrap_or(line),
            None => segment,
        };
        let line_offset = offset;
        offset += segment.len();
        // 1) code-comment 指令:整行重写成可见 markdown(先于 git 指令,
        //    因为它的 body 里可能含 `::git-stage{...}` 字样)。
        let content_offset = line.len() - line.trim_start_matches([' ', '\t']).len();
        if range_overlaps(
            &text_ranges,
            line_offset + content_offset..line_offset + content_offset + 1,
        ) && !range_overlaps(
            &protected,
            line_offset + content_offset..line_offset + content_offset + 1,
        ) {
            let content = &line[content_offset..];
            let markers = content.bytes().take_while(|byte| *byte == b':').count();
            if (1..=3).contains(&markers) && content[markers..].starts_with("code-comment{") {
                let visible = rewrite_code_comment_line(line, cwd).map_or_else(
                    || line.to_string(),
                    |rewritten| rewritten.trim_end().to_string(),
                );
                visible_lines.push(visible);
                continue;
            }
        }
        // 2) git 动作指令:从行内剥离(一行可有多条)。
        let (visible_line, line_actions) =
            strip_line_directives(line, line_offset, &protected, &text_ranges);
        for action in line_actions {
            if seen.insert(action.clone()) {
                git_actions.push(action);
            }
        }
        if visible_line == line {
            visible_lines.push(visible_line);
        } else {
            visible_lines.push(visible_line.trim_end().to_string());
        }
    }

    // 尾部空行修剪(指令剥离后常留下)。
    while visible_lines
        .last()
        .is_some_and(std::string::String::is_empty)
    {
        visible_lines.pop();
    }

    ParsedAssistantMarkdown {
        visible_markdown: visible_lines.join("\n"),
        git_actions,
    }
}

// 标记 Markdown 中的代码、链接及原始 HTML,避免把字面示例当成动作。
fn markdown_ranges(markdown: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let parser = Parser::new(markdown);
    let mut ranges: Vec<_> = parser
        .reference_definitions()
        .iter()
        .map(|(_, definition)| definition.span.clone())
        .collect();
    let mut text_ranges = Vec::new();
    let mut html_code: Option<(&str, usize, usize)> = None;
    for (event, range) in parser.into_offset_iter() {
        match event {
            Event::InlineHtml(ref html) => {
                let tag = html.trim().to_ascii_lowercase();
                if let Some((name, start, depth)) = html_code.as_mut() {
                    if html_tag_matches(&tag, name, true) {
                        *depth -= 1;
                        if *depth == 0 {
                            ranges.push(*start..range.end);
                            html_code = None;
                        }
                    } else if html_tag_matches(&tag, name, false) && !tag.ends_with("/>") {
                        *depth += 1;
                    }
                } else if !tag.ends_with("/>") {
                    for name in ["code", "pre", "script", "style", "textarea"] {
                        if html_tag_matches(&tag, name, false) {
                            html_code = Some((name, range.start, 1));
                            break;
                        }
                    }
                }
                ranges.push(range);
            }
            Event::Text(_) => text_ranges.push(range),
            Event::Code(_) | Event::Html(_) => ranges.push(range),
            Event::Start(
                Tag::CodeBlock(_) | Tag::Link { .. } | Tag::Image { .. } | Tag::HtmlBlock,
            ) => ranges.push(range),
            _ => {}
        }
    }
    if let Some((_, start, _)) = html_code {
        ranges.push(start..markdown.len());
    }
    (merge_ranges(ranges), merge_ranges(text_ranges))
}

fn merge_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|range| range.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        if let Some(last) = merged.last_mut()
            && range.start <= last.end
        {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    merged
}

// 只匹配完整标签名,避免把 `<code-example>` 当成 `<code>`。
fn html_tag_matches(tag: &str, name: &str, closing: bool) -> bool {
    let prefix = if closing {
        format!("</{name}")
    } else {
        format!("<{name}")
    };
    tag.strip_prefix(&prefix)
        .and_then(|suffix| suffix.chars().next())
        .is_some_and(|character| {
            character == '>' || character == '/' || character.is_ascii_whitespace()
        })
}

fn range_overlaps(protected: &[Range<usize>], range: Range<usize>) -> bool {
    let index = protected.partition_point(|item| item.end <= range.start);
    protected
        .get(index)
        .is_some_and(|item| item.start < range.end)
}

// 判断指令前的反斜杠是否转义了第一个冒号。
fn is_escaped(line: &str, start: usize) -> bool {
    line.as_bytes()[..start]
        .iter()
        .rev()
        .take_while(|byte| **byte == b'\\')
        .count()
        % 2
        == 1
}

/// 把一行里的非字面 `::git-*` 指令剥掉,返回可见文本与动作列表。
fn strip_line_directives(
    line: &str,
    offset: usize,
    protected: &[Range<usize>],
    text_ranges: &[Range<usize>],
) -> (String, Vec<GitActionDirective>) {
    let mut visible = String::new();
    let mut actions = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = line[cursor..].find("::git-") {
        let start = cursor + relative;
        visible.push_str(&line[cursor..start]);
        if is_escaped(line, start)
            || !range_overlaps(text_ranges, offset + start..offset + start + 1)
            || range_overlaps(protected, offset + start..offset + start + 6)
        {
            visible.push_str("::git-");
            cursor = start + 6;
            continue;
        }
        let after_start = start + 2;
        let Some(relative_end) = line[after_start..].find('}') else {
            visible.push_str(&line[start..]);
            return (visible, actions);
        };
        let end = after_start + relative_end + 1;
        if range_overlaps(protected, offset + start..offset + end) {
            visible.push_str(&line[start..end]);
        } else {
            let body = &line[after_start..end - 1];
            if let Some((name, attrs)) = body.split_once('{')
                && let Some(attrs) = parse_attributes(attrs)
                && let Some(action) = parse_git_action(name, &attrs)
            {
                actions.push(action);
            }
        }
        cursor = end;
    }
    visible.push_str(&line[cursor..]);
    (visible, actions)
}

fn parse_git_action(name: &str, attrs: &HashMap<String, String>) -> Option<GitActionDirective> {
    let cwd = attrs.get("cwd")?.clone();
    if cwd.trim().is_empty() {
        return None;
    }
    let branch = || {
        attrs
            .get("branch")
            .filter(|value| !value.trim().is_empty())
            .cloned()
    };
    match name {
        "git-stage" => Some(GitActionDirective::Stage { cwd }),
        "git-commit" => Some(GitActionDirective::Commit { cwd }),
        "git-create-branch" => Some(GitActionDirective::CreateBranch {
            branch: branch()?,
            cwd,
        }),
        "git-push" => Some(GitActionDirective::Push {
            branch: branch()?,
            cwd,
        }),
        "git-create-pr" => Some(GitActionDirective::CreatePr {
            branch: branch()?,
            cwd,
            url: attrs.get("url").cloned(),
            is_draft: attrs.get("isDraft").is_some_and(|value| value == "true"),
        }),
        _ => None,
    }
}

/// 第一份属性解析器(git 指令用):空格分隔的 `key=value`,值可带双引号
/// (引号内不做转义,取到下一个引号为止)。
fn parse_attributes(input: &str) -> Option<HashMap<String, String>> {
    let mut attrs = HashMap::new();
    let mut rest = input.trim();
    while !rest.is_empty() {
        let eq = rest.find('=')?;
        let key = rest[..eq].trim();
        if key.is_empty() {
            return None;
        }
        rest = rest[eq + 1..].trim_start();
        let (value, next) = if let Some(quoted) = rest.strip_prefix('"') {
            let end = quoted.find('"')?;
            (quoted[..end].to_string(), &quoted[end + 1..])
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            (rest[..end].to_string(), &rest[end..])
        };
        attrs.insert(key.to_string(), value);
        rest = next.trim_start();
    }
    Some(attrs)
}

/// code-comment 行重写。1-3 个冒号前缀;三个必填属性(title / body /
/// file)缺失或空 → `None`(调用方按原文保留)。
fn rewrite_code_comment_line(line: &str, cwd: &Path) -> Option<String> {
    let content = line.trim_start_matches([' ', '\t']);
    let indent = &line[..line.len() - content.len()];
    let marker_length = content.bytes().take_while(|byte| *byte == b':').count();
    if !(1..=3).contains(&marker_length) {
        return None;
    }

    let directive = content[marker_length..].strip_prefix("code-comment{")?;
    let end = code_comment_end(directive)?;
    let attributes = parse_code_comment_attributes(&directive[..end])?;
    let suffix = &directive[end + 1..];
    let title = attributes.get("title")?.trim();
    let body = attributes.get("body")?.trim();
    let file = attributes.get("file")?.trim();
    (!title.is_empty() && !body.is_empty() && !file.is_empty()).then_some(())?;

    let start = directive_integer(&attributes, "start").unwrap_or(1).max(1);
    let end = directive_integer(&attributes, "end")
        .unwrap_or(start)
        .max(start);
    // title 已含 [Pn] 前缀则不重复加。
    let title = if title_has_priority(title) {
        title.to_string()
    } else if let Some(p) = attributes
        .get("priority")
        .map(|p| p.trim_start_matches(['P', 'p']))
        .and_then(|p| p.parse::<u8>().ok())
        .filter(|p| *p <= 3)
    {
        format!("[P{p}] {title}")
    } else {
        title.to_string()
    };
    let file_display = relative_display(file, cwd);
    let range = if end > start {
        format!("{file_display}:{start}-{end}")
    } else {
        format!("{file_display}:{start}")
    };
    let mut out = format!("{indent}- {title} — {range}\n");
    for body_line in body.lines() {
        out.push_str(indent);
        out.push_str("  ");
        out.push_str(body_line);
        out.push('\n');
    }
    if !suffix.trim().is_empty() {
        out.push_str(indent);
        out.push_str(suffix.trim_start_matches([' ', '\t']));
        out.push('\n');
    }
    Some(out)
}

// 只把引号外的右花括号视为结束标记,保留后续文本。
fn code_comment_end(input: &str) -> Option<usize> {
    let mut quoted = false;
    let mut chars = input.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        match character {
            '\\' if quoted && chars.peek().is_some_and(|(_, next)| *next == '"') => {
                chars.next();
            }
            '"' => quoted = !quoted,
            '}' if !quoted => return Some(index),
            _ => {}
        }
    }
    None
}

/// title 是否已带 `[P<digits>]` 优先级前缀。
fn title_has_priority(title: &str) -> bool {
    let Some(rest) = title.strip_prefix("[P") else {
        return false;
    };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && rest[digits..].starts_with(']')
}

/// file 相对 cwd(统一 `/` 分隔符);不在 cwd 下则原样。
fn relative_display(file: &str, cwd: &Path) -> String {
    Path::new(file)
        .strip_prefix(cwd)
        .unwrap_or(Path::new(file))
        .to_string_lossy()
        .replace('\\', "/")
}

fn directive_integer(attrs: &HashMap<String, String>, key: &str) -> Option<usize> {
    attrs.get(key).and_then(|value| value.trim().parse().ok())
}

/// 第二份属性解析器(code-comment 用):空格分隔的 `key=value`,引号值经
/// [`parse_quoted_value`] 支持 `\"` 转义(其它反斜杠序列原样保留)——
/// 与上一份行为不同,测试锁定,勿统一。
fn parse_code_comment_attributes(input: &str) -> Option<HashMap<String, String>> {
    let mut attributes = HashMap::new();
    let mut rest = input.trim();
    while !rest.is_empty() {
        let equals = rest.find('=')?;
        let name = rest[..equals].trim();
        if name.is_empty() {
            return None;
        }
        rest = rest[equals + 1..].trim_start();
        let (value, next) = if let Some(quoted) = rest.strip_prefix('"') {
            parse_quoted_value(quoted)?
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            (rest[..end].to_string(), &rest[end..])
        };
        attributes.insert(name.to_string(), value);
        rest = next.trim_start();
    }
    Some(attributes)
}

/// 解析引号包裹的值:遇闭合 `"` 返回(值, 剩余);只把 `\"` 还原成 `"`,
/// 其它字符(含孤立反斜杠,如 `\n`)原样入值。无闭合引号 → `None`。
fn parse_quoted_value(input: &str) -> Option<(String, &str)> {
    let mut value = String::new();
    let mut characters = input.char_indices().peekable();

    while let Some((index, character)) = characters.next() {
        if character == '"' {
            return Some((value, &input[index + 1..]));
        }
        match character {
            '\\' if characters.peek().is_some_and(|(_, next)| *next == '"') => {
                value.push('"');
                characters.next();
            }
            _ => value.push(character),
        }
    }

    None
}

#[cfg(test)]
mod tests;
