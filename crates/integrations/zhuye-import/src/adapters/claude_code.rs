//! Claude Code 会话文件 adapter。
//!
//! 源格式(`~/.claude/projects/<escaped-cwd>/<session-uuid>.jsonl`,逐行
//! JSON,本 crate 按本机 v2.x 实测逆向;格式为向前兼容解析 —— 未知
//! `type` 行一律跳过):
//!
//! ```json
//! {"type":"user","uuid":"…","parentUuid":"…","sessionId":"<uuid>",
//!  "timestamp":"2026-10-10T08:30:54.948Z","cwd":"/abs/cwd","cwd","gitBranch":"…",
//!  "isSidechain":false,"isMeta":true?,
//!  "message":{"role":"user","content":"文本" | [block...]}}
//! {"type":"assistant","uuid":"…","message":{"role":"assistant",
//!  "model":"…","content":[{"type":"text"|"thinking"|"tool_use",…}],
//!  "usage":{"input_tokens":…,"cache_read_input_tokens":…,"output_tokens":…}}}
//! {"type":"summary","summary":"…","leafUuid":"…"}
//! {"type":"ai-title","aiTitle":"…","sessionId":"…"}
//! {"type":"system"|"attachment"|"file-history-snapshot"|"cost-state"|"mode"|
//!  "permission-mode"|"last-prompt", …}
//! ```
//!
//! 映射规则:
//! - 真实 user 行(非 `isMeta`、非 tool_result)开启新 turn;content 为
//!   string 时即 prompt 文本,为数组时取其中 text / image 块;
//! - `tool_result` 在 **user 行**里,并入当前 turn 的 assistant 时序;
//! - 连续 assistant 行(一次 API 响应被拆多行落盘)折叠进当前 turn;
//! - `isCompactSummary:true` 的 user 行是压缩摘要 → `Compaction` 条目
//!   (`type:"summary"` 行只是标题索引,忽略);
//! - `isSidechain:true` 子代理行默认跳过;
//! - `ai-title` → 会话标题;`cwd` → workspace;首个 assistant
//!   `message.model` → 模型。

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;
use zhuye_protocol::ThreadId;

use crate::adapter::HarnessAdapter;
use crate::error::ImportError;
use crate::model::{
    Entry, HarnessKind, ImportOptions, ImportSource, ImportWarning, ImportedSession, ImportedTurn,
    ImportedUsage, IrBlock, UserBlock, file_stem, parse_timestamp,
};

/// Claude Code 的 user 行注入前缀(命令脚手架,非真实用户输入)。
const INJECTED_PREFIXES: &[&str] = &[
    "<command-name>",
    "<command-message>",
    "<local-command",
    "Caveat:",
    "<system-reminder>",
];

pub struct ClaudeCodeAdapter;

impl HarnessAdapter for ClaudeCodeAdapter {
    fn kind(&self) -> HarnessKind {
        HarnessKind::ClaudeCode
    }

    /// 判别特征:Claude 行带顶层 `sessionId` 字段 + 字符串 `type`。
    /// 不要求 `uuid`:真实文件常以 `last-prompt` / `mode` / `summary` 等
    /// 无 `uuid` 的行开头。Codex 首行(`session_meta`)无顶层
    /// `sessionId`(其 id 在 `payload` 内),不会误命中。
    fn probe(&self, path: &Path) -> bool {
        let Some(first) = first_non_empty_line(path) else {
            return false;
        };
        let Ok(v) = serde_json::from_str::<Value>(&first) else {
            return false;
        };
        v.get("type").and_then(Value::as_str).is_some()
            && v.get("type").map(Value::is_string).unwrap_or(false)
            && v.get("sessionId").is_some()
    }

    fn parse(
        &self,
        source: &ImportSource,
        opts: &ImportOptions,
    ) -> Result<ImportedSession, ImportError> {
        let path = match source {
            ImportSource::JsonlFile(p) => p.clone(),
            ImportSource::SqliteSession { .. } => {
                return Err(ImportError::Other(
                    "claude-code 来源必须是 JSONL 文件,SQLite 来源属于 zcode".into(),
                ));
            }
        };
        let content = std::fs::read_to_string(&path)?;
        let display = path.display().to_string();

        parse_lines(&content, &display, file_stem(&path), opts)
    }

