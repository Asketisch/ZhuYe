//! 指令解析测试(移植 Reflect-TUI 的 5 例;原 insta 快照断言按快照内容
//! 展开为显式字符串断言,避免引入 insta 依赖)。

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn strips_and_parses_git_action_directives() {
    let parsed = parse_assistant_markdown(
        "Done\n\n::git-stage{cwd=\"/repo\"} ::git-push{cwd=\"/repo\" branch=\"feat/x\"} ::git-stage{cwd=\"C:\\repo\\\"}",
        Path::new("/repo"),
    );

    assert_eq!(parsed.visible_markdown, "Done");
    assert_eq!(
        parsed.git_actions,
        vec![
            GitActionDirective::Stage {
                cwd: "/repo".to_string()
            },
            GitActionDirective::Push {
                cwd: "/repo".to_string(),
                branch: "feat/x".to_string()
            },
            // Windows 风格反斜杠路径原样通过(引号内不转义)。
            GitActionDirective::Stage {
                cwd: "C:\\repo\\".to_string()
            },
        ]
    );
}

#[test]
fn hides_malformed_directives_without_materializing_rows() {
    let parsed = parse_assistant_markdown("Done ::git-push{cwd=\"/repo\"}", Path::new("/repo"));

    assert_eq!(parsed.visible_markdown, "Done");
    assert!(parsed.git_actions.is_empty());
}

#[test]
fn renders_code_comment_directives_as_markdown() {
    let parsed = parse_assistant_markdown(
        concat!(
            "Found two issues.\n\n",
            r#"::code-comment{title="Fix body= parsing" body="Keep role=\"tab\", ::git-stage{cwd=/tmp}, file=, and \n literal." file="/repo/src/app.ts" start=10 end=12 priority="P2"}"#,
            "\n\n",
            r#":::code-comment{title="[P1] Clamp the range" body="The line range should match the App." file="reflect/src/range.ts" start=8 end=2 priority=3}"#,
        ),
        Path::new("/repo"),
    );

    // 快照内容(移植自 TUI 版 insta 快照)。
    assert_eq!(
        parsed.visible_markdown,
        "Found two issues.\n\n\
         - [P2] Fix body= parsing — src/app.ts:10-12\n  \
         Keep role=\"tab\", ::git-stage{cwd=/tmp}, file=, and \\n literal.\n\n\
         - [P1] Clamp the range — reflect/src/range.ts:8\n  \
         The line range should match the App."
    );
    assert!(parsed.git_actions.is_empty());
}

#[test]
fn preserves_non_directive_and_malformed_code_comment_text() {
    let markdown = "Mention `::code-comment{title=\"Example\"}` inline.\n::code-comment{title=\"Missing body\" file=\"/repo/src/app.ts\"}";
    let parsed = parse_assistant_markdown(markdown, Path::new("/repo"));

    assert_eq!(parsed.visible_markdown, markdown);
}

#[test]
fn malformed_code_comment_cannot_emit_nested_git_actions() {
    let markdown = concat!(
        "::code-comment{title=问题 body=\"::git-push{cwd=/repo branch=bad}\"}\n",
        ":::code-comment{title=问题 body=\"::git-push{cwd=/repo branch=bad}\n",
        "::git-push{cwd=/repo branch=good}"
    );
    let parsed = parse_assistant_markdown(markdown, Path::new("/repo"));
    assert!(parsed.visible_markdown.contains("branch=bad"));
    assert_eq!(parsed.git_actions.len(), 1);
    assert!(format!("{:?}", parsed.git_actions[0]).contains("good"));
}

#[test]
fn last_created_branch_cwd_uses_the_last_matching_directive() {
    let parsed = parse_assistant_markdown(
        "::git-create-branch{cwd=\"/first\" branch=\"first\"}\n::git-push{cwd=\"/repo\" branch=\"first\"}\n::git-create-branch{cwd=\"/second\" branch=\"second\"}",
        Path::new("/repo"),
    );

    assert_eq!(parsed.last_created_branch_cwd(), Some("/second"));
}

#[test]
fn code_and_link_literals_never_create_git_actions() {
    let markdown = concat!(
        "示例 `::git-commit{cwd=/inline}`。\n",
        "[查看示例](https://example.test/::git-stage{cwd=/url})\n",
        "\\::git-stage{cwd=/escaped}\n",
        "```text\n::git-push{cwd=/fenced branch=x}\n",
        "::code-comment{title=例子 body=正文 file=/repo/a}\n```\n",
        "~~~\n::git-commit{cwd=/tilde}\n~~~\n",
        "    ::git-stage{cwd=/indented}\n",
        "实际动作 ::git-stage{cwd=/actual}\n",
    );
    let parsed = parse_assistant_markdown(markdown, Path::new("/repo"));
    assert_eq!(
        parsed.git_actions,
        [GitActionDirective::Stage {
            cwd: "/actual".into()
        }]
    );
    assert!(
        parsed
            .visible_markdown
            .contains("`::git-commit{cwd=/inline}`")
    );
    assert!(parsed.visible_markdown.contains("::git-stage{cwd=/url}"));
    assert!(
        parsed
            .visible_markdown
            .contains("\\::git-stage{cwd=/escaped}")
    );
    assert!(
        parsed
            .visible_markdown
            .contains("::git-push{cwd=/fenced branch=x}")
    );
    assert!(
        parsed
            .visible_markdown
            .contains("::code-comment{title=例子 body=正文 file=/repo/a}")
    );
    assert!(parsed.visible_markdown.contains("::git-commit{cwd=/tilde}"));
    assert!(
        parsed
            .visible_markdown
            .contains("    ::git-stage{cwd=/indented}")
    );
    assert!(parsed.visible_markdown.ends_with("实际动作"));
}

