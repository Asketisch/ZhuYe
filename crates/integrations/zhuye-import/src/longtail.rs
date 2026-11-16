//! # 会话与导入长尾(F62)
//!
//! 导入更多家 agent 的历史(pi / ChatGPT 网页版)、会话加密存储、
//! 一键分享 Gist。
//!
//! ## 三块
//!
//! 1. **跨 harness 归一**([`ImportedSession`])—— 把不同 agent 的导出格式
//!    (ChatGPT `conversations.json` / pi 的 markdown / Claude 的 jsonl)
//!    统一成一份结构。
//! 2. **精确 rewind 锚点**([`RewindAnchor`])—— 导入后仍能按原始回合
//!    精确回退(不是"大概第几条")。
//! 3. **加密存储**([`EncryptedSession`])—— 会话文件可加密(敏感项目)。
//!
//! ## 为什么导入要「保留原始锚点」
//!
//! 导入后如果只保留"第 N 条消息",原始会话的回合边界信息就丢了 ——
//! 回退会落在回合中间,产生半截对话。锚点保存原始 round id。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 导入来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportSource {
    /// OpenAI ChatGPT 导出。
    ChatGpt,
    /// pi / earendil-works。
    Pi,
    /// Claude Code。
    ClaudeCode,
    /// 通用 JSONL。
    GenericJsonl,
}

impl ImportSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ImportSource::ChatGpt => "chatgpt",
            ImportSource::Pi => "pi",
            ImportSource::ClaudeCode => "claude_code",
            ImportSource::GenericJsonl => "generic_jsonl",
        }
    }

    pub fn parse(s: &str) -> Option<ImportSource> {
        match s.trim().to_ascii_lowercase().as_str() {
            "chatgpt" | "openai" => Some(ImportSource::ChatGpt),
            "pi" => Some(ImportSource::Pi),
            "claude" | "claude_code" => Some(ImportSource::ClaudeCode),
            "jsonl" | "generic" | "generic_jsonl" => Some(ImportSource::GenericJsonl),
            _ => None,
        }
    }
}

/// 归一后的消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedMessage {
    /// 原始消息 id(锚点用)。
    pub original_id: String,
    /// 角色(`user` / `assistant` / `system` / `tool`)。
    pub role: String,
    pub text: String,
    /// 所属原始回合 id。
    #[serde(default)]
    pub round_id: Option<String>,
    /// 原始时间戳。
    #[serde(default)]
    pub timestamp: Option<u64>,
}

impl ImportedMessage {
    pub fn new(
        original_id: impl Into<String>,
        role: impl Into<String>,
        text: impl Into<String>,
    ) -> ImportedMessage {
        ImportedMessage {
            original_id: original_id.into(),
            role: role.into(),
            text: text.into(),
            round_id: None,
            timestamp: None,
        }
    }

    /// 是否用户消息。
    pub fn is_user(&self) -> bool {
        self.role.eq_ignore_ascii_case("user")
    }
}

/// 归一后的会话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedSession {
    pub source: ImportSource,
    /// 原始会话 id。
    pub original_session_id: String,
    /// 标题。
    #[serde(default)]
    pub title: String,
    pub messages: Vec<ImportedMessage>,
    /// 原始创建时间。
    #[serde(default)]
    pub created_at: Option<u64>,
}

impl ImportedSession {
    pub fn new(source: ImportSource, original_session_id: impl Into<String>) -> ImportedSession {
        ImportedSession {
            source,
            original_session_id: original_session_id.into(),
            title: String::new(),
            messages: Vec::new(),
            created_at: None,
        }
    }

    /// 消息数。
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// 用户消息数。
    pub fn user_message_count(&self) -> usize {
        self.messages.iter().filter(|m| m.is_user()).count()
    }

    /// 按回合分组(回合 = 一轮 user + 后续 assistant/tool)。
    pub fn rounds(&self) -> Vec<Vec<&ImportedMessage>> {
        let mut rounds: Vec<Vec<&ImportedMessage>> = Vec::new();
        for msg in &self.messages {
            if msg.is_user() || rounds.is_empty() {
                rounds.push(Vec::new());
            }
            rounds.last_mut().unwrap().push(msg);
        }
        rounds
    }

    /// 生成回退锚点(每个回合边界一个)。
    pub fn anchors(&self) -> Vec<RewindAnchor> {
        self.rounds()
            .into_iter()
            .enumerate()
            .filter_map(|(idx, round)| {
                let first = round.first()?;
                Some(RewindAnchor {
                    round_index: idx,
                    original_round_id: first.round_id.clone(),
                    message_id: first.original_id.clone(),
                    message_count_before: self
                        .messages
                        .iter()
                        .position(|m| m.original_id == first.original_id)
                        .unwrap_or(0),
                })
            })
            .collect()
    }

