//! MCP `Content` ↔ `zhuye_protocol::ContentBlock` 转换。
//!
//! MCP 5 个 variant:`Text` / `Image` / `Resource` / `Audio` / `ResourceLink`。
//! Zhuye `ContentBlock` 有 `Text` / `Image` / `Diff` / `ToolUse` / `ToolResult`。
//! `Audio` 与 `Resource*` 暂降级为文本 placeholder (`[unsupported MCP content: ...]`),
//! 留 v0.4 补全。

use base64::Engine as _;
use rmcp::model::ContentBlock as McpContent;

use zhuye_protocol::ContentBlock;

/// 把 MCP `Vec<Content>` 转 `Vec<ContentBlock>`。
///
/// 不支持的内容输出占位文本;无效图片输出错误提示。
pub fn convert_mcp_content(items: &[McpContent]) -> Vec<ContentBlock> {
    convert_mcp_content_checked(items).0
}

pub(crate) fn convert_mcp_content_checked(items: &[McpContent]) -> (Vec<ContentBlock>, bool) {
    let mut invalid = false;
    let blocks = items
        .iter()
        .map(|item| convert_one(item, &mut invalid))
        .collect();
    (blocks, invalid)
}

fn convert_one(raw: &McpContent, invalid: &mut bool) -> ContentBlock {
    match raw {
        McpContent::Text(t) => ContentBlock::Text {
            text: t.text.clone(),
        },
        McpContent::Image(img) => match base64::engine::general_purpose::STANDARD
            .decode(&img.data)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&img.data))
        {
            Ok(data) => ContentBlock::Image {
                data,
                mime_type: img.mime_type.clone(),
            },
            Err(_) => {
                *invalid = true;
                ContentBlock::Text {
                    text: "[无效的 MCP 图片数据]".into(),
                }
            }
        },
        McpContent::Audio(a) => ContentBlock::Text {
            text: format!("[unsupported MCP content: audio/{}]", a.mime_type),
        },
        McpContent::Resource(_) | McpContent::ResourceLink(_) => ContentBlock::Text {
            text: "[unsupported MCP content: resource]".to_string(),
        },
        // rmcp 3.x 标记 `non_exhaustive`,未来加 variant 时此分支兜底。
        #[allow(unreachable_patterns)]
        _ => ContentBlock::Text {
            text: format!("[unsupported MCP content: {:?}]", raw),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trip() {
        let items = vec![McpContent::text("hi")];
        let out = convert_mcp_content(&items);
        match &out[0] {
            ContentBlock::Text { text } => assert_eq!(text, "hi"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn image_is_decoded_to_original_bytes() {
        let items = vec![McpContent::image("iVBORw==", "image/png")];
        let out = convert_mcp_content(&items);
        match &out[0] {
            ContentBlock::Image { data, mime_type } => {
                assert_eq!(data, &[0x89, b'P', b'N', b'G']);
                assert_eq!(mime_type, "image/png");
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn unpadded_image_is_decoded() {
        let items = vec![McpContent::image("iVBORw", "image/png")];
        let out = convert_mcp_content(&items);
        assert!(
            matches!(&out[0], ContentBlock::Image { data, .. } if data == &[0x89, b'P', b'N', b'G'])
        );
    }

    #[test]
    fn invalid_image_is_explained_without_leaking_payload() {
        let items = vec![McpContent::image("not-base64!", "image/png")];
        let (out, invalid) = convert_mcp_content_checked(&items);
        assert!(invalid);
        assert!(
            matches!(&out[0], ContentBlock::Text { text } if text.contains("无效") && !text.contains("not-base64"))
        );
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let out = convert_mcp_content(&[]);
        assert!(out.is_empty());
    }
}
