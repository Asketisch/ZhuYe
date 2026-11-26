//! v1.11 基建#8:多文件记忆目录(wiki 知识库)。
//!
//! deepagents 式记忆的第二块拼图:MEMORY.md 之外,`agent-memory/
//! {agent_type}/topics/<name>.md` 承载长主题知识(架构决策 / 领域名词 /
//! 操作手册……)。MEMORY.md 保持精炼索引,超长内容放 topic 文件,
//! 检索(`retrieve_relevant`)把 topic 文件一并纳入 BM25 候选 ——
//! 注入预算内按相关度装箱,单文件不再挤占其他条目。
//!
//! 主题名约束:仅 `[A-Za-z0-9_-]`,1~64 字符 —— 杜绝路径穿越
//! (`../`)与大小写歧义文件名,同时保证跨平台可移植。
//!
//! 写入走 `zhuye_fs::write_atomic`(与 MEMORY.md 条目编辑同源)。

use std::path::PathBuf;

use crate::model::{MemoryError, MemoryScope};
use crate::store::FileMemoryStore;

/// topic 文件名长度上限(不含 `.md` 后缀)。
const TOPIC_NAME_MAX: usize = 64;

/// 校验主题名:非空、仅 `[A-Za-z0-9_-]`、长度 ≤ [`TOPIC_NAME_MAX`]。
/// 失败统一 [`MemoryError::InvalidKey`](语义复用:key 非法族)。
pub fn validate_topic_name(name: &str) -> Result<(), MemoryError> {
    let ok = !name.is_empty()
        && name.chars().count() <= TOPIC_NAME_MAX
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(MemoryError::InvalidKey(name.to_string()))
    }
}

impl FileMemoryStore {
    /// `(scope, agent_type)` 的 topics 目录。
    fn topics_dir(&self, scope: MemoryScope, agent_type: &str) -> Result<PathBuf, MemoryError> {
        // 复用 MEMORY.md 的 resolve 口径(防 Session scope 落盘等不变量),
        // 取其父目录拼 topics 子目录。
        let memory_md = self.path(scope, agent_type)?;
        Ok(memory_md
            .parent()
            .map_or_else(|| PathBuf::from("topics"), |p| p.join("topics")))
    }

