//! Codex rollout adapter(CLI / Desktop 共用,`~/.codex/sessions/**/rollout-*.jsonl`)。
//!
//! 源格式(按本机 Codex 0.166 实测逆向;向前兼容解析,未知 type 跳过):
//!
//! ```json
//! {"timestamp":"…","type":"session_meta","payload":{"id":"<uuid v7>","timestamp":"…",
//!  "cwd":"/abs/cwd","originator":"Codex Desktop","cli_version":"…","model_provider":"openai",…}}
//! {"timestamp":"…","type":"response_item","payload":{"type":"message","role":"user|assistant|developer",
//!  "content":[{"type":"input_text"|"output_text","text":"…"}],"phase"?:…}}
//! {"timestamp":"…","type":"response_item","payload":{"type":"function_call","name":"…",
//!  "call_id":"call_…","arguments":"{\"json\":\"str\"}"}}
//! {"timestamp":"…","type":"response_item","payload":{"type":"function_call_output","call_id":"…",
//!  "output":"文本" | {"content":"…",…}}}
//! {"timestamp":"…","type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch",
//!  "call_id":"…","input":"补丁原文"}}
//! {"timestamp":"…","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"…","output":"…"}}
//! {"timestamp":"…","type":"response_item","payload":{"type":"reasoning","summary":[
//!  {"type":"summary_text","text":"…"}],"encrypted_content"?:…}}
//! {"type":"compacted","payload":{"message":{…message 形态,正文是摘要…},"replacement_history":[…]}}
//! {"type":"turn_context","payload":{"cwd":"…","model":"openai/gpt-6.1-terra",…}}
//! {"type":"token_usage_record","payload":{"turn_token_usage":{…},"usage":{…},…}}
//! {"type":"event_msg"|"world_state"|"turn_context",…}  // 非对话内容
//! ```
//!
//! 映射规则:
//! - 真实 user `message`(文本不以注入标签开头)开启新 turn;`developer`
//!   角色与注入 user 行默认过滤(`keep_injected` 保留);
//! - `function_call.arguments` 是 JSON 字符串 → 解析为对象,失败降级
//!   `{"raw": 原文}`;`custom_tool_call.input`(补丁原文)→ `{"input": 原文}`;
//! - `reasoning.summary[].text` 拼接为 Thinking;仅有 `encrypted_content`
//!   的条目无法解密,记 warning 跳过;
//! - `compacted.payload.message` → `Compaction` 条目;
//! - `turn_context.model` → 会话模型;`session_meta.cwd` → workspace;
//! - `token_usage_record` 的轮用量挂在当前 turn。

use std::path::PathBuf;

use base64::Engine;
use chrono::{DateTime, Utc};
use serde_json::Value;
use zhuye_protocol::ThreadId;

use crate::adapter::HarnessAdapter;
use crate::adapters::claude_code::first_non_empty_line;
use crate::error::ImportError;
use crate::model::{
    Entry, HarnessKind, ImportOptions, ImportSource, ImportWarning, ImportedSession, ImportedTurn,
    ImportedUsage, IrBlock, UserBlock, parse_timestamp,
};

/// Codex 注入的 user 上下文前缀(运行时脚手架,非用户输入)。
const INJECTED_PREFIXES: &[&str] = &[
    "<environment_context>",
    "<user_instructions>",
    "<permissions instructions>",
    "<skills_instructions>",
    "<apps_instructions>",
    "<app_instructions>",
    "<collaboration_mode>",
    "<model_switch>",
    "<turn_aborted>",
    "<codex_internal_context>",
    "<turn_context>",
    "# AGENTS.md instructions",
];

pub struct CodexAdapter;