    fn discover(&self) -> Result<Vec<ImportSource>, ImportError> {
        // 默认安装位置:$HOME/.claude/projects/<escaped-cwd>/<uuid>.jsonl。
        let mut out = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            let projects = PathBuf::from(home).join(".claude").join("projects");
            if let Ok(entries) = std::fs::read_dir(&projects) {
                let mut project_dirs: Vec<_> = entries.flatten().collect();
                project_dirs.sort_by_key(|e| e.file_name());
                for pd in project_dirs {
                    let Ok(files) = std::fs::read_dir(pd.path()) else {
                        continue;
                    };
                    let mut files: Vec<_> = files.flatten().collect();
                    files.sort_by_key(|e| e.file_name());
                    for f in files {
                        let p = f.path();
                        if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                            out.push(ImportSource::JsonlFile(p));
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}

/// 行级解析主循环(独立成函数便于单测直接喂字符串)。
pub(crate) fn parse_lines(
    content: &str,
    display: &str,
    file_stem_id: Option<&str>,
    opts: &ImportOptions,
) -> Result<ImportedSession, ImportError> {
    let mut warnings: Vec<ImportWarning> = Vec::new();

    // ── 会话标识:文件名 stem 优先,回退首行 sessionId ──
    let thread_id: Option<ThreadId> =
        file_stem_id.and_then(|s| uuid::Uuid::parse_str(s).ok().map(ThreadId));
    let mut first_session_id: Option<String> = None;
    let mut started_at: Option<DateTime<Utc>> = None;
    let mut workspace: Option<String> = None;
    let mut model: Option<String> = None;
    let mut title: Option<String> = None;

    let mut entries: Vec<Entry> = Vec::new();
    // keep_injected 时的注入行缓冲:挂到下一个真实 user 输入所在轮。
    let mut pending: Vec<String> = Vec::new();
    let mut current: Option<ImportedTurn> = None;

    for (lineno, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            warnings.push(ImportWarning::new(
                "malformed-line",
                format!("{display}:{lineno} 不是合法 JSON,已跳过"),
            ));
            continue;
        };

        // 首个可用的会话级字段在任何行上补齐(文件顺序即时间序)。
        if first_session_id.is_none() {
            first_session_id = v
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if started_at.is_none() {
            started_at = v
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_timestamp);
        }
        if workspace.is_none() {
            workspace = v.get("cwd").and_then(Value::as_str).map(str::to_string);
        }

        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "ai-title" => {
                title = v
                    .get("aiTitle")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or(title);
            }
            "user" => {
                // 子代理行(同一文件里交织的主/子轨迹)默认剔除。
                let sidechain = v.get("isSidechain").and_then(Value::as_bool) == Some(true);
                if sidechain && !opts.include_sidechain {
                    continue;
                }
                let message = v.get("message").cloned().unwrap_or(Value::Null);
                let role = message.get("role").and_then(Value::as_str).unwrap_or("");
                if role != "user" {
                    continue;
                }
                let is_compact = v.get("isCompactSummary").and_then(Value::as_bool) == Some(true);
                let is_meta = v.get("isMeta").and_then(Value::as_bool) == Some(true);
                let ts = v
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .and_then(parse_timestamp);
                let content = message.get("content").cloned().unwrap_or(Value::Null);

                if is_compact {
                    // 压缩摘要:真实可续跑上下文 → Compaction 条目。
                    if let Some(summary) = extract_text(&content) {
                        flush(&mut entries, &mut current);
                        entries.push(Entry::Compaction { summary, at: ts });
                    }
                    continue;
                }
                if is_meta || (matches!(content, Value::String(ref s) if is_injected(s))) {
                    // "Caveat: …" 等注入行。默认剔除;keep_injected 时缓冲,
                    // 前置到下一个真实 user 输入所在轮 —— 独立成轮会产生
                    // 「只有 user 无 assistant」的孤立消息,且与下一条 user
                    // 连续(部分 provider 拒绝连续同角色消息)。
                    if !opts.keep_injected {
                        continue;
                    }
                    if let Some(t) = extract_text(&content) {
                        pending.push(t);
                    }
                    continue;
                }

                match content {
                    Value::String(s) => {
                        // 注入串已在上方统一处理(过滤或缓冲),这里必然是
                        // 真实用户输入。
                        flush(&mut entries, &mut current);
                        let mut turn = ImportedTurn::empty();
                        turn.started_at = ts;
                        // 前置挂缓冲的注入上下文(见 is_meta 分支)。
                        for ctx in pending.drain(..) {
                            turn.user.push(UserBlock::Text { text: ctx });
                        }
                        turn.user.push(UserBlock::Text { text: s });
                        current = Some(turn);
                    }
                    Value::Array(blocks) => {
                        // 数组形态:tool_result 归当前 turn;text/image 是
                        // 新 prompt(开新 turn)。
                        let mut prompt: Vec<UserBlock> = Vec::new();
                        let mut results: Vec<IrBlock> = Vec::new();
                        for b in &blocks {
                            match b.get("type").and_then(Value::as_str).unwrap_or("") {
                                "tool_result" => {
                                    results.push(map_tool_result(
                                        b,
                                        &mut warnings,
                                        display,
                                        lineno,
                                    ));
                                }
                                "text" => {
                                    let text = b
                                        .get("text")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_string();
                                    if !text.trim().is_empty() {
                                        prompt.push(UserBlock::Text { text });
                                    }
                                }
                                "image" => {
                                    if let Some(img) = map_image(b) {
                                        prompt.push(img);
                                    } else {
                                        warnings.push(ImportWarning::new(
                                            "unsupported-image",
                                            format!(
                                                "{display}:{lineno} 图片块缺少 base64 source,已跳过"
                                            ),
                                        ));
                                    }
                                }
                                _ => {}
                            }
                        }
                        if !prompt.is_empty() {
                            flush(&mut entries, &mut current);
                            let mut turn = ImportedTurn::empty();
                            turn.started_at = ts;
                            // 前置挂缓冲的注入上下文(见 is_meta 分支)。
                            for ctx in pending.drain(..) {
                                turn.user.push(UserBlock::Text { text: ctx });
                            }
                            turn.user.extend(prompt);
                            current = Some(turn);
                        }
                        // tool_result 紧随其后并入当前 turn(含 prompt 开新
                        // turn 的场景 —— 此时 result 属于新 turn,语义一致)。
                        if !results.is_empty() {
                            let turn = current.get_or_insert_with(|| {
                                let mut t = ImportedTurn::empty();
                                t.started_at = ts;
                                t
                            });
                            turn.assistant.extend(results);
                        }
                    }
                    _ => {}
                }
            }
            "assistant" => {
                let sidechain = v.get("isSidechain").and_then(Value::as_bool) == Some(true);
                if sidechain && !opts.include_sidechain {
                    continue;
                }
                let message = v.get("message").cloned().unwrap_or(Value::Null);
                if message.get("role").and_then(Value::as_str) != Some("assistant") {
                    continue;
                }
                if model.is_none() {
                    model = message
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                let ts = v
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .and_then(parse_timestamp);

                let turn = current.get_or_insert_with(|| {
                    let mut t = ImportedTurn::empty();
                    t.started_at = ts;
                    t
                });
                if let Some(arr) = message.get("content").and_then(Value::as_array) {
                    for b in arr {
                        match b.get("type").and_then(Value::as_str).unwrap_or("") {
                            "text" => {
                                let text = b
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string();
                                if !text.trim().is_empty() {
                                    turn.assistant.push(IrBlock::Text { text });
                                }
                            }
                            "thinking" => {
                                // thinking 字段优先(标准),回退 text。
                                let text = b
                                    .get("thinking")
                                    .or_else(|| b.get("text"))
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string();
                                if !text.is_empty() {
                                    turn.assistant.push(IrBlock::Thinking { text });
                                }
                            }
                            "tool_use" => {
                                turn.assistant.push(IrBlock::ToolUse {
                                    id: b
                                        .get("id")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_string(),
                                    name: b
                                        .get("name")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_string(),
                                    args: b.get("input").cloned().unwrap_or(Value::Null),
                                });
                            }
                            _ => {}
                        }
                    }
                }
                // usage:同一 API 响应拆多行时各行重复,取最后一次即可。
                if let Some(u) = message.get("usage") {
                    let input = u.get("input_tokens").and_then(Value::as_u64);
                    let output = u.get("output_tokens").and_then(Value::as_u64);
                    if input.is_some() || output.is_some() {
                        turn.usage = Some(ImportedUsage {
                            input_tokens: input.unwrap_or(0),
                            output_tokens: output.unwrap_or(0),
                            cached_tokens: u
                                .get("cache_read_input_tokens")
                                .and_then(Value::as_u64)
                                .unwrap_or(0),
                        });
                    }
                }
            }
            // summary(标题索引)/ system / attachment / 其余审计行:忽略。
            _ => {}
        }
    }
    flush(&mut entries, &mut current);

    // ── 定型 thread_id ──
    let thread_id = match thread_id.or_else(|| {
        first_session_id
            .as_deref()
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
            .map(ThreadId)
    }) {
        Some(t) => t,
        None => {
            warnings.push(ImportWarning::new(
                "invalid-uuid",
                "源会话 id 不是合法 UUID,已分配新的 thread id",
            ));
            ThreadId::new()
        }
    };

    if entries.is_empty() {
        return Err(ImportError::EmptySession(display.to_string()));
    }

    Ok(ImportedSession {
        kind: HarnessKind::ClaudeCode,
        source_id: first_session_id.unwrap_or_else(|| display.to_string()),
        thread_id,
        model: model.unwrap_or_default(),
        started_at: started_at.unwrap_or_else(Utc::now),
        workspace,
        title,
        entries,
        warnings,
    })
}

/// 收尾当前 turn(非空才入列)。
fn flush(entries: &mut Vec<Entry>, current: &mut Option<ImportedTurn>) {
    if let Some(t) = current.take() {
        if !t.is_empty() {
            entries.push(Entry::Turn(t));
        }
    }
}

/// user 行文本是否是注入脚手架。
fn is_injected(s: &str) -> bool {
    INJECTED_PREFIXES.iter().any(|p| s.starts_with(p))
}

/// 从 content(string 或 [text 块数组])提取拼接文本。
fn extract_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()).filter(|s| !s.trim().is_empty()),
        Value::Array(blocks) => {
            let joined: Vec<&str> = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            let joined = joined.join("\n");
            Some(joined).filter(|s| !s.trim().is_empty())
        }
        _ => None,
    }
}