    /// 列出某 `(scope, agent_type)` 的全部主题文件名(按名字排序;
    /// 不含 `.md` 后缀)。目录不存在 → 空表。
    pub fn list_topics(
        &self,
        scope: MemoryScope,
        agent_type: &str,
    ) -> Result<Vec<String>, MemoryError> {
        if matches!(scope, MemoryScope::Session) {
            return Ok(Vec::new());
        }
        let dir = self.topics_dir(scope, agent_type)?;
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.push(stem.to_string());
            }
        }
        out.sort();
        Ok(out)
    }

    /// 读取一个主题文件的正文。不存在 → 空串(与 MEMORY.md 读语义一致)。
    pub fn load_topic(
        &self,
        scope: MemoryScope,
        agent_type: &str,
        topic: &str,
    ) -> Result<String, MemoryError> {
        validate_topic_name(topic)?;
        if matches!(scope, MemoryScope::Session) {
            return Ok(String::new());
        }
        let path = self
            .topics_dir(scope, agent_type)?
            .join(format!("{topic}.md"));
        if !path.exists() {
            return Ok(String::new());
        }
        Ok(std::fs::read_to_string(&path)?)
    }

    /// 写一个主题文件(整文件替换,原子写)。自动建 topics 目录。
    /// 空正文 = 删除该文件(幂等)。
    pub fn save_topic(
        &self,
        scope: MemoryScope,
        agent_type: &str,
        topic: &str,
        content: &str,
    ) -> Result<(), MemoryError> {
        validate_topic_name(topic)?;
        if matches!(scope, MemoryScope::Session) {
            return Err(MemoryError::SessionNotPersisted);
        }
        let path = self
            .topics_dir(scope, agent_type)?
            .join(format!("{topic}.md"));
        if content.trim().is_empty() {
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        zhuye_fs::write_atomic(&path, content.as_bytes())?;
        Ok(())
    }

    /// 删除一个主题文件(不存在静默成功,幂等)。
    pub fn remove_topic(
        &self,
        scope: MemoryScope,
        agent_type: &str,
        topic: &str,
    ) -> Result<(), MemoryError> {
        validate_topic_name(topic)?;
        if matches!(scope, MemoryScope::Session) {
            return Ok(());
        }
        let path = self
            .topics_dir(scope, agent_type)?
            .join(format!("{topic}.md"));
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, FileMemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = FileMemoryStore::new(dir.path(), dir.path());
        (dir, s)
    }

    #[test]
    fn topic_name_validation() {
        assert!(validate_topic_name("arch-decisions").is_ok());
        assert!(validate_topic_name("API_Names_2").is_ok());
        assert!(validate_topic_name("").is_err());
        assert!(validate_topic_name("../evil").is_err());
        assert!(validate_topic_name("has space").is_err());
        assert!(validate_topic_name("a.b").is_err());
        let long = "a".repeat(TOPIC_NAME_MAX + 1);
        assert!(validate_topic_name(&long).is_err());
        // 上限边界:恰好 64 字符合法。
        let ok = "a".repeat(TOPIC_NAME_MAX);
        assert!(validate_topic_name(&ok).is_ok());
    }

    #[test]
    fn topic_write_read_list_remove_roundtrip() {
        let (_d, s) = store();
        s.save_topic(
            MemoryScope::Project,
            "agent",
            "arch",
            "# 架构\npostgres 16\n",
        )
        .unwrap();
        s.save_topic(
            MemoryScope::Project,
            "agent",
            "glossary",
            "# 术语\n烛夜 = ZhuYe\n",
        )
        .unwrap();
        assert_eq!(
            s.load_topic(MemoryScope::Project, "agent", "arch").unwrap(),
            "# 架构\npostgres 16\n"
        );
        let topics = s.list_topics(MemoryScope::Project, "agent").unwrap();
        assert_eq!(topics, vec!["arch", "glossary"]);
        // 不存在的 topic 读到空串。
        assert_eq!(
            s.load_topic(MemoryScope::Project, "agent", "nope").unwrap(),
            ""
        );
        // 删除幂等。
        s.remove_topic(MemoryScope::Project, "agent", "arch")
            .unwrap();
        assert_eq!(
            s.list_topics(MemoryScope::Project, "agent").unwrap(),
            vec!["glossary"]
        );
        s.remove_topic(MemoryScope::Project, "agent", "arch")
            .unwrap();
    }

    /// 空正文写入 = 删除文件(幂等清空语义)。
    #[test]
    fn empty_save_removes_topic() {
        let (_d, s) = store();
        s.save_topic(MemoryScope::Project, "agent", "t1", "content")
            .unwrap();
        s.save_topic(MemoryScope::Project, "agent", "t1", "")
            .unwrap();
        assert!(
            s.list_topics(MemoryScope::Project, "agent")
                .unwrap()
                .is_empty()
        );
        // 再写空也不报错(文件已不存在)。
        s.save_topic(MemoryScope::Project, "agent", "t1", "  \n")
            .unwrap();
    }

    /// 非法主题名拒绝写入(路径穿越防护)。
    #[test]
    fn save_topic_rejects_traversal() {
        let (_d, s) = store();
        let err = s.save_topic(MemoryScope::Project, "agent", "../evil", "x");
        assert!(err.is_err());
        // 确认没有文件写到 topics 之外。
        assert!(
            s.list_topics(MemoryScope::Project, "agent")
                .unwrap()
                .is_empty()
        );
    }

    /// Session scope:读空、列空、写拒绝(与 MEMORY.md 同不变量)。
    #[test]
    fn session_scope_topics_not_persisted() {
        let (_d, s) = store();
        assert_eq!(
            s.load_topic(MemoryScope::Session, "agent", "t").unwrap(),
            ""
        );
        assert!(
            s.list_topics(MemoryScope::Session, "agent")
                .unwrap()
                .is_empty()
        );
        assert!(
            s.save_topic(MemoryScope::Session, "agent", "t", "x")
                .is_err()
        );
    }
}