impl HarnessAdapter for CodexAdapter {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Codex
    }

    /// 判别特征:首行 `type == "session_meta"` 且带 `payload` 对象。
    fn probe(&self, path: &std::path::Path) -> bool {
        let Some(first) = first_non_empty_line(path) else {
            return false;
        };
        let Ok(v) = serde_json::from_str::<Value>(&first) else {
            return false;
        };
        v.get("type").and_then(Value::as_str) == Some("session_meta")
            && v.get("payload").is_some_and(Value::is_object)
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
                    "codex 来源必须是 JSONL 文件,SQLite 来源属于 zcode".into(),
                ));
            }
        };
        let content = std::fs::read_to_string(&path)?;
        parse_lines(&content, &path.display().to_string(), opts)
    }

    fn discover(&self) -> Result<Vec<ImportSource>, ImportError> {
        // 默认安装位置:$HOME/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl。
        let mut out = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            let root = PathBuf::from(home).join(".codex").join("sessions");
            collect_rollout_files(&root, &mut out);
        }
        out.sort_by_key(|s| s.display());
        Ok(out)
    }
}

/// 收集各级 JSONL；同一真实目录只遍历一次，避免符号链接形成环。
fn collect_rollout_files(dir: &std::path::Path, out: &mut Vec<ImportSource>) {
    let mut pending = vec![dir.to_path_buf()];
    let mut visited = std::collections::HashSet::new();
    while let Some(dir) = pending.pop() {
        let Ok(real_dir) = dir.canonicalize() else {
            continue;
        };
        if !visited.insert(real_dir) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut paths: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
        paths.sort();
        for path in paths.into_iter().rev() {
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                out.push(ImportSource::JsonlFile(path));
            }
        }
    }
}