/// user 行里的 tool_result 块 → IrBlock。content 为 string 或
/// `[{type:"text"|"image",…}]`;图片降级为占位文本(结果侧不保真)。
fn map_tool_result(
    b: &Value,
    warnings: &mut Vec<ImportWarning>,
    display: &str,
    lineno: usize,
) -> IrBlock {
    let content = b.get("content").cloned().unwrap_or(Value::Null);
    let output = match &content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            let mut texts: Vec<String> = Vec::new();
            for p in parts {
                match p.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text" => {
                        texts.push(
                            p.get("text")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .into(),
                        );
                    }
                    "image" => texts.push("[图片结果未保留]".into()),
                    _ => {}
                }
            }
            texts.join("\n")
        }
        _ => String::new(),
    };
    if content.is_null() {
        warnings.push(ImportWarning::new(
            "empty-tool-result",
            format!("{display}:{lineno} tool_result 无 content,记为空输出"),
        ));
    }
    IrBlock::ToolResult {
        call_id: b
            .get("tool_use_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        output,
        is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
    }
}

/// Claude 图片块 `{"type":"image","source":{"type":"base64","media_type":…,
/// "data":…}}` → 字节;非 base64 源返回 None。
fn map_image(b: &Value) -> Option<UserBlock> {
    let source = b.get("source")?;
    if source.get("type").and_then(Value::as_str) != Some("base64") {
        return None;
    }
    let mime = source
        .get("media_type")
        .and_then(Value::as_str)
        .unwrap_or("image/png")
        .to_string();
    let b64 = source.get("data").and_then(Value::as_str)?;
    let data = decode_base64(b64)?;
    Some(UserBlock::Image {
        data,
        mime_type: mime,
    })
}

