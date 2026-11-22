//! 工具结果预算(result budget)—— 超长输出的落盘 + 信封替换。
//!
//! 修复背景(借鉴 ZCode result-serialization 三层预算):此前的处理是
//! 「各工具独立截断上限,超长直接砍掉」—— 被砍掉的内容**彻底蒸发**,
//! 模型既拿不到全文,也没有任何"还有更多"的信号。现在:文本总量超过
//! [`ResultBudget::max_inline_bytes`] 时,全文落盘到
//! `$ZHUYE_HOME/artifacts/<call_id>.txt`,模型收到 `<persisted-output>`
//! 信封(路径 + 字节数 + 开头预览)—— 需要细节时可用 `read` 工具按
//! 路径取回全文。禁用落盘(或落盘失败)时退化为中间截断(保留头尾,
//! 砍中间)。空输出替换为显式占位符,防止"静默成功"被误读为正常回复。
//!
//! 纯函数 + 同步落盘;由 `ToolExecutionQueue` 在 sanitizer 之后、
//! PostToolUse hook 之前应用(hook 与模型看到的是同一份预算后输出)。

use std::path::{Path, PathBuf};

use zhuye_protocol::{ContentBlock, ToolOutput};

/// 工具结果预算设置。
#[derive(Debug, Clone)]
pub struct ResultBudget {
    /// 总开关。`false` = 完全 no-op(历史行为)。
    pub enabled: bool,
    /// 文本字节上限:超过即落盘 + 信封替换。
    pub max_inline_bytes: usize,
    /// 信封内预览的字符数(开头连续取,不受中间截断影响)。
    pub preview_chars: usize,
    /// 是否落盘全文。`false` = 超限走中间截断(保留头尾),不写文件。
    pub persist: bool,
}

impl Default for ResultBudget {
    fn default() -> Self {
        Self {
            enabled: true,
            max_inline_bytes: 100_000,
            preview_chars: 2_000,
            persist: true,
        }
    }
}

/// 空输出占位符:防止"静默成功"被模型误读为正常回复。
pub const EMPTY_OUTPUT_PLACEHOLDER: &str = "(Tool completed with no output)";

/// 信封开标签(wire 上模型可见;`read` 工具按 `path` 取全文)。
pub const PERSISTED_OUTPUT_OPEN: &str = "<persisted-output";

/// 中间截断:保留头尾各半,中间替换为省略标记。
///
/// 修复背景:旧实现"从尾部按字符边界截断"只保留开头 —— 日志类输出的
/// 结尾(错误摘要、最终状态)往往比开头更有信息量。
pub fn truncate_middle(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(3) / 2;
    let head: String = text.chars().take(keep).collect();
    let tail: String = text.chars().skip(total - keep).collect();
    format!("{head}\n…[中间省略 {} 字符]…\n{tail}", total - keep * 2)
}

/// 应用结果预算:原地修改 `output`。
///
/// - `tool` / `call_id`:仅用于 artifact 文件命名与 metadata 标注;
/// - 落盘目录:`$ZHUYE_HOME > $HOME/.zhuye` 下 `artifacts/`(zhuye-fs
///   单点解析);目录不存在时自动创建,写失败退化为中间截断并 warn。
pub fn apply(output: &mut ToolOutput, tool: &str, call_id: &str, budget: &ResultBudget) {
    if !budget.enabled {
        return;
    }
    // 空输出占位(先于预算:空输出永远不该原样出门)。
    if output.content.is_empty() {
        output
            .content
            .push(ContentBlock::text(EMPTY_OUTPUT_PLACEHOLDER));
        return;
    }
    // 仅统计文本块字节;非文本块(图像等)受保护通道,不参与预算。
    let text_bytes: usize = output
        .content
        .iter()
        .map(|c| match c {
            ContentBlock::Text { text } => text.len(),
            _ => 0,
        })
        .sum();
    if text_bytes <= budget.max_inline_bytes {
        return;
    }
    // 拼接全部文本(非文本块原样保留在原位)。
    let full_text = output
        .content
        .iter()
        .map(|c| match c {
            ContentBlock::Text { text } => text.as_str(),
            _ => "",
        })
        .collect::<Vec<_>>()
        .join("\n");
    let total_chars = full_text.chars().count() as u64;

    // 优先落盘全文;失败(或被禁用)退化为中间截断。
    let artifact_path = if budget.persist {
        write_artifact(tool, call_id, &full_text)
    } else {
        None
    };
    let envelope = match artifact_path.as_ref() {
        Some(path) => {
            let preview: String = full_text.chars().take(budget.preview_chars).collect();
            let open_tag = format!(
                "{} path=\"{}\" bytes=\"{text_bytes}\" tool=\"{tool}\"",
                PERSISTED_OUTPUT_OPEN,
                path.display()
            );
            format!(
                "{open_tag}\n{preview}\n…(输出共 {total_chars} 字符,已全文落盘;\
                 用 read 工具按 path 读取)\n</persisted-output>"
            )
        }
        None => truncate_middle(
            &full_text,
            budget.max_inline_bytes.saturating_div(3).max(1000),
        ),
    };
    // 替换全部文本块为单个信封块;非文本块原样保留。
    let mut new_content = Vec::with_capacity(output.content.len());
    let mut envelope_pushed = false;
    for block in output.content.drain(..) {
        match block {
            ContentBlock::Text { .. } => {
                if !envelope_pushed {
                    new_content.push(ContentBlock::text(envelope.clone()));
                    envelope_pushed = true;
                }
                // 其余文本块丢弃(已并入信封 / 截断)。
            }
            other => new_content.push(other),
        }
    }
    output.content = new_content;
    output.metadata["artifact"] = artifact_path
        .as_ref()
        .map_or(serde_json::json!("(persist disabled)"), |p| {
            serde_json::json!(p.display().to_string())
        });
}

