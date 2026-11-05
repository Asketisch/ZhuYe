//! `zhuye-import` —— 外部 Harness 会话历史导入。
//!
//! 把 Claude Code / Codex / ZCode 的历史对话解析并转换为烛夜原生
//! rollout 会话(`~/.zhuye/sessions/YYYY/MM/DD/<thread_id>.jsonl`),
//! 导入产物即是普通会话:`zhuye session ls` 可见、
//! `zhuye exec --resume <id>` 可续跑、Desktop 按工作区归属展示。
//!
//! ## 分层
//!
//! ```text
//! ImportSource ──HarnessAdapter::parse──▶ ImportedSession(IR)
//!              ──convert::imported_to_records──▶ Vec<RolloutRecord>
//!              ──write::write_session──▶ ~/.zhuye/sessions/...
//! ```
//!
//! 每家来源一个 adapter(`adapters/`),中间表示与转换层共享,新增
//! 来源(如 DeepSeek)只加一个 adapter 模块 + 注册一行。
//!
//! ## 设计约束
//!
//! - **不进 SDK facade**:本 crate 是插件(`plugins/harness-import`)的
//!   能力核心,`zhuye` 门面与 `zhuye-cli` 不依赖它;正常使用 SDK 的
//!   项目零感知。CLI 用户经 `zhuye-import` 独立二进制(直跑模式)或
//!   插件 MCP 工具(`zhuye-import mcp`)使用。
//! - **幂等**:源 id 是合法 UUID 时原样保留为烛夜 thread id(去
//!   `sess_` 前缀),重复导入同一源会命中 `AlreadyExists`。
//! - **保真边界**:turn 内时序(文本/工具调用/工具结果)全保留;
//!   thinking 默认丢弃(`include_thinking` 折叠保留);Harness 注入的
//!   环境脚手架默认过滤(`keep_injected` 保留);加密 reasoning 无法
//!   解密,记 warning 跳过。
//! - **安全边界**:落盘复用 `zhuye-rollout::serialize_redacted`,密钥
//!   脱敏与 16 KiB 字段截断与原生会话一致。

pub mod adapter;
pub mod adapters;
pub mod convert;
pub mod error;
pub mod mcp_server;
pub mod model;
pub mod write;

pub use adapter::{HarnessAdapter, adapter_for, all_kinds, sniff};
pub use convert::{ConvertOptions, imported_to_records};
pub use error::ImportError;
pub use model::{
    Entry, HarnessKind, ImportOptions, ImportSource, ImportWarning, ImportedSession, ImportedTurn,
    ImportedUsage, IrBlock, UserBlock,
};
pub use write::{WriteReport, write_session};

use std::path::{Path, PathBuf};

/// 单个来源的导入结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ImportOutcome {
    /// 来源描述(文件路径或 `db#sess_key`)。
    pub source: String,
    /// 来源 id 原字符串。
    pub source_id: String,
    /// 目标烛夜 thread id。
    pub thread_id: zhuye_protocol::ThreadId,
    /// 落盘路径(`dry_run` 时为 `None`)。
    pub path: Option<PathBuf>,
    /// 导入的 turn 数。
    pub turns: usize,
    /// 记录条数(`dry_run` 时同样给出,便于预估)。
    pub records: usize,
    /// 警告列表。
    pub warnings: Vec<ImportWarning>,
}

impl ImportOutcome {
    /// 单行人类可读摘要(CLI 默认输出格式)。
    pub fn summary_line(&self) -> String {
        let target = self
            .path
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(dry-run)".to_string());
        format!(
            "imported {} -> {} ({} turns, {} records, {} warnings) -> {}",
            self.source_id,
            self.thread_id,
            self.turns,
            self.records,
            self.warnings.len(),
            target
        )
    }
}

/// 导入单个来源(parse → convert → write 完整管线)。
///
/// `kind` 为 `None` 时对 JSONL 来源自动嗅探(`--from auto`);ZCode 的
/// SQLite 来源必须显式 `HarnessKind::Zcode`。`dry_run` 只解析转换不落盘。
pub fn import_one(
    base: &Path,
    source: &ImportSource,
    kind: Option<HarnessKind>,
    opts: &ImportOptions,
    force: bool,
    dry_run: bool,
) -> Result<ImportOutcome, ImportError> {
    let resolved = match kind {
        Some(k) => k,
        None => match source {
            ImportSource::JsonlFile(p) => {
                sniff(p).ok_or_else(|| ImportError::UnknownFormat(p.display().to_string()))?
            }
            ImportSource::SqliteSession { .. } => {
                return Err(ImportError::Other(
                    "SQLite 来源必须显式指定 --from zcode".into(),
                ));
            }
        },
    };

    let adapter = adapter_for(resolved);
    let session = adapter.parse(source, opts)?;
    let records = imported_to_records(
        &session,
        &ConvertOptions {
            include_thinking: opts.include_thinking,
        },
    );

    let outcome = ImportOutcome {
        source: source.display(),
        source_id: session.source_id.clone(),
        thread_id: session.thread_id,
        path: None,
        turns: session.turn_count(),
        records: records.len(),
        warnings: session.warnings.clone(),
    };

    if dry_run {
        return Ok(outcome);
    }

    let report = write_session(base, &session, &records, force)?;
    Ok(ImportOutcome {
        path: Some(report.path),
        ..outcome
    })
}

