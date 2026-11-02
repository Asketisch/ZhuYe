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
fn last_created_branch_cwd_uses_the_last_matching_directive() {
    let parsed = parse_assistant_markdown(
        "::git-create-branch{cwd=\"/first\" branch=\"first\"}\n::git-push{cwd=\"/repo\" branch=\"first\"}\n::git-create-branch{cwd=\"/second\" branch=\"second\"}",
        Path::new("/repo"),
    );

    assert_eq!(parsed.last_created_branch_cwd(), Some("/second"));
}