/// 把全文写到 artifacts 目录,返回绝对路径。目录按需创建。
fn write_artifact(tool: &str, call_id: &str, full_text: &str) -> Option<PathBuf> {
    let dir = zhuye_fs::data_dir()?.join("artifacts");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(dir = ?dir, error = %e, "tool artifacts 目录创建失败;退化为中间截断");
        return None;
    }
    // 文件名清洗:call_id 来自 provider,tool 名来自注册表 —— 只保留
    // 白名单字符,防路径逃逸(与 ZCode session-data-files 同纪律)。
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let path = dir.join(format!("{}_{}.txt", sanitize(tool), sanitize(call_id)));
    match zhuye_fs::write_atomic(&path, full_text.as_bytes()) {
        Ok(()) => Some(path),
        Err(e) => {
            tracing::warn!(path = ?path, error = %e, "tool artifact 落盘失败;退化为中间截断");
            None
        }
    }
}

/// 供测试与调用方核对信封形状。
pub fn is_persisted_envelope(text: &str) -> bool {
    text.starts_with(PERSISTED_OUTPUT_OPEN)
}

/// 供测试构造超限输出。
pub fn budget_for_test(max_inline_bytes: usize) -> ResultBudget {
    ResultBudget {
        enabled: true,
        max_inline_bytes,
        preview_chars: 2_000,
        persist: true,
    }
}

/// 供测试引用 Path 类型(避免 unused import 警告)。
#[allow(dead_code)]
fn _path_type_used(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_output(text: &str) -> ToolOutput {
        ToolOutput {
            content: vec![ContentBlock::text(text)],
            is_error: false,
            metadata: serde_json::json!({}),
            elapsed_ms: 1,
        }
    }

    #[test]
    fn 空输出替换为占位符() {
        let mut out = ToolOutput {
            content: vec![],
            is_error: false,
            metadata: serde_json::json!({}),
            elapsed_ms: 1,
        };
        apply(&mut out, "bash", "c1", &ResultBudget::default());
        assert!(
            matches!(
                &out.content[0],
                ContentBlock::Text { text } if text == EMPTY_OUTPUT_PLACEHOLDER
            ),
            "空输出应为占位符: {:?}",
            out.content
        );
    }

    #[test]
    fn 预算内输出原样保留() {
        let mut out = make_output("short");
        apply(&mut out, "bash", "c1", &budget_for_test(1000));
        assert!(
            matches!(&out.content[0], ContentBlock::Text { text } if text == "short"),
            "预算内输出应原样保留: {:?}",
            out.content
        );
        assert!(out.metadata.get("artifact").is_none());
    }

    #[test]
    fn 超限输出落盘并出信封() {
        let big = "x".repeat(5_000);
        let mut out = make_output(&big);
        apply(&mut out, "bash", "call-1", &budget_for_test(1_000));
        assert_eq!(out.content.len(), 1, "文本块应折叠为单个信封");
        let text = match &out.content[0] {
            ContentBlock::Text { text } => text,
            other => panic!("expect text, got {other:?}"),
        };
        assert!(
            is_persisted_envelope(text),
            "应有 persisted-output 信封: {text}"
        );
        assert!(
            text.contains("5000") || text.contains("5000 字符") || text.contains("bytes=\"5000\"")
        );
        assert!(text.contains('…'), "应有省略标记");
        // metadata 标注 artifact 路径(落盘成功时)。
        let artifact = out.metadata.get("artifact").and_then(|v| v.as_str());
        assert!(
            artifact.is_some_and(|p| p.contains("artifacts")),
            "metadata 应含 artifact 路径: {out:?}"
        );
        // 落盘文件真实存在且为全文。
        if let Some(p) = artifact {
            let content = std::fs::read_to_string(p).expect("artifact 可读");
            assert_eq!(content, big, "artifact 应是未截断全文");
        }
    }

    #[test]
    fn 中间截断保留头尾() {
        let long = format!("HEAD{}TAIL", "m".repeat(500));
        let cut = truncate_middle(&long, 50);
        assert!(cut.starts_with("HEAD"));
        assert!(cut.ends_with("TAIL"));
        assert!(cut.contains("中间省略"));
        assert!(cut.chars().count() < 120);
        // 预算内不截断。
        assert_eq!(truncate_middle("short", 50), "short");
    }

    #[test]
    fn 非文本块受保护保留原位() {
        let mut out = ToolOutput {
            content: vec![
                ContentBlock::text("x".repeat(5_000)),
                ContentBlock::Image {
                    mime_type: "image/png".into(),
                    data: "aGk=".into(),
                },
            ],
            is_error: false,
            metadata: serde_json::json!({}),
            elapsed_ms: 1,
        };
        apply(&mut out, "bash", "call-2", &budget_for_test(1_000));
        assert!(
            out.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. })),
            "图像块应原样保留"
        );
        assert!(
            out.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if is_persisted_envelope(text))),
            "文本块应折叠为信封"
        );
    }
}