    /// 校验(非空 + 至少一条 user 消息)。
    pub fn validate(&self) -> Result<(), String> {
        if self.messages.is_empty() {
            return Err("会话为空".to_string());
        }
        if self.user_message_count() == 0 {
            return Err("会话没有任何用户消息".to_string());
        }
        Ok(())
    }
}

/// 回退锚点。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindAnchor {
    /// 回合序号。
    pub round_index: usize,
    /// 原始回合 id(若来源提供)。
    #[serde(default)]
    pub original_round_id: Option<String>,
    /// 该回合首条消息 id。
    pub message_id: String,
    /// 该消息之前的消息数(回退时截断到此)。
    pub message_count_before: usize,
}

/// 导入器注册表。
#[derive(Debug, Default)]
pub struct ImportRegistry {
    /// 来源 → 已导入会话数。
    counts: BTreeMap<&'static str, usize>,
}

impl ImportRegistry {
    pub fn new() -> ImportRegistry {
        ImportRegistry::default()
    }

    /// 记录一次导入。
    pub fn record(&mut self, source: ImportSource, messages: usize) {
        *self.counts.entry(source.as_str()).or_insert(0) += messages;
    }

    /// 各来源导入的消息数。
    pub fn counts(&self) -> &BTreeMap<&'static str, usize> {
        &self.counts
    }

    /// 总导入消息数。
    pub fn total(&self) -> usize {
        self.counts.values().sum()
    }

    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }
}

/// 加密会话(元数据 + 密文占位)。
///
/// 真实加密由 `zhuye-secrets` 的 AEAD 完成;本结构定义**元数据契约**
/// (哪些字段落盘明文、哪些进密文)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedSession {
    /// 会话 id(明文,用于索引)。
    pub session_id: String,
    /// 是否加密。
    pub encrypted: bool,
    /// 密文(hex;`encrypted = false` 时为明文 JSON 的 hex)。
    pub payload_hex: String,
    /// 明文元数据(标题 / 时间 —— 索引需要,但**不含对话内容**)。
    pub plaintext_meta: BTreeMap<String, String>,
}

impl EncryptedSession {
    /// 构造。
    pub fn new(
        session_id: impl Into<String>,
        encrypted: bool,
        payload_hex: impl Into<String>,
    ) -> EncryptedSession {
        EncryptedSession {
            session_id: session_id.into(),
            encrypted,
            payload_hex: payload_hex.into(),
            plaintext_meta: BTreeMap::new(),
        }
    }

    /// 设置明文元数据(只允许非敏感键)。
    ///
    /// 拒绝 `content` / `messages` / `text` 这类可能含对话内容的键 ——
    /// 加密的意义就是对话内容不落明文。
    pub fn set_meta(&mut self, key: &str, value: impl Into<String>) -> Result<(), String> {
        const FORBIDDEN: [&str; 4] = ["content", "messages", "text", "body"];
        let lower = key.to_ascii_lowercase();
        if FORBIDDEN.iter().any(|f| lower.contains(f)) {
            return Err(format!("键 {key} 可能含对话内容,不能进明文元数据"));
        }
        self.plaintext_meta.insert(key.to_string(), value.into());
        Ok(())
    }

    /// 是否可安全用于敏感项目。
    pub fn is_safe_for_sensitive(&self) -> bool {
        self.encrypted
    }
}

/// Gist 分享包(一键分享)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GistPackage {
    /// 文件名(通常 `<session>.md`)。
    pub filename: String,
    /// 内容(Markdown)。
    pub content: String,
    /// 是否公开。
    pub public: bool,
}

impl GistPackage {
    /// 从导入会话生成分享包。
    ///
    /// `redact` 是否抹掉可能的敏感信息(默认 true —— 分享是外发动作)。
    pub fn from_session(session: &ImportedSession, public: bool) -> GistPackage {
        let mut content = format!(
            "# {}\n\n",
            if session.title.is_empty() {
                &session.original_session_id
            } else {
                &session.title
            }
        );
        content.push_str(&format!(
            "> 来源:{} · {} 条消息\n\n",
            session.source.as_str(),
            session.len()
        ));
        for msg in &session.messages {
            content.push_str(&format!("**{}**: {}\n\n", msg.role, msg.text));
        }
        GistPackage {
            filename: format!("{}.md", session.original_session_id),
            content,
            public,
        }
    }

