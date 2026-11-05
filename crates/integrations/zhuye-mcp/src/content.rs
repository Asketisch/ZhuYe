//! MCP `Content` ↔ `zhuye_protocol::ContentBlock` 转换。
//!
//! MCP 5 个 variant:`Text` / `Image` / `Resource` / `Audio` / `ResourceLink`。
//! Zhuye `ContentBlock` 有 `Text` / `Image` / `Diff` / `ToolUse` / `ToolResult`。
//! `Audio` 与 `Resource*` 暂降级为文本 placeholder (`[unsupported MCP content: ...]`),
//! 留 v0.4 补全。

use base64::Engine as _;
use rmcp::model::{Content, RawContent};

use zhuye_protocol::ContentBlock;

/// 把 MCP `Vec<Content>` 转 `Vec<ContentBlock>`。
///
/// 不支持的内容输出占位文本;无效图片输出错误提示。
pub fn convert_mcp_content(items: &[Content]) -> Vec<ContentBlock> {
    convert_mcp_content_checked(items).0
}

pub(crate) fn convert_mcp_content_checked(items: &[Content]) -> (Vec<ContentBlock>, bool) {
    let mut invalid = false;
    let blocks = items
        .iter()
        .map(|item| convert_one(&item.raw, &mut invalid))
        .collect();
    (blocks, invalid)
}

fn convert_one(raw: &RawContent, invalid: &mut bool) -> ContentBlock {
    match raw {
        RawContent::Text(t) => ContentBlock::Text {
            text: t.text.clone(),
        },
        RawContent::Image(img) => match base64::engine::general_purpose::STANDARD
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
        RawContent::Audio(a) => ContentBlock::Text {
            text: format!("[unsupported MCP content: audio/{}]", a.mime_type),
        },
        RawContent::Resource(_) | RawContent::ResourceLink(_) => ContentBlock::Text {
            text: "[unsupported MCP content: resource]".to_string(),
        },
        // rmcp 1.7 标记 `non_exhaustive`,未来加 variant 时此分支兜底。
        #[allow(unreachable_patterns)]
        _ => ContentBlock::Text {
            text: format!("[unsupported MCP content: {:?}]", raw),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{RawImageContent, RawTextContent};

    #[test]
    fn text_round_trip() {
        let items = vec![Content::new(
            RawContent::Text(RawTextContent {
                text: "hi".to_string(),
                meta: None,
            }),
            None,
        )];
        let out = convert_mcp_content(&items);
        match &out[0] {
            ContentBlock::Text { text } => assert_eq!(text, "hi"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn image_is_decoded_to_original_bytes() {
        let items = vec![Content::new(
            RawContent::Image(RawImageContent {
                data: "iVBORw==".to_string(),
                mime_type: "image/png".to_string(),
                meta: None,
            }),
            None,
        )];
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
        let items = vec![Content::new(
            RawContent::Image(RawImageContent {
                data: "iVBORw".into(),
                mime_type: "image/png".into(),
                meta: None,
            }),
            None,
        )];
        let out = convert_mcp_content(&items);
        assert!(
            matches!(&out[0], ContentBlock::Image { data, .. } if data == &[0x89, b'P', b'N', b'G'])
        );
    }

    #[test]
    fn invalid_image_is_explained_without_leaking_payload() {
        let items = vec![Content::new(
            RawContent::Image(RawImageContent {
                data: "not-base64!".into(),
                mime_type: "image/png".into(),
                meta: None,
            }),
            None,
        )];
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
