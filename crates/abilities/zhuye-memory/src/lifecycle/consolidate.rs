//! v1.11 基建#8:记忆整理(去重 / 冲突消解)。
//!
//! deepagents 式记忆的第四块拼图:MEMORY.md 随时间增长会出现
//! 同名条目(大小写 / 首尾空白差异)、空条目与重复行 —— 乐观锁只保证
//! 并发写不丢,不保证内容质量。本模块做**确定性**整理(零 LLM 成本):
//!
//! 1. 标题规范化(trim + ASCII 小写)后同名合并 —— 后条正文里**未出现
//!    过的行**追加到前条(保留双方信息,不武断丢弃,冲突交由 LLM / 用户
//!    在合并结果上后续修订);标题保留**首次出现**的原样大小写;
//! 2. 空正文条目剔除;
//! 3. 重建文档,条目顺序保持首次出现序。
//!
//! 触发入口:
//! - 手动:`memory` 工具 `consolidate` action;
//! - 后台:submission_loop 每 N turns 自动 spawn(见
//!   `AgentConfig::memory_consolidate_every`,0 = 关闭)。

use std::sync::Arc;

use crate::model::{MemoryError, MemoryScope};
use crate::retrieval::sections::split_sections;
use crate::store::FileMemoryStore;

/// v1.11 基建#8:后台整理句柄 —— bootstrap 持有具体 `FileMemoryStore`
/// 与 agent 名,submission_loop 每 N turns 拿它 spawn 一次整理(不阻塞
/// turn)。`Arc<dyn MemoryStore>` 做不了类型下探,故单独收口。
#[derive(Clone)]
pub struct Consolidator {
    file: Arc<FileMemoryStore>,
    agent_type: String,
    /// 参与整理的 scope 序(默认 Project + User)。
    pub scopes: Vec<MemoryScope>,
}

impl std::fmt::Debug for Consolidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Consolidator")
            .field("agent_type", &self.agent_type)
            .field("scopes", &self.scopes)
            .finish()
    }
}

impl Consolidator {
    /// 以 Project + User 双 scope 构造(默认覆盖面)。
    pub fn new(file: Arc<FileMemoryStore>, agent_type: impl Into<String>) -> Self {
        Self {
            file,
            agent_type: agent_type.into(),
            scopes: vec![MemoryScope::Project, MemoryScope::User],
        }
    }

    /// 跑一轮整理,返回每个 scope 的统计(文件缺失的 scope 无条目)。
    /// 整理失败逐 scope 记日志不中断(后台任务,尽力而为)。
    pub fn run(&self) -> Vec<(MemoryScope, ConsolidateStats)> {
        let mut out = Vec::new();
        for scope in &self.scopes {
            match self.file.consolidate(*scope, &self.agent_type) {
                Ok(Some(stats)) => {
                    if stats.merged > 0 || stats.dropped_empty > 0 {
                        tracing::info!(
                            scope = %scope,
                            merged = stats.merged,
                            dropped_empty = stats.dropped_empty,
                            sections_after = stats.sections_after,
                            "memory consolidation applied"
                        );
                    }
                    out.push((*scope, stats));
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(scope = %scope, error = %e, "memory consolidation failed; skipped");
                }
            }
        }
        out
    }
}

/// 一次整理的统计(工具输出 / 日志用)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConsolidateStats {
    /// 合并的同名条目对数(3 个同名计 2 对)。
    pub merged: usize,
    /// 剔除的空条目数。
    pub dropped_empty: usize,
    /// 整理前条目数。
    pub sections_before: usize,
    /// 整理后条目数。
    pub sections_after: usize,
}