/// 行级解析主循环。
pub(crate) fn parse_lines(
    content: &str,
    display: &str,
    opts: &ImportOptions,
) -> Result<ImportedSession, ImportError> {
    let mut warnings: Vec<ImportWarning> = Vec::new();

    let mut thread_id: Option<ThreadId> = None;
    let mut source_id: Option<String> = None;
    let mut started_at: Option<DateTime<Utc>> = None;
    let mut workspace: Option<String> = None;
    let mut model: Option<String> = None;

    let mut entries: Vec<Entry> = Vec::new();
    // keep_injected 时的注入脚手架缓冲:挂到下一个真实 user 输入所在轮。
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
        let ts = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp);

        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "session_meta" => {
                let payload = v.get("payload").cloned().unwrap_or(Value::Null);
                let id = payload
                    .get("id")
                    .or_else(|| payload.get("session_id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                source_id = Some(id.to_string());
                thread_id = uuid::Uuid::parse_str(id).ok().map(ThreadId);
                if started_at.is_none() {
                    started_at = payload
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .and_then(parse_timestamp)
                        .or(ts);
                }
                if workspace.is_none() {
                    workspace = payload
                        .get("cwd")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
            }
            "turn_context" => {
                let payload = v.get("payload").cloned().unwrap_or(Value::Null);
                if let Some(m) = payload.get("model").and_then(Value::as_str) {
                    model = Some(m.to_string());
                }
                if workspace.is_none() {
                    workspace = payload
                        .get("cwd")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
            }
            "response_item" => {
                let payload = v.get("payload").cloned().unwrap_or(Value::Null);
                match payload.get("type").and_then(Value::as_str).unwrap_or("") {
                    "message" => {
                        let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
                        let text = join_content_text(&payload).unwrap_or_default();
                        match role {
                            // developer 消息是运行时脚手架,默认剔除。
                            "developer" if !opts.keep_injected => continue,
                            "developer" | "user" => {
                                // 注入行(developer / <environment_context> 等
                                // 脚手架)默认剔除;keep_injected 时**缓冲**而非
                                // 立即开轮 —— 它们属于紧随其后的真实用户输入
                                // 那一轮(原会话里就是同一次请求的上下文),
                                // 独立成轮会产生「只有 user 无 assistant」的
                                // 孤立消息,且与下一条 user 连续,部分 provider
                                // 会拒绝。缓冲内容在真实输入到达时前置。
                                let injected =
                                    role == "developer" || (role == "user" && is_injected(&text));
                                if injected {
                                    if !opts.keep_injected || text.is_empty() {
                                        continue;
                                    }
                                    pending.push(text);
                                    continue;
                                }
                                let user_blocks = user_content_blocks(
                                    &payload,
                                    display,
                                    lineno + 1,
                                    &mut warnings,
                                );
                                if user_blocks.is_empty() {
                                    continue;
                                }
                                // 真实用户输入 → 开新 turn(前置挂缓冲的注入
                                // 上下文)。
                                if let Some(t) = current.take() {
                                    if !t.is_empty() {
                                        entries.push(Entry::Turn(t));
                                    }
                                }
                                let mut turn = ImportedTurn::empty();
                                turn.started_at = ts;
                                for ctx in pending.drain(..) {
                                    turn.user.push(UserBlock::Text { text: ctx });
                                }
                                turn.user.extend(user_blocks);
                                current = Some(turn);
                            }
                            "assistant" => {
                                if text.is_empty() {
                                    continue;
                                }
                                let turn = current.get_or_insert_with(|| {
                                    let mut t = ImportedTurn::empty();
                                    t.started_at = ts;
                                    t
                                });
                                turn.assistant.push(IrBlock::Text { text });
                            }
                            _ => {}
                        }
                    }
                    "function_call" | "custom_tool_call" => {
                        let name = payload
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let call_id = payload
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        // function_call.arguments 是 JSON 字符串;custom_tool_call
                        // input 是补丁原文 → 包一层 {"input": …}。
                        let args = if payload.get("type").and_then(Value::as_str)
                            == Some("custom_tool_call")
                        {
                            serde_json::json!({ "input": payload.get("input").and_then(Value::as_str).unwrap_or_default() })
                        } else {
                            match payload
                                .get("arguments")
                                .and_then(Value::as_str)
                                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                            {
                                Some(v) => v,
                                None => {
                                    warnings.push(ImportWarning::new(
                                        "unparsable-arguments",
                                        format!("{display}:{lineno} function_call.arguments 非 JSON,降级为 raw"),
                                    ));
                                    serde_json::json!({ "raw": payload.get("arguments").and_then(Value::as_str).unwrap_or_default() })
                                }
                            }
                        };
                        let turn = current.get_or_insert_with(|| {
                            let mut t = ImportedTurn::empty();
                            t.started_at = ts;
                            t
                        });
                        turn.assistant.push(IrBlock::ToolUse {
                            id: call_id,
                            name,
                            args,
                        });
                    }
                    "function_call_output" | "custom_tool_call_output" => {
                        let call_id = payload
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let output = output_to_text(payload.get("output"));
                        let turn = current.get_or_insert_with(|| {
                            let mut t = ImportedTurn::empty();
                            t.started_at = ts;
                            t
                        });
                        turn.assistant.push(IrBlock::ToolResult {
                            call_id,
                            output,
                            is_error: false,
                        });
                    }
                    "reasoning" => {
                        // summary[].text 拼接;仅 encrypted_content 的跳过。
                        let mut texts: Vec<String> = Vec::new();
                        if let Some(list) = payload.get("summary").and_then(Value::as_array) {
                            for s in list {
                                if let Some(t) = s.get("text").and_then(Value::as_str) {
                                    texts.push(t.to_string());
                                }
                            }
                        }
                        if texts.is_empty() {
                            if payload
                                .get("encrypted_content")
                                .and_then(Value::as_str)
                                .is_some()
                            {
                                warnings.push(ImportWarning::new(
                                    "encrypted-reasoning",
                                    format!("{display}:{lineno} reasoning 已加密,无法导入"),
                                ));
                            }
                            continue;
                        }
                        let turn = current.get_or_insert_with(|| {
                            let mut t = ImportedTurn::empty();
                            t.started_at = ts;
                            t
                        });
                        turn.assistant.push(IrBlock::Thinking {
                            text: texts.join("\n"),
                        });
                    }
                    _ => {}
                }
            }
            "compacted" => {
                // payload.message 是压缩摘要正文(message 形态)。
                let payload = v.get("payload").cloned().unwrap_or(Value::Null);
                let summary = join_content_text(&payload)
                    .or_else(|| payload.get("message").and_then(join_content_text))
                    .unwrap_or_default();
                if !summary.is_empty() {
                    if let Some(t) = current.take() {
                        if !t.is_empty() {
                            entries.push(Entry::Turn(t));
                        }
                    }
                    entries.push(Entry::Compaction { summary, at: ts });
                }
            }
            "token_usage_record" => {
                // 轮用量挂当前轮;轮不存在时忽略(线程累计无 per-turn 归属)。
                let payload = v.get("payload").cloned().unwrap_or(Value::Null);
                if let Some(usage) = extract_usage(&payload) {
                    if let Some(turn) = current.as_mut() {
                        turn.usage = Some(usage);
                    }
                }
            }
            // event_msg / world_state / 其余:非对话内容,忽略。
            _ => {}
        }
    }
    if let Some(t) = current.take() {
        if !t.is_empty() {
            entries.push(Entry::Turn(t));
        }
    }

    let thread_id = match thread_id {
        Some(t) => t,
        None => {
            warnings.push(ImportWarning::new(
                "invalid-uuid",
                "session_meta 缺少合法 uuid,已分配新的 thread id",
            ));
            ThreadId::new()
        }
    };
    if entries.is_empty() {
        return Err(ImportError::EmptySession(display.to_string()));
    }

    Ok(ImportedSession {
        kind: HarnessKind::Codex,
        source_id: source_id.unwrap_or_else(|| display.to_string()),
        thread_id,
        model: model.unwrap_or_default(),
        started_at: started_at.unwrap_or_else(Utc::now),
        workspace,
        title: None,
        entries,
        warnings,
    })
}

/// user 文本是否是注入脚手架。
fn is_injected(s: &str) -> bool {
    INJECTED_PREFIXES.iter().any(|p| s.starts_with(p))
}

/// 把 message 形态 payload 的 content[].text 拼接(input_text/output_text 兼容)。
fn join_content_text(payload: &Value) -> Option<String> {
    let content = payload.get("content")?.as_array()?;
    let joined: Vec<&str> = content
        .iter()
        .map(|c| c.get("text").and_then(Value::as_str).unwrap_or_default())
        .filter(|t| !t.is_empty())
        .collect();
    if joined.is_empty() {
        None
    } else {
        Some(joined.join("\n"))
    }
}

/// 按源顺序保留文本和内嵌图片；无法恢复的图片显式告警。
fn user_content_blocks(
    payload: &Value,
    display: &str,
    lineno: usize,
    warnings: &mut Vec<ImportWarning>,
) -> Vec<UserBlock> {
    let Some(content) = payload.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut blocks = Vec::new();
    for item in content {
        match item.get("type").and_then(Value::as_str) {
            Some("input_text" | "text") => {
                if let Some(text) = item
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    blocks.push(UserBlock::Text {
                        text: text.to_string(),
                    });
                }
            }
            Some("input_image" | "local_image") => {
                let url = item.get("image_url").and_then(|value| {
                    value
                        .as_str()
                        .or_else(|| value.get("url").and_then(Value::as_str))
                });
                let image = url.and_then(|url| {
                    let (header, encoded) = url.strip_prefix("data:")?.split_once(',')?;
                    let mime = header.strip_suffix(";base64")?;
                    if !mime.starts_with("image/") || encoded.is_empty() {
                        return None;
                    }
                    let data = base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .or_else(|_| {
                            base64::engine::general_purpose::STANDARD_NO_PAD.decode(encoded)
                        })
                        .ok()?;
                    Some(UserBlock::Image {
                        data,
                        mime_type: mime.to_string(),
                    })
                });
                if let Some(image) = image {
                    blocks.push(image);
                } else {
                    warnings.push(ImportWarning::new(
                        "unsupported-image",
                        format!("{display}:{lineno} 图片不是有效的内嵌 base64 数据,已跳过"),
                    ));
                }
            }
            _ => {
                if let Some(text) = item
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    blocks.push(UserBlock::Text {
                        text: text.to_string(),
                    });
                }
            }
        }
    }
    blocks
}