/// 无依赖 base64 解码(标准字母表 + padding 容错)。
fn decode_base64(s: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = TABLE.iter().position(|&t| t == c)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

/// 读取文件首个非空行(嗅探用,最多读 64 KiB 防御超大行)。
pub(crate) fn first_non_empty_line(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 64 * 1024];
    let n = f.read(&mut buf).ok()?;
    let head = String::from_utf8_lossy(&buf[..n]);
    head.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"
{"type":"summary","summary":"旧标题","leafUuid":"8d6954fe-a8e2-4d10-b212-7066f62a784a"}
{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"和当前项目无关,本机装了 tailscale"},"uuid":"162b240c-1111-1111-1111-111111111111","timestamp":"2026-10-10T08:28:10.000Z","cwd":"/work/demo","sessionId":"2f9ee90f-bbc0-41d8-b072-35cb085a7017","version":"2.0.0"}
{"type":"ai-title","aiTitle":"Tailscale SSH 连接","sessionId":"2f9ee90f-bbc0-41d8-b072-35cb085a7017"}
{"parentUuid":"162b240c","isSidechain":false,"type":"assistant","message":{"role":"assistant","model":"qwen3.8-flash-next","content":[{"type":"thinking","thinking":"用户在问 tailscale"}],"uuid":"04f0cb3d"},"timestamp":"2026-10-10T08:28:20.000Z","sessionId":"2f9ee90f-bbc0-41d8-b072-35cb085a7017"}
{"parentUuid":"04f0cb3d","isSidechain":false,"type":"assistant","message":{"role":"assistant","model":"qwen3.8-flash-next","content":[{"type":"text","text":" tailscale ping 用的是 tailnet "},{"type":"tool_use","id":"chatcmpl-tool-bbeb9cd916ec1b66","name":"Bash","input":{"command":"tailscale ping 100.124.8.17","description":"ping"}}],"usage":{"input_tokens":2503,"cache_read_input_tokens":38400,"output_tokens":1243}},"timestamp":"2026-10-10T08:28:21.000Z","sessionId":"2f9ee90f-bbc0-41d8-b072-35cb085a7017"}
{"parentUuid":"586484e6","isSidechain":false,"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"chatcmpl-tool-bbeb9cd916ec1b66","is_error":false,"content":"100.124.8.17 aitopatom-ebe4"}]},"timestamp":"2026-10-10T08:28:25.000Z","sessionId":"2f9ee90f-bbc0-41d8-b072-35cb085a7017"}
{"parentUuid":"162b240c","isSidechain":false,"type":"user","isMeta":true,"message":{"role":"user","content":"Caveat: 注入提示,应被过滤"},"uuid":"meta-1","timestamp":"2026-10-10T08:28:26.000Z","sessionId":"2f9ee90f-bbc0-41d8-b072-35cb085a7017"}
{"parentUuid":"586484e6","isSidechain":false,"type":"assistant","message":{"role":"assistant","model":"qwen3.8-flash-next","content":[{"type":"text","text":"通了,ssh 22 也已放行。"}],"usage":{"input_tokens":2600,"cache_read_input_tokens":38400,"output_tokens":80}},"timestamp":"2026-10-10T08:28:30.000Z","sessionId":"2f9ee90f-bbc0-41d8-b072-35cb085a7017"}
{"parentUuid":"x","isSidechain":true,"type":"user","message":{"role":"user","content":"子代理输入,默认剔除"},"uuid":"side-1","timestamp":"2026-10-10T08:28:31.000Z","sessionId":"2f9ee90f-bbc0-41d8-b072-35cb085a7017"}
{"parentUuid":"y","isSidechain":false,"type":"user","message":{"role":"user","content":"第二问:怎么访问端口页面"},"uuid":"20c5058f","timestamp":"2026-10-10T08:29:00.000Z","sessionId":"2f9ee90f-bbc0-41d8-b072-35cb085a7017"}
{"parentUuid":"20c5058f","isSidechain":false,"type":"assistant","message":{"role":"assistant","model":"qwen3.8-flash-next","content":[{"type":"text","text":"用 tailscale serve。"}]},"timestamp":"2026-10-10T08:29:05.000Z","sessionId":"2f9ee90f-bbc0-41d8-b072-35cb085a7017"}
{"type":"system","subtype":"turn_duration","durationMs":160172,"messageCount":15,"timestamp":"2026-10-10T08:30:54.948Z","uuid":"7be118ce","isMeta":false,"sessionId":"2f9ee90f-bbc0-41d8-b072-35cb085a7017"}
"#;

    /// 主干链路:turn 切分、thinking/tool_use/tool_result 归位、
    /// meta/子代理行过滤、ai-title/cwd/model 提取。
    #[test]
    fn parses_turns_tools_and_metadata() {
        let opts = ImportOptions::default();
        let s = parse_lines(
            FIXTURE,
            "fixture.jsonl",
            Some("2f9ee90f-bbc0-41d8-b072-35cb085a7017"),
            &opts,
        )
        .unwrap();

        assert_eq!(
            s.thread_id,
            ThreadId::parse_str("2f9ee90f-bbc0-41d8-b072-35cb085a7017").unwrap()
        );
        assert_eq!(s.title.as_deref(), Some("Tailscale SSH 连接"));
        assert_eq!(s.workspace.as_deref(), Some("/work/demo"));
        assert_eq!(s.model, "qwen3.8-flash-next");
        assert_eq!(s.turn_count(), 2, "两问两答");

        let Entry::Turn(t1) = &s.entries[0] else {
            panic!("首条应是 Turn");
        };
        assert_eq!(t1.user.len(), 1);
        let UserBlock::Text { text } = &t1.user[0] else {
            panic!("user 应是文本");
        };
        assert!(text.contains("tailscale"));
        // thinking + text + tool_use + tool_result + text 时序折叠。
        let kinds: Vec<&str> = t1
            .assistant
            .iter()
            .map(|b| match b {
                IrBlock::Text { .. } => "text",
                IrBlock::Thinking { .. } => "thinking",
                IrBlock::ToolUse { .. } => "tool_use",
                IrBlock::ToolResult { .. } => "tool_result",
            })
            .collect();
        assert_eq!(
            kinds,
            ["thinking", "text", "tool_use", "tool_result", "text"]
        );
        match &t1.assistant[2] {
            IrBlock::ToolUse { id, name, args } => {
                assert_eq!(id, "chatcmpl-tool-bbeb9cd916ec1b66");
                assert_eq!(name, "Bash");
                assert_eq!(args["command"], "tailscale ping 100.124.8.17");
            }
            other => panic!("{other:?}"),
        }
        match &t1.assistant[3] {
            IrBlock::ToolResult {
                call_id,
                output,
                is_error,
            } => {
                assert_eq!(call_id, "chatcmpl-tool-bbeb9cd916ec1b66");
                assert_eq!(output, "100.124.8.17 aitopatom-ebe4");
                assert!(!is_error);
            }
            other => panic!("{other:?}"),
        }
        // usage 取该 turn 最后一次上报。
        assert_eq!(t1.usage.map(|u| u.input_tokens), Some(2600));
    }

    /// isMeta / sidechain 默认过滤;include_sidechain 放开子代理。
    #[test]
    fn filters_meta_and_sidechain() {
        let opts = ImportOptions::default();
        let s = parse_lines(FIXTURE, "f.jsonl", None, &opts).unwrap();
        let has_sidechain = s.entries.iter().any(|e| {
            matches!(e, Entry::Turn(t) if t.user.iter().any(|u| matches!(u, UserBlock::Text{text} if text.contains("子代理输入"))))
        });
        assert!(!has_sidechain, "sidechain 行不应出现");
        // 文件名 stem 缺失时回退首行 sessionId。
        assert_eq!(
            s.thread_id,
            ThreadId::parse_str("2f9ee90f-bbc0-41d8-b072-35cb085a7017").unwrap()
        );

        let opts_open = ImportOptions {
            include_sidechain: true,
            ..Default::default()
        };
        let s2 = parse_lines(FIXTURE, "f.jsonl", None, &opts_open).unwrap();
        let has = s2.entries.iter().any(|e| {
            matches!(e, Entry::Turn(t) if t.user.iter().any(|u| matches!(u, UserBlock::Text{text} if text.contains("子代理输入"))))
        });
        assert!(has, "include_sidechain 时子代理轮应保留");
    }

    /// 压缩摘要行 → Compaction 条目。
    #[test]
    fn compact_summary_becomes_compaction() {
        let fixture = r#"
{"type":"user","message":{"role":"user","content":"第一问"},"uuid":"a","sessionId":"6f9ee90f-0000-0000-0000-000000000000","timestamp":"2026-01-01T00:00:00Z","cwd":"/w"}
{"type":"assistant","message":{"role":"assistant","model":"m","content":[{"type":"text","text":"答"}]},"sessionId":"6f9ee90f-0000-0000-0000-000000000000","timestamp":"2026-01-01T00:00:05Z"}
{"type":"user","isCompactSummary":true,"message":{"role":"user","content":"This session is being continued from a previous conversation… 摘要正文"},"uuid":"b","sessionId":"6f9ee90f-0000-0000-0000-000000000000","timestamp":"2026-01-01T01:00:00Z","cwd":"/w"}
{"type":"user","message":{"role":"user","content":"继续问"},"uuid":"c","sessionId":"6f9ee90f-0000-0000-0000-000000000000","timestamp":"2026-01-01T01:00:10Z","cwd":"/w"}
"#;
        let s = parse_lines(fixture, "f.jsonl", None, &ImportOptions::default()).unwrap();
        let kinds: Vec<&str> = s
            .entries
            .iter()
            .map(|e| match e {
                Entry::Turn(_) => "turn",
                Entry::Compaction { .. } => "compaction",
            })
            .collect();
        assert_eq!(kinds, ["turn", "compaction", "turn"]);
        match &s.entries[1] {
            Entry::Compaction { summary, .. } => {
                assert!(summary.contains("摘要正文"));
            }
            other => panic!("{other:?}"),
        }
    }

    /// probe:带 sessionId 的行命中;Codex session_meta / 纯 JSON 不命中。
    #[test]
    fn probe_discriminates() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("x.jsonl");
        std::fs::write(
            &claude,
            r#"{"type":"user","sessionId":"s","uuid":"u","message":{}}"#,
        )
        .unwrap();
        // 真实文件常以无 uuid 的行开头(last-prompt / mode / summary)。
        let claude2 = dir.path().join("x2.jsonl");
        std::fs::write(
            &claude2,
            r#"{"type":"last-prompt","leafUuid":"l","sessionId":"s"}"#,
        )
        .unwrap();
        let codex = dir.path().join("y.jsonl");
        std::fs::write(
            &codex,
            r#"{"timestamp":"t","type":"session_meta","payload":{"id":"x"}}"#,
        )
        .unwrap();
        let plain = dir.path().join("p.jsonl");
        std::fs::write(&plain, r#"{"hello":1}"#).unwrap();
        assert!(ClaudeCodeAdapter.probe(&claude));
        assert!(ClaudeCodeAdapter.probe(&claude2));
        assert!(!ClaudeCodeAdapter.probe(&codex));
        assert!(!ClaudeCodeAdapter.probe(&plain));
    }

    /// base64 图片解成字节进 user 块。
    #[test]
    fn image_block_decoded() {
        // "hi" 的 base64 是 "aGk="。
        let fixture = r#"
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"看图"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGk="}}]},"uuid":"a","sessionId":"7f9ee90f-0000-0000-0000-000000000000","timestamp":"2026-01-01T00:00:00Z","cwd":"/w"}
"#;
        let s = parse_lines(fixture, "f.jsonl", None, &ImportOptions::default()).unwrap();
        let Entry::Turn(t) = &s.entries[0] else {
            panic!();
        };
        assert_eq!(t.user.len(), 2);
        match &t.user[1] {
            UserBlock::Image { data, mime_type } => {
                assert_eq!(data, b"hi");
                assert_eq!(mime_type, "image/png");
            }
            other => panic!("{other:?}"),
        }
    }

    /// 畸形 JSON 行跳过并记 warning,不整体失败。
    #[test]
    fn malformed_line_skipped_with_warning() {
        let fixture = "{not json}\n{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"问\"},\"sessionId\":\"8f9ee90f-0000-0000-0000-000000000000\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/w\"}\n";
        let s = parse_lines(fixture, "f.jsonl", None, &ImportOptions::default()).unwrap();
        assert_eq!(s.turn_count(), 1);
        assert!(s.warnings.iter().any(|w| w.code == "malformed-line"));
    }
}