/// 对 MEMORY.md 文本做确定性整理,返回新文本与统计。
/// 无变化时 `new == old`(零拷贝语义:调用方可据 `stats` 判断是否写盘)。
pub fn consolidate_document(content: &str) -> (String, ConsolidateStats) {
    let sections = split_sections(crate::model::MemoryScope::Project, content);
    let mut stats = ConsolidateStats {
        sections_before: sections.len(),
        ..Default::default()
    };
    // 规范化标题 → 聚合正文行(保持首次出现序)。
    let mut order: Vec<String> = Vec::new();
    let mut display_titles: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut bodies: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut seen_lines: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    for sec in &sections {
        let title = sec.title.trim().to_string();
        // 合并 key:ASCII 小写归一("Deploy" 与 "deploy" 视为同一主题);
        // 展示标题保留首次出现的原样。
        let norm = title.to_ascii_lowercase();
        let body = sec.body.trim().to_string();
        if body.is_empty() {
            stats.dropped_empty += 1;
            continue;
        }
        if !bodies.contains_key(&norm) {
            order.push(norm.clone());
            bodies.insert(norm.clone(), Vec::new());
            seen_lines.insert(norm.clone(), std::collections::HashSet::new());
            // 记录首次出现的展示标题(重建时用)。
            display_titles.insert(norm.clone(), title);
        } else {
            stats.merged += 1;
        }
        let lines = bodies.get_mut(&norm).expect("norm just inserted");
        let seen = seen_lines.get_mut(&norm).expect("norm just inserted");
        for line in body.lines() {
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            if seen.insert(line.to_string()) {
                lines.push(line.to_string());
            }
        }
    }
    stats.sections_after = order.len();
    // 重建文档:无标题条目(引言)保持文件头部;标题用首次出现的原样。
    let mut out = String::with_capacity(content.len());
    for norm in &order {
        if let Some(display) = display_titles.get(norm).filter(|t| !t.is_empty()) {
            out.push_str(&format!("## {display}\n"));
        }
        for line in bodies.get(norm).into_iter().flatten() {
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
    }
    // 收尾:去多余空行,统一以单个换行结束(空文档保持空)。
    let trimmed = out.trim();
    if trimmed.is_empty() {
        (String::new(), stats)
    } else {
        (format!("{trimmed}\n"), stats)
    }
}

impl FileMemoryStore {
    /// 整理某 `(scope, agent_type)` 的 MEMORY.md:有变化则原子写回,
    /// 返回统计;文件不存在 → 无操作(`None`)。Session scope 拒绝。
    pub fn consolidate(
        &self,
        scope: MemoryScope,
        agent_type: &str,
    ) -> Result<Option<ConsolidateStats>, MemoryError> {
        if matches!(scope, MemoryScope::Session) {
            return Err(MemoryError::SessionNotPersisted);
        }
        let path = self.path(scope, agent_type)?;
        let current = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let (next, stats) = consolidate_document(&current);
        if stats.merged == 0 && stats.dropped_empty == 0 && next == current {
            return Ok(Some(stats));
        }
        zhuye_fs::write_atomic(&path, next.as_bytes())?;
        Ok(Some(stats))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;

    #[test]
    fn merge_same_title_case_insensitive_diff() {
        // 大小写差异视为同名(trim 规范化;大小写不敏感合并)。
        let content = "## Deploy\nuses github actions\n\n## deploy\nruns on main branch\n";
        let (next, stats) = consolidate_document(content);
        assert_eq!(stats.merged, 1);
        assert_eq!(stats.sections_before, 2);
        assert_eq!(stats.sections_after, 1);
        assert!(next.contains("uses github actions"));
        assert!(next.contains("runs on main branch"));
        // 两个标题块合并为一个。
        assert_eq!(next.matches("## ").count(), 1);
    }

    #[test]
    fn duplicate_lines_dedup_on_merge() {
        let content = "## Env\nRUST_BACKTRACE=1\n\n## Env\nRUST_BACKTRACE=1\nRUST_LOG=debug\n";
        let (next, stats) = consolidate_document(content);
        assert_eq!(stats.merged, 1);
        assert_eq!(next.matches("RUST_BACKTRACE=1").count(), 1);
        assert!(next.contains("RUST_LOG=debug"));
    }

    #[test]
    fn empty_sections_dropped() {
        let content = "## Keep\nreal content\n\n## Empty\n\n   \n";
        let (next, stats) = consolidate_document(content);
        assert_eq!(stats.dropped_empty, 1);
        assert!(next.contains("## Keep"));
        assert!(!next.contains("## Empty"));
    }

    #[test]
    fn clean_document_is_noop() {
        let content = "## A\nalpha\n\n## B\nbeta\n";
        let (next, stats) = consolidate_document(content);
        assert_eq!(stats.merged, 0);
        assert_eq!(stats.dropped_empty, 0);
        // 条目顺序与正文保持,重建文本语义等价(空白归一)。
        assert!(next.contains("## A\nalpha"));
        assert!(next.contains("## B\nbeta"));
        assert_eq!(next.matches("## ").count(), 2);
    }

    #[test]
    fn intro_without_title_preserved() {
        let content = "intro line\n## A\nalpha\n";
        let (next, stats) = consolidate_document(content);
        assert_eq!(stats.sections_before, 2);
        assert!(next.starts_with("intro line"));
        assert!(next.contains("## A"));
    }

    #[test]
    fn store_consolidate_writes_back_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        let s = FileMemoryStore::new(dir.path(), dir.path());
        s.save(
            crate::model::MemoryScope::Project,
            "agent",
            "## X\nsame\n\n## x\nsame\nmore\n",
        )
        .unwrap();
        let stats = s
            .consolidate(crate::model::MemoryScope::Project, "agent")
            .unwrap()
            .expect("文件存在应有统计");
        assert_eq!(stats.merged, 1);
        let after = s.load(crate::model::MemoryScope::Project, "agent").unwrap();
        assert_eq!(after.matches("## ").count(), 1);
        assert!(after.contains("more"));
        // 再跑一次:无变化(幂等)。
        let again = s
            .consolidate(crate::model::MemoryScope::Project, "agent")
            .unwrap()
            .unwrap();
        assert_eq!(again.merged, 0);
    }

    #[test]
    fn store_consolidate_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let s = FileMemoryStore::new(dir.path(), dir.path());
        assert!(
            s.consolidate(crate::model::MemoryScope::Project, "agent")
                .unwrap()
                .is_none()
        );
    }
}