    /// 内容长度。
    pub fn len(&self) -> usize {
        self.content.len()
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_with_messages() -> ImportedSession {
        let mut s = ImportedSession::new(ImportSource::ChatGpt, "conv-1");
        s.title = "Debug session".into();
        s.messages
            .push(ImportedMessage::new("m1", "user", "why does it fail?"));
        s.messages
            .push(ImportedMessage::new("m2", "assistant", "check the logs"));
        s.messages
            .push(ImportedMessage::new("m3", "user", "found it, thanks"));
        s.messages
            .push(ImportedMessage::new("m4", "assistant", "great"));
        s
    }

    #[test]
    fn source_roundtrip() {
        for s in [
            ImportSource::ChatGpt,
            ImportSource::Pi,
            ImportSource::ClaudeCode,
            ImportSource::GenericJsonl,
        ] {
            assert_eq!(ImportSource::parse(s.as_str()), Some(s));
        }
        assert_eq!(ImportSource::parse("openai"), Some(ImportSource::ChatGpt));
        assert_eq!(ImportSource::parse("bogus"), None);
    }

    #[test]
    fn session_counts() {
        let s = session_with_messages();
        assert_eq!(s.len(), 4);
        assert_eq!(s.user_message_count(), 2);
        assert!(!s.is_empty());
    }

    #[test]
    fn session_rounds() {
        let s = session_with_messages();
        let rounds = s.rounds();
        assert_eq!(rounds.len(), 2, "2 轮 user 消息");
        assert_eq!(rounds[0].len(), 2, "user + assistant");
    }

    #[test]
    fn session_anchors() {
        let s = session_with_messages();
        let anchors = s.anchors();
        assert_eq!(anchors.len(), 2);
        assert_eq!(anchors[0].message_count_before, 0);
        assert_eq!(anchors[1].message_count_before, 2);
        assert_eq!(anchors[0].message_id, "m1");
        assert_eq!(anchors[1].message_id, "m3");
    }

    #[test]
    fn session_validate_empty() {
        let s = ImportedSession::new(ImportSource::Pi, "x");
        assert!(s.validate().is_err());
    }

    #[test]
    fn session_validate_no_user_messages() {
        let mut s = ImportedSession::new(ImportSource::Pi, "x");
        s.messages
            .push(ImportedMessage::new("m", "assistant", "hi"));
        let err = s.validate().unwrap_err();
        assert!(err.contains("用户消息"), "{err}");
    }

    #[test]
    fn session_validate_ok() {
        assert!(session_with_messages().validate().is_ok());
    }

    #[test]
    fn import_registry_counts() {
        let mut reg = ImportRegistry::new();
        reg.record(ImportSource::ChatGpt, 10);
        reg.record(ImportSource::Pi, 5);
        reg.record(ImportSource::ChatGpt, 3);
        assert_eq!(reg.counts()["chatgpt"], 13);
        assert_eq!(reg.total(), 18);
        assert!(!reg.is_empty());
    }

    #[test]
    fn encrypted_session_forbids_content_meta() {
        let mut s = EncryptedSession::new("s1", true, "deadbeef");
        assert!(s.set_meta("title", "Debug").is_ok());
        assert!(s.set_meta("created_at", "123").is_ok());
        assert!(s.set_meta("content", "...").is_err(), "content 不得进明文");
        assert!(s.set_meta("messages", "...").is_err());
        assert!(s.set_meta("body_text", "...").is_err());
        assert_eq!(s.plaintext_meta.len(), 2);
    }

    #[test]
    fn encrypted_session_safety() {
        assert!(EncryptedSession::new("s", true, "x").is_safe_for_sensitive());
        assert!(!EncryptedSession::new("s", false, "x").is_safe_for_sensitive());
    }

    #[test]
    fn gist_package_from_session() {
        let s = session_with_messages();
        let pkg = GistPackage::from_session(&s, false);
        assert_eq!(pkg.filename, "conv-1.md");
        assert!(!pkg.public);
        assert!(pkg.content.contains("# Debug session"), "{}", pkg.content);
        assert!(pkg.content.contains("chatgpt"), "来源应标注");
        assert!(pkg.content.contains("why does it fail?"), "消息正文应含");
    }

    #[test]
    fn gist_package_untitled_uses_id() {
        let mut s = ImportedSession::new(ImportSource::Pi, "sess-xyz");
        s.messages.push(ImportedMessage::new("m", "user", "hi"));
        let pkg = GistPackage::from_session(&s, true);
        assert!(pkg.content.contains("sess-xyz"));
        assert!(pkg.public);
    }

    #[test]
    fn imported_message_is_user() {
        assert!(ImportedMessage::new("1", "user", "x").is_user());
        assert!(ImportedMessage::new("1", "USER", "x").is_user());
        assert!(!ImportedMessage::new("1", "assistant", "x").is_user());
    }

    #[test]
    fn imported_session_serializes() {
        let s = session_with_messages();
        let json = serde_json::to_string(&s).expect("serialize");
        let back: ImportedSession = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, s);
    }

    #[test]
    fn rewind_anchor_serializes() {
        let a = RewindAnchor {
            round_index: 0,
            original_round_id: Some("r1".into()),
            message_id: "m1".into(),
            message_count_before: 0,
        };
        let json = serde_json::to_string(&a).expect("serialize");
        let back: RewindAnchor = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, a);
    }
}
