//! `write` —— 写文本文件(自动 mkdir,返回 unified diff)。
//!
//! 详见 `docs/tools-and-hooks.md §2.3`。

use crate::sandbox::PinnedWrite;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};
use async_trait::async_trait;
use serde_json::Value;
use similar::TextDiff;
use zhuye_protocol::PermissionMode;

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        zhuye_prompt::copy("tool.write")
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to workspace, or absolute path under workspace"
                },
                "content": {
                    "type": "string",
                    "description": "Full new file content (overwrites existing file)"
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn required_permission(&self) -> PermissionMode {
        PermissionMode::Prompt
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let path_str =
            args.get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: "missing 'path'".into(),
                })?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                // 错误信息保留关键字 `missing 'content'` 兼容已有测试,
                // 同时给 LLM 一条清晰路径:Plan mode 下写 plan 应改用 PlanWrite。
                message: "missing 'content' — write 需要 path 与 content 两个参数。Plan mode 下写 plan 推荐改用 PlanWrite {name, content}".into(),
            })?;

        let path = std::path::Path::new(path_str);
        // 写路径解析:写根内放行;越界经审批门强制询问(ZCode alwaysAsk
        // 同款 —— 写工作区外是一次人工确认,不设路径硬墙);`.git` 等元
        // 数据拒绝。
        let final_path = crate::sandbox::resolve_writable_path_with_approval(
            &ctx,
            "write",
            &args,
            path,
        )
        .await?;
        // 固定父目录并拒绝路径替换或符号链接，再读旧内容及原子替换。
        let target = PinnedWrite::prepare(&final_path)
            .map_err(|e| ToolError::Io(format!("prepare {path_str}: {e}")))?;
        let existing = target.read_existing().map_err(|e| {
            if e.kind() == std::io::ErrorKind::InvalidInput {
                ToolError::InvalidArgs {
                    message: format!("path 不是普通文件: {path_str}"),
                }
            } else {
                ToolError::Io(format!("read {path_str}: {e}"))
            }
        })?;
        // `existing` 为 `None` 表示旧内容存在但无法按文本比较(二进制或超过
        // diff 上限)。此时不能拿空串算 diff —— 那会把「覆盖了非文本文件」
        // 谎报成「新建文件」—— 而是省略 diff 块并在文本里说明。
        target
            .replace(content.as_bytes())
            .map_err(|e| ToolError::Io(format!("write {path_str}: {e}")))?;

        let mut message = format!("wrote {path_str} ({} bytes)", content.len());
        let mut blocks = Vec::with_capacity(2);
        let unified = match existing.as_deref() {
            Some(before) => {
                let diff = TextDiff::from_lines(before, content)
                    .unified_diff()
                    .to_string();
                blocks.push(zhuye_protocol::ContentBlock::Diff {
                    unified_diff: diff.clone(),
                });
                diff
            }
            None => {
                message.push_str(" [旧内容非文本或过大，diff 省略]");
                String::new()
            }
        };
        blocks.insert(0, zhuye_protocol::ContentBlock::text(message));
        Ok(ToolOutput {
            content: blocks,
            is_error: false,
            metadata: serde_json::json!({
                "path": final_path.to_string_lossy(),
                "path_str": path_str,
                "bytes": content.len(),
                "diff": unified,
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    #[cfg(unix)]
    #[tokio::test]
    async fn preexisting_temporary_symlink_does_not_overwrite_outside_file() {
        use std::os::unix::fs::symlink;
        let ws = tmp_workspace();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("victim.txt");
        std::fs::write(&victim, "original").unwrap();
        symlink(&victim, ws.join("a.txt.zhuye-tmp")).unwrap();
        WriteTool
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path":"a.txt", "content":"new"}),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "original");
    }

    /// 写根之外的探针目录。测试工作区在 temp_dir 下,而 `/tmp` 本身是
    /// 合法写根 —— 旧用例的 sibling 目录不再构成越界;`$HOME` 下的探针
    /// 是唯一可靠的「写根之外」位置。
    fn outside_roots_dir(tag: &str) -> std::path::PathBuf {
        // Windows 无 HOME 环境变量(CI 实证:expect 直接 panic),回退
        // USERPROFILE;下同。
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .expect("测试需要 HOME 或 USERPROFILE");
        let dir = std::path::Path::new(&home).join(format!(
            ".zhuye_sb_probe_{}_{}",
            tag,
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cleanup_probe(dir: &std::path::Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn missing_directory_followed_by_parents_cannot_escape() {
        let ws = tmp_workspace();
        let probe = outside_roots_dir("missing");
        let victim = probe.join("victim.txt");
        std::fs::write(&victim, "original").unwrap();
        // `missing/../` 归一后指向 $HOME 下的探针文件 —— 写根之外。
        let escaped = probe.join("missing").join("..").join("victim.txt");
        let result = WriteTool
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": escaped.to_string_lossy(), "content":"new"}),
            )
            .await;
        assert!(matches!(result, Err(ToolError::PathEscape { .. })));
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "original");
        cleanup_probe(&probe);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn normalized_parent_cannot_hide_an_external_symlink() {
        use std::os::unix::fs::symlink;
        let ws = tmp_workspace();
        let probe = outside_roots_dir("norm");
        let victim = probe.join("victim.txt");
        std::fs::write(&victim, "original").unwrap();
        symlink(&probe, ws.join("link")).unwrap();
        let result = WriteTool
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path":"missing/../link/victim.txt", "content":"new"}),
            )
            .await;
        assert!(matches!(result, Err(ToolError::PathEscape { .. })));
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "original");
        cleanup_probe(&probe);
    }

    #[tokio::test]
    async fn missing_parent_component_normalizes_inside_workspace() {
        let ws = tmp_workspace();
        WriteTool
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path":"missing/../file.txt", "content":"inside"}),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(ws.join("file.txt")).unwrap(),
            "inside"
        );
    }

    fn make_ctx(workspace: &std::path::Path) -> ToolContext {
        ToolContext::for_workspace(workspace)
    }

    fn tmp_workspace() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("zhuye_write_test_{}_{}", std::process::id(), n));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).ok();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn writes_new_file() {
        let ws = tmp_workspace();
        let t = WriteTool;
        let out = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "a.txt", "content": "hello\nworld"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(ws.join("a.txt")).unwrap(),
            "hello\nworld"
        );
        // Diff 块应存在。
        assert!(
            out.content
                .iter()
                .any(|c| matches!(c, zhuye_protocol::ContentBlock::Diff { .. }))
        );
    }

    #[tokio::test]
    async fn auto_creates_parent_dirs() {
        let ws = tmp_workspace();
        let ws_canonical = ws.canonicalize().unwrap();
        let t = WriteTool;
        let out = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "sub/dir/a.txt", "content": "x"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "execute failed: {out:?}");
        assert!(ws_canonical.join("sub/dir/a.txt").exists());
    }

    #[tokio::test]
    async fn overwrites_existing_file_with_diff() {
        let ws = tmp_workspace();
        std::fs::write(ws.join("a.txt"), "old\n").unwrap();
        let t = WriteTool;
        let out = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "a.txt", "content": "new\n"}),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "new\n");
        // Diff 应包含删除 / 添加标记。
        let diff = out.content.iter().find_map(|c| match c {
            zhuye_protocol::ContentBlock::Diff { unified_diff } => Some(unified_diff.clone()),
            _ => None,
        });
        let diff = diff.expect("diff block");
        assert!(diff.contains("-old") || diff.contains("---"));
        assert!(diff.contains("+new") || diff.contains("+++"));
    }

    #[tokio::test]
    async fn overwriting_binary_file_omits_diff() {
        let ws = tmp_workspace();
        // 非 UTF-8 字节:旧实现把它当空串算 diff,谎称「原文件是空的」。
        std::fs::write(ws.join("blob.dat"), [0xff_u8, 0xfe, 0x00, 0x90]).unwrap();
        let out = WriteTool
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "blob.dat", "content": "text now\n"}),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(ws.join("blob.dat")).unwrap(),
            "text now\n"
        );
        assert!(
            !out.content
                .iter()
                .any(|c| matches!(c, zhuye_protocol::ContentBlock::Diff { .. })),
            "binary overwrite must not report a diff"
        );
        assert_eq!(out.metadata["diff"], "");
        let text = match &out.content[0] {
            zhuye_protocol::ContentBlock::Text { text } => text.clone(),
            other => panic!("expected text block, got {other:?}"),
        };
        assert!(text.contains("diff"), "missing explanation: {text}");
    }

    #[tokio::test]
    async fn rejects_missing_path() {
        let ws = tmp_workspace();
        let t = WriteTool;
        let err = t
            .execute(make_ctx(&ws), serde_json::json!({"content": "x"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    // ── 0.2 回归 → workspace-write 口径 ────────────────────────────
    // `/tmp` 是合法写根:相对 `..` 走到 temp 内的 sibling 属于放行面;
    // 越界负例一律用 `$HOME` 下的探针(见 outside_roots_dir)。

    /// 相对 `..` 穿越到 temp 内 sibling:解析后仍在写根内,放行。
    #[tokio::test]
    async fn relative_parent_dir_into_tmp_root_ok() {
        let ws = tmp_workspace();
        let sibling = ws.parent().unwrap().join("zhuye_write_tmp_probe.txt");
        let _ = std::fs::remove_file(&sibling);
        WriteTool
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "../zhuye_write_tmp_probe.txt", "content": "ok"}),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&sibling).unwrap(),
            "ok",
            "temp 内 sibling 属于写根,应写入成功"
        );
        let _ = std::fs::remove_file(&sibling);
    }

    /// 嵌套 `..`(先下钻再跳出)解析进 `$HOME` 探针:必须被拒。
    #[tokio::test]
    async fn rejects_nested_parent_dir_escape() {
        let probe = outside_roots_dir("nested");
        #[cfg(not(unix))]
        {
            // 非 Unix(Windows):symlink 创建需要管理员/开发者模式,
            // 本回归仅在 Unix 上执行。
            cleanup_probe(&probe);
            return;
        }
        #[cfg(unix)]
        {
            let ws = tmp_workspace();
            std::fs::create_dir_all(ws.join("sub")).unwrap();
            let victim = probe.join("victim.txt");
            std::fs::write(&victim, "original").unwrap();
            std::os::unix::fs::symlink(&probe, ws.join("link")).unwrap();
            let t = WriteTool;
            let err = t
                .execute(
                    make_ctx(&ws),
                    serde_json::json!({"path": "sub/../link/victim.txt", "content": "pwned"}),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(err, ToolError::PathEscape { .. }),
                "嵌套 .. 穿越必须被拒,得到: {err:?}"
            );
            assert_eq!(std::fs::read_to_string(&victim).unwrap(), "original");
            cleanup_probe(&probe);
        }
    }

    /// 绝对路径指向写根之外(`$HOME` 探针):必须被拒。
    #[tokio::test]
    async fn rejects_absolute_path_escape() {
        let ws = tmp_workspace();
        let probe = outside_roots_dir("abs");
        let target = probe.join("pwned.txt");
        let t = WriteTool;
        let err = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": target.to_string_lossy(), "content": "pwned"}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::PathEscape { .. }),
            "绝对路径穿越必须被拒,得到: {err:?}"
        );
        assert!(!target.exists());
        cleanup_probe(&probe);
    }

    /// 越界写 + 审批门:强制询问被批准 → 写入成功;被拒 → 结构化
    /// `PermissionDenied` 且目标不落盘。headless(`approval = None`)的
    /// 旧行为(`PathEscape`)由上方既有用例覆盖。
    #[tokio::test]
    async fn outside_root_write_asks_gate_then_proceeds_or_denies() {
        use std::sync::Arc;
        use zhuye_protocol::{EventMsg, ReviewDecision};

        let ws = tmp_workspace();
        let probe = outside_roots_dir("gate");

        // 放行路径:后台应答者收到 ApprovalRequest 即批准。
        let (tx, mut rx) = tokio::sync::mpsc::channel::<zhuye_protocol::Event>(8);
        let gate = Arc::new(crate::approval::ApprovalGate::new(tx, "write-gate-allow"));
        let responder_gate = gate.clone();
        let responder = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if let EventMsg::ApprovalRequest(req) = event.msg {
                    responder_gate.complete(&req.request_id, ReviewDecision::Approve);
                    break;
                }
            }
        });
        let mut tctx = make_ctx(&ws);
        tctx.approval = Some(gate);
        let approved = probe.join("approved.txt");
        WriteTool
            .execute(
                tctx,
                serde_json::json!({"path": approved.to_string_lossy(), "content": "ok"}),
            )
            .await
            .unwrap();
        responder.await.unwrap();
        assert_eq!(std::fs::read_to_string(&approved).unwrap(), "ok");

        // 拒绝路径:deny → `PermissionDenied`,目标不落盘。
        let (tx, mut rx) = tokio::sync::mpsc::channel::<zhuye_protocol::Event>(8);
        let gate = Arc::new(crate::approval::ApprovalGate::new(tx, "write-gate-deny"));
        let responder_gate = gate.clone();
        let responder = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if let EventMsg::ApprovalRequest(req) = event.msg {
                    responder_gate.complete(
                        &req.request_id,
                        ReviewDecision::Deny {
                            reason: "not now".into(),
                        },
                    );
                    break;
                }
            }
        });
        let mut tctx = make_ctx(&ws);
        tctx.approval = Some(gate);
        let denied = probe.join("denied.txt");
        let err = WriteTool
            .execute(
                tctx,
                serde_json::json!({"path": denied.to_string_lossy(), "content": "pwned"}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::PermissionDenied { .. }),
            "拒绝后应报 PermissionDenied,得到: {err:?}"
        );
        responder.await.unwrap();
        assert!(!denied.exists(), "被拒的越界写不得落盘");
        cleanup_probe(&probe);
    }

    /// 指向写根之外**已存在**文件的 symlink 必须被拒。
    /// component-walk 对每个现存 component 做 canonicalize,所以这条
    /// symlink 会被解析到真实路径,被最终校验拦下。
    #[tokio::test]
    async fn rejects_symlink_to_existing_outside_file() {
        let probe = outside_roots_dir("link");
        let outside_file = probe.join("exists.txt");
        std::fs::write(&outside_file, "original").unwrap();
        #[cfg(not(unix))]
        {
            // 非 Unix(Windows):创建 symlink 需要管理员/开发者模式,
            // 本回归仅在 Unix 上执行。
            let _ = outside_file;
            cleanup_probe(&probe);
            return;
        }
        #[cfg(unix)]
        {
            // ws 绑定在 cfg(unix) 块内:Windows 下提前 return,避免未使用告警。
            let ws = tmp_workspace();
            std::os::unix::fs::symlink(&outside_file, ws.join("link.txt")).unwrap();
            let t = WriteTool;
            let err = t
                .execute(
                    make_ctx(&ws),
                    serde_json::json!({"path": "link.txt", "content": "pwned"}),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(err, ToolError::PathEscape { .. }),
                "指向外部已存在文件的 symlink 必须被拒,得到: {err:?}"
            );
            assert_eq!(
                std::fs::read_to_string(&outside_file).unwrap(),
                "original",
                "symlink 目标内容不得被改写"
            );
            cleanup_probe(&probe);
        }
    }

    /// 回归(0.2):**悬空** symlink(target 不存在)不得把文件写到 workspace 外。
    ///
    /// 这条依赖 temp+rename 原子落盘:`fs::write` 会跟随悬空 symlink 在
    /// workspace 外创建目标文件(旧实现的逃逸路径),而 `rename` 是**替换**
    /// symlink 本身,写入落在 workspace 内。因此这里不强制要求报错,只
    /// 断言 workspace 外不得出现新文件。
    #[tokio::test]
    async fn dangling_symlink_does_not_write_outside() {
        let ws = tmp_workspace();
        let outside_file = ws.parent().unwrap().join("zhuye_write_dangling_target.txt");
        let _ = std::fs::remove_file(&outside_file);
        #[cfg(not(unix))]
        {
            // 非 Unix(Windows):创建 symlink 需要管理员/开发者模式,
            // 本回归仅在 Unix 上执行。
            let _ = outside_file;
            return;
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside_file, ws.join("dangling.txt")).unwrap();
            let t = WriteTool;
            let out = t
                .execute(
                    make_ctx(&ws),
                    serde_json::json!({"path": "dangling.txt", "content": "pwned"}),
                )
                .await;
            assert!(
                !outside_file.exists(),
                "悬空 symlink 不得导致 workspace 外被写入: {}",
                outside_file.display()
            );
            // 落盘成功时,workspace 内的 symlink 已被替换为普通文件。
            if out.is_ok() {
                assert!(ws.join("dangling.txt").is_file());
                assert!(
                    !std::fs::symlink_metadata(ws.join("dangling.txt"))
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
            }
        }
    }

    /// 回归(0.2):workspace 内的正常相对路径仍须放行(防过度收紧)。
    #[tokio::test]
    async fn still_allows_in_workspace_relative_path() {
        let ws = tmp_workspace();
        let t = WriteTool;
        let out = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "sub/deep/a.txt", "content": "ok"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "workspace 内相对路径应放行: {out:?}");
        assert_eq!(
            std::fs::read_to_string(ws.join("sub/deep/a.txt")).unwrap(),
            "ok"
        );
    }

    /// 回归:目录目标应给出清晰错误而非 Io 噪声。
    #[tokio::test]
    async fn rejects_directory_target() {
        let ws = tmp_workspace();
        std::fs::create_dir_all(ws.join("adir")).unwrap();
        let t = WriteTool;
        let err = t
            .execute(
                make_ctx(&ws),
                serde_json::json!({"path": "adir", "content": "x"}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgs { .. }),
            "目录目标应报 InvalidArgs,得到: {err:?}"
        );
    }

    /// 回归:原子落盘不得在 workspace 里留下 .zhuye-tmp 残留。
    #[tokio::test]
    async fn leaves_no_temp_file_behind() {
        let ws = tmp_workspace();
        let t = WriteTool;
        t.execute(
            make_ctx(&ws),
            serde_json::json!({"path": "a.txt", "content": "hello"}),
        )
        .await
        .unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(&ws)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("zhuye-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不得残留临时文件: {leftovers:?}");
    }
}