/// 工具输出统一文本化:字符串直取;对象优先 `content` 字段,否则紧凑 JSON。
fn output_to_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(_)) => {
            if let Some(c) = v.and_then(|o| o.get("content")).and_then(Value::as_str) {
                c.to_string()
            } else {
                serde_json::to_string(v.unwrap()).unwrap_or_default()
            }
        }
        Some(other) if !other.is_null() => serde_json::to_string(other).unwrap_or_default(),
        _ => String::new(),
    }
}

/// token_usage_record → ImportedUsage(优先 turn 级,退回 usage / 线程级)。
fn extract_usage(payload: &Value) -> Option<ImportedUsage> {
    for key in ["turn_token_usage", "usage", "thread_token_usage"] {
        if let Some(u) = payload.get(key) {
            let input = u.get("input_tokens").and_then(Value::as_u64);
            let output = u.get("output_tokens").and_then(Value::as_u64);
            if input.is_some() || output.is_some() {
                return Some(ImportedUsage {
                    input_tokens: input.unwrap_or(0),
                    output_tokens: output.unwrap_or(0),
                    cached_tokens: u
                        .get("cached_input_tokens")
                        .or_else(|| u.get("cached_tokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                });
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn discovery_handles_directory_link_cycles_and_aliases() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("2026/11");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("rollout.jsonl"), "{}").unwrap();
        symlink(root.path(), dir.join("cycle")).unwrap();
        symlink(root.path().join("2026"), root.path().join("alias")).unwrap();

        let mut sources = Vec::new();
        collect_rollout_files(root.path(), &mut sources);
        assert_eq!(sources.len(), 1);
        assert!(sources[0].display().ends_with("rollout.jsonl"));
    }

    #[test]
    fn imports_user_images_and_warns_for_unavailable_sources() {
        let content = concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"01a18f63-104f-7021-b325-72f576186023\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"前\"},{\"type\":\"input_image\",\"image_url\":\"data:image/png;base64,aGk=\"},{\"type\":\"input_text\",\"text\":\"后\"}]}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_image\",\"image_url\":{\"url\":\"data:image/jpeg;base64,AQI\"}}]}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"local_image\",\"path\":\"/missing\"}]}}"
        );
        let session = parse_lines(content, "rollout.jsonl", &ImportOptions::default()).unwrap();
        assert_eq!(session.turn_count(), 2);
        let Entry::Turn(first) = &session.entries[0] else {
            panic!("缺少首轮")
        };
        assert_eq!(
            first.user,
            vec![
                UserBlock::Text { text: "前".into() },
                UserBlock::Image {
                    data: b"hi".to_vec(),
                    mime_type: "image/png".into()
                },
                UserBlock::Text { text: "后".into() },
            ]
        );
        let Entry::Turn(second) = &session.entries[1] else {
            panic!("缺少第二轮")
        };
        assert_eq!(
            second.user,
            vec![UserBlock::Image {
                data: vec![1, 2],
                mime_type: "image/jpeg".into()
            }]
        );
        assert!(
            session
                .warnings
                .iter()
                .any(|warning| warning.code == "unsupported-image")
        );
    }

    const FIXTURE: &str = r#"
{"timestamp":"2026-10-31T00:36:36.637Z","ordinal":1,"type":"session_meta","payload":{"session_id":"01a18f63-104f-7021-b325-72f576186023","id":"01a18f63-104f-7021-b325-72f576186023","timestamp":"2026-10-31T00:36:36.584Z","cwd":"/work/cx","originator":"Codex Desktop","cli_version":"0.166.0","model_provider":"openai"}}
{"timestamp":"2026-10-31T00:36:40.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n沙箱信息\n</environment_context>"}]}}
{"timestamp":"2026-10-31T00:36:41.000Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<skills_instructions>技能注入</skills_instructions>"}]}}
{"timestamp":"2026-10-31T00:36:42.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"帮我看下进程"}]}}
{"timestamp":"2026-10-31T00:36:43.000Z","type":"turn_context","payload":{"cwd":"/work/cx","model":"openai/gpt-6.1-terra","effort":"high","approval_policy":"never"}}
{"timestamp":"2026-10-31T00:36:44.000Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"先列进程"}],"content":null}}
{"timestamp":"2026-10-31T00:36:45.000Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"我来查看进程。"}]}}
{"timestamp":"2026-10-31T00:36:46.000Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call_a1","arguments":"{\"cmd\":\"ps aux\"}"}}
{"timestamp":"2026-10-31T00:36:47.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_a1","output":"pid 1 …"}}
{"timestamp":"2026-10-31T00:36:48.000Z","type":"token_usage_record","payload":{"turn_token_usage":{"input_tokens":900,"cached_input_tokens":700,"output_tokens":120},"usage":{"input_tokens":900,"output_tokens":120}}}
{"timestamp":"2026-10-31T00:37:00.000Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"final","content":[{"type":"output_text","text":"进程列表如上。"}]}}
{"timestamp":"2026-10-31T00:38:00.000Z","type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","call_id":"call_p1","input":"*** Begin Patch"}}
{"timestamp":"2026-10-31T00:38:01.000Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_p1","output":"Done!"}}
{"timestamp":"2026-10-31T00:39:00.000Z","type":"compacted","payload":{"message":{"type":"message","role":"user","content":[{"type":"input_text","text":"<SUMMARY>此前对话摘要</SUMMARY>"}]}}}
{"timestamp":"2026-10-31T00:40:00.000Z","type":"event_msg","payload":{"type":"task_complete","last_agent_message":"完成"}}
"#;

    /// 主干:turn 切分、注入/developer 过滤、tool 配对、reasoning、
    /// compacted、usage、model/cwd 提取。
    #[test]
    fn parses_codex_rollout() {
        let s = parse_lines(FIXTURE, "rollout.jsonl", &ImportOptions::default()).unwrap();
        assert_eq!(
            s.thread_id,
            ThreadId::parse_str("01a18f63-104f-7021-b325-72f576186023").unwrap()
        );
        assert_eq!(s.model, "openai/gpt-6.1-terra");
        assert_eq!(s.workspace.as_deref(), Some("/work/cx"));
        assert_eq!(s.turn_count(), 1, "注入行不开启 turn;apply_patch 在同轮");

        let Entry::Turn(t) = &s.entries[0] else {
            panic!();
        };
        assert_eq!(t.user.len(), 1);
        let UserBlock::Text { text } = &t.user[0] else {
            panic!();
        };
        assert_eq!(text, "帮我看下进程");
        let kinds: Vec<&str> = t
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
            [
                "thinking",
                "text",
                "tool_use",
                "tool_result",
                "text",
                "tool_use",
                "tool_result"
            ]
        );
        // function_call.arguments 解析为对象。
        match &t.assistant[2] {
            IrBlock::ToolUse { name, args, .. } => {
                assert_eq!(name, "exec_command");
                assert_eq!(args["cmd"], "ps aux");
            }
            other => panic!("{other:?}"),
        }
        // custom_tool_call.input → {"input": …}。
        match &t.assistant[5] {
            IrBlock::ToolUse { name, args, .. } => {
                assert_eq!(name, "apply_patch");
                assert_eq!(args["input"], "*** Begin Patch");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(t.usage.map(|u| u.input_tokens), Some(900));
        assert_eq!(t.usage.map(|u| u.cached_tokens), Some(700));

        // compacted → Compaction。
        match s.entries.last().unwrap() {
            Entry::Compaction { summary, .. } => assert!(summary.contains("此前对话摘要")),
            other => panic!("{other:?}"),
        }
    }

    /// keep_injected 保留注入行(developer → user 上下文)。
    #[test]
    fn keep_injected_retains_scaffold() {
        let s = parse_lines(
            FIXTURE,
            "rollout.jsonl",
            &ImportOptions {
                keep_injected: true,
                ..Default::default()
            },
        )
        .unwrap();
        let user_texts: usize = s
            .entries
            .iter()
            .filter_map(|e| match e {
                Entry::Turn(t) => Some(t.user.len()),
                _ => None,
            })
            .sum();
        assert!(user_texts >= 3, "注入 user + developer + 真实 user");
    }

    /// keep_injected 的注入行必须**前置到真实 user 所在轮**,不能独立成轮
    /// (否则产生只有 user 无 assistant 的孤立消息,且与下一条 user 连续,
    /// provider 会拒绝)。回归:修复前注入行各自开轮。
    #[test]
    fn keep_injected_merges_into_real_turn() {
        let s = parse_lines(
            FIXTURE,
            "rollout.jsonl",
            &ImportOptions {
                keep_injected: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(s.turn_count(), 1, "注入不得独立成轮");
        let Entry::Turn(t) = &s.entries[0] else {
            panic!();
        };
        // 顺序:注入 env → 注入 skills → 真实输入。
        let texts: Vec<&str> = t
            .user
            .iter()
            .filter_map(|b| match b {
                UserBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts.len(), 3, "{texts:?}");
        assert!(texts[0].contains("沙箱"), "{texts:?}");
        assert!(texts[1].contains("技能"), "{texts:?}");
        assert_eq!(texts[2], "帮我看下进程");
        // 该轮仍有 assistant 内容(不是孤立 user 轮)。
        assert!(!t.assistant.is_empty());
    }

    /// arguments 非 JSON 降级 raw;encrypted reasoning 记 warning。
    #[test]
    fn degraded_arguments_and_encrypted_reasoning() {
        let fixture = r#"
{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"01a18f63-104f-7021-b325-72f576186099","cwd":"/w"}}
{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"q"}]}}
{"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"function_call","name":"x","call_id":"c1","arguments":"not-json"}}
{"timestamp":"2026-01-01T00:00:03Z","type":"response_item","payload":{"type":"reasoning","summary":[],"encrypted_content":"AAAA"}}
"#;
        let s = parse_lines(fixture, "r.jsonl", &ImportOptions::default()).unwrap();
        let Entry::Turn(t) = &s.entries[0] else {
            panic!();
        };
        match &t.assistant[0] {
            IrBlock::ToolUse { args, .. } => assert_eq!(args["raw"], "not-json"),
            other => panic!("{other:?}"),
        }
        assert!(s.warnings.iter().any(|w| w.code == "encrypted-reasoning"));
    }

    /// probe:Codex session_meta 命中,Claude 行不命中。
    #[test]
    fn probe_discriminates() {
        let dir = tempfile::tempdir().unwrap();
        let codex = dir.path().join("r.jsonl");
        std::fs::write(&codex, r#"{"type":"session_meta","payload":{"id":"x"}}"#).unwrap();
        let claude = dir.path().join("c.jsonl");
        std::fs::write(&claude, r#"{"type":"user","sessionId":"s","uuid":"u"}"#).unwrap();
        assert!(CodexAdapter.probe(&codex));
        assert!(!CodexAdapter.probe(&claude));
    }
}