#[test]
fn multiline_inline_code_and_crlf_preserve_directive_literals() {
    let markdown =
        "示例 ``代码\r\n::git-stage{cwd=/literal}\r\n仍是代码``\r\n::git-commit{cwd=/actual}";
    let parsed = parse_assistant_markdown(markdown, Path::new("/repo"));
    assert_eq!(
        parsed.git_actions,
        [GitActionDirective::Commit {
            cwd: "/actual".into()
        }]
    );
    assert!(
        parsed
            .visible_markdown
            .contains("::git-stage{cwd=/literal}")
    );
}

#[test]
fn escaped_and_linked_directives_keep_following_real_actions() {
    let markdown = r#"\::git-stage{cwd=/literal} [说明](https://example.test/::git-stage{cwd=/bad}) ::git-commit{cwd=/good}"#;
    let parsed = parse_assistant_markdown(markdown, Path::new("/repo"));
    assert_eq!(
        parsed.git_actions,
        [GitActionDirective::Commit {
            cwd: "/good".into()
        }]
    );
    assert!(
        parsed
            .visible_markdown
            .contains("\\::git-stage{cwd=/literal}")
    );
    assert!(parsed.visible_markdown.contains("::git-stage{cwd=/bad}"));
}

#[test]
fn html_code_content_is_not_interpreted_as_a_git_action() {
    let markdown =
        r#"示例 <code class="example">::git-push{cwd=/bad branch=x}</code> ::git-stage{cwd=/good}"#;
    let parsed = parse_assistant_markdown(markdown, Path::new("/repo"));
    assert_eq!(
        parsed.git_actions,
        [GitActionDirective::Stage {
            cwd: "/good".into()
        }]
    );
    assert!(
        parsed
            .visible_markdown
            .contains("::git-push{cwd=/bad branch=x}")
    );
}

#[test]
fn literal_code_and_hard_break_whitespace_are_preserved() {
    let markdown = "说明  \n```text\n::git-stage{cwd=/literal}  \n```";
    let parsed = parse_assistant_markdown(markdown, Path::new("/repo"));
    assert!(parsed.git_actions.is_empty());
    assert_eq!(parsed.visible_markdown, markdown);
}

#[test]
fn code_comment_preserves_braces_in_body_and_trailing_text() {
    let markdown = r#"::code-comment{title="问题" body="保留 \"引号\" 和 } 内容" file="/repo/a.rs"} 后续 {说明}"#;
    let parsed = parse_assistant_markdown(markdown, Path::new("/repo"));
    assert_eq!(
        parsed.visible_markdown,
        "- 问题 — a.rs:1\n  保留 \"引号\" 和 } 内容\n后续 {说明}"
    );
    assert!(parsed.git_actions.is_empty());
}

#[test]
fn empty_working_directory_or_branch_never_creates_an_action() {
    let markdown = concat!(
        "::git-stage{cwd=\"\"} ",
        "::git-commit{cwd=\"  \"} ",
        "::git-create-branch{cwd=/repo branch=\"\"} ",
        "::git-push{cwd=/repo branch=\"  \"} ",
        "::git-create-pr{cwd=/repo branch=\"\"}"
    );
    let parsed = parse_assistant_markdown(markdown, Path::new("/repo"));
    assert!(parsed.git_actions.is_empty(), "{:?}", parsed.git_actions);
}

#[test]
fn reference_link_definitions_cannot_create_actions() {
    let markdown =
        "[ref]: https://example.test/::git-stage{cwd=/literal}\n\n[ref]\n\n::git-commit{cwd=/real}";
    let parsed = parse_assistant_markdown(markdown, Path::new("/repo"));
    assert_eq!(
        parsed.git_actions,
        [GitActionDirective::Commit {
            cwd: "/real".into()
        }]
    );
    assert!(
        parsed
            .visible_markdown
            .contains("::git-stage{cwd=/literal}")
    );
}

#[test]
fn duplicate_reference_definitions_are_not_actions() {
    let markdown = "[ref]: https://first.test\n[ref]: https://example.test/::git-stage{cwd=/literal}\n\n[ref]\n\n::git-commit{cwd=/real}";
    let parsed = parse_assistant_markdown(markdown, Path::new("/repo"));
    assert_eq!(
        parsed.git_actions,
        [GitActionDirective::Commit {
            cwd: "/real".into()
        }]
    );
    assert!(
        parsed
            .visible_markdown
            .contains("::git-stage{cwd=/literal}")
    );
}

#[test]
fn image_alt_and_link_labels_remain_literal() {
    let markdown = "![::git-stage{cwd=/image}](image.png) [::git-commit{cwd=/link}](https://example.test) ::git-stage{cwd=/real}";
    let parsed = parse_assistant_markdown(markdown, Path::new("/repo"));
    assert_eq!(
        parsed.git_actions,
        [GitActionDirective::Stage {
            cwd: "/real".into()
        }]
    );
    assert!(parsed.visible_markdown.contains("::git-stage{cwd=/image}"));
    assert!(parsed.visible_markdown.contains("::git-commit{cwd=/link}"));
}