/// 批量导入:逐源执行 [`import_one`],单源失败记入返回值而非中断
/// (`--all` 场景个别畸形会话不应拖垮整批)。
///
/// 返回 `(成功列表, 失败列表)`。
pub fn import_all(
    base: &Path,
    sources: &[ImportSource],
    kind: Option<HarnessKind>,
    opts: &ImportOptions,
    force: bool,
    dry_run: bool,
) -> (Vec<ImportOutcome>, Vec<(String, ImportError)>) {
    let mut ok = Vec::new();
    let mut failed = Vec::new();
    for s in sources {
        match import_one(base, s, kind, opts, force, dry_run) {
            Ok(o) => ok.push(o),
            // 幂等冲突单列(调用方通常想静默跳过)。
            Err(e) => failed.push((s.display(), e)),
        }
    }
    (ok, failed)
}

/// 按种类列出全部可导入来源(`--all`)。
pub fn discover(kind: HarnessKind) -> Result<Vec<ImportSource>, ImportError> {
    adapter_for(kind).discover()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 端到端:Claude Code 文件 → 本地会话,且可被原生 `replay` 回读。
    #[test]
    fn end_to_end_claude_file_to_local_session() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("abc.jsonl");
        fs::write(
            &src,
            r#"{"type":"user","message":{"role":"user","content":"第一问"},"sessionId":"cf9ee90f-0000-4000-8000-000000000003","uuid":"u1","timestamp":"2026-05-01T10:00:00Z","cwd":"/w/e2e"}
{"type":"assistant","message":{"role":"assistant","model":"test-model","content":[{"type":"text","text":"第一答"}],"usage":{"input_tokens":10,"output_tokens":5}},"sessionId":"cf9ee90f-0000-4000-8000-000000000003","timestamp":"2026-05-01T10:00:05Z"}"#,
        )
        .unwrap();

        let base = dir.path().join("sessions");
        let outcome = import_one(
            &base,
            &ImportSource::JsonlFile(src.clone()),
            None, // auto 嗅探
            &ImportOptions::default(),
            false,
            false,
        )
        .unwrap();
        assert_eq!(outcome.turns, 1);
        assert_eq!(
            outcome.thread_id,
            zhuye_protocol::ThreadId::parse_str("cf9ee90f-0000-4000-8000-000000000003").unwrap()
        );

        // 产物可被原生 reader 按文件名命中(索引契约)。
        let found = zhuye_rollout::index::find_session_path(&base, outcome.thread_id)
            .expect("按 id 找不到刚导入的会话");
        let records = crate::write::read_records_sync(&found);
        assert!(records.len() >= 3, "meta + user + assistant + token_count");

        // 幂等:重复导入命中 AlreadyExists。
        let again = import_one(
            &base,
            &ImportSource::JsonlFile(src),
            None,
            &ImportOptions::default(),
            false,
            false,
        );
        assert!(matches!(again, Err(ImportError::AlreadyExists(_))));
    }

    /// auto 嗅探失败报 UnknownFormat。
    #[test]
    fn unknown_format_reported() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("x.jsonl");
        fs::write(&src, "{\"hello\":1}\n").unwrap();
        let err = import_one(
            dir.path(),
            &ImportSource::JsonlFile(src),
            None,
            &ImportOptions::default(),
            false,
            true,
        )
        .unwrap_err();
        assert!(matches!(err, ImportError::UnknownFormat(_)));
    }

    /// dry_run 不落盘。
    #[test]
    fn dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("abc.jsonl");
        fs::write(
            &src,
            r#"{"type":"user","message":{"role":"user","content":"q"},"sessionId":"df9ee90f-0000-4000-8000-000000000004","uuid":"u","timestamp":"2026-05-01T10:00:00Z","cwd":"/w"}"#,
        )
        .unwrap();
        let base = dir.path().join("sessions");
        let o = import_one(
            &base,
            &ImportSource::JsonlFile(src),
            None,
            &ImportOptions::default(),
            false,
            true,
        )
        .unwrap();
        assert!(o.path.is_none());
        assert!(!base.exists(), "dry-run 不得建目录");
    }
}
