//! v1.11 基建#8:跨会话笔记召回(deepagents 式)。
//!
//! `FileBackedNoteStore` 只按同一 thread_id 恢复本次会话的 FIFO 30 条;
//! 旧会话的发现(「某文件在哪」「某坑怎么绕」)从此永远沉底。本模块在
//! **读取侧**打通:扫描 `session-notes/*.jsonl` 里**其他会话**的笔记,
//! 以当前查询做 BM25 相关度排序,取 top-k 注入 —— 零 LLM 成本,
//! 纯读侧改动,写路径(`add_session_note`)完全不变。
//!
//! 健壮性:坏行 / 坏文件逐条跳过(不因单个损坏文件丢掉全部召回);
//! 文件数与命中数双上限(防大 `session-notes/` 目录拖慢 pre_loop)。

use std::path::Path;

use serde::Deserialize;

/// 一条跨会话召回的笔记。
#[derive(Debug, Clone, PartialEq)]
pub struct RecalledNote {
    /// 来源会话(thread id,即 JSONL 文件名 stem)。
    pub thread_id: String,
    /// 笔记正文。
    pub text: String,
    /// 笔记创建时间(RFC3339 解析失败时为 `None`)。
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// 单行 JSONL 的解析形态(与 `FileBackedNoteStore` 落盘格式同源)。
#[derive(Deserialize)]
struct NoteLine {
    text: String,
    #[serde(default)]
    created_at: Option<String>,
}

/// 扫描目录上限(按修改时间取最近 N 个文件,其余忽略)。
const MAX_SCAN_FILES: usize = 64;
/// 召回条数上限(pre_loop 注入预算保护)。
pub const RECALL_LIMIT: usize = 5;

/// 跨会话笔记召回:在 `notes_home/session-notes/*.jsonl` 中找**其他
/// 会话**(排除 `exclude_thread`)的笔记,按 `query` BM25 相关度取
/// top-`k`。`query` 为空时按「文件越新优先、文件内越靠后(越新)优先」
/// 取最近笔记。
pub fn recall_cross_session(
    notes_home: &Path,
    exclude_thread: &str,
    query: &str,
    k: usize,
) -> Vec<RecalledNote> {
    let dir = notes_home.join("session-notes");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    // 收集候选文件(排除当前会话;按修改时间新→旧,截 MAX_SCAN_FILES)。
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf, String)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                return None;
            }
            let stem = path.file_stem()?.to_str()?.to_string();
            if stem == exclude_thread {
                return None;
            }
            let mtime = e.metadata().and_then(|m| m.modified()).ok()?;
            Some((mtime, path, stem))
        })
        .collect();
    files.sort_by_key(|(mtime, _, _)| std::cmp::Reverse(*mtime));
    files.truncate(MAX_SCAN_FILES);

    // 逐文件解析(单文件上限 200 条,防超长 JSONL)。
    let mut candidates: Vec<(String, NoteLine)> = Vec::new();
    for (_mtime, path, stem) in &files {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        let mut count = 0usize;
        for line in content.lines().rev() {
            if count >= 200 {
                break;
            }
            if let Ok(note) = serde_json::from_str::<NoteLine>(line)
                && !note.text.trim().is_empty()
            {
                candidates.push((stem.clone(), note));
                count += 1;
            }
        }
    }
    if candidates.is_empty() {
        return Vec::new();
    }

    // 打分排序:有查询走 BM25;无查询按收集序(文件新→旧 + 行新→旧)。
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    if !query.trim().is_empty() {
        let terms = zhuye_bm25::tokenize(query);
        let docs: Vec<(usize, String)> = candidates
            .iter()
            .enumerate()
            .map(|(i, (_, n))| (i, n.text.clone()))
            .collect();
        let hits = zhuye_bm25::rank_lines(&terms, &docs, 1.2, 0.75);
        let score_of = |idx: usize| -> f64 {
            hits.iter()
                .find(|h| h.line_no == idx)
                .map_or(0.0, |h| h.score)
        };
        order.sort_by(|&a, &b| {
            score_of(b)
                .partial_cmp(&score_of(a))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.cmp(&b))
        });
        // 零相关条目不占召回名额(与记忆检索的「相关优先」语义一致,
        // 但保留 top-k 若全零则返回空 —— 宁缺毋滥,不注入噪声)。
        let mut scored: Vec<usize> = order.into_iter().filter(|&i| score_of(i) > 0.0).collect();
        scored.truncate(k);
        order = scored;
    } else {
        order.truncate(k);
    }

    order
        .into_iter()
        .map(|i| {
            let (thread_id, n) = &candidates[i];
            RecalledNote {
                thread_id: thread_id.clone(),
                text: n.text.trim().to_string(),
                created_at: n
                    .created_at
                    .as_deref()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&chrono::Utc)),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_notes(home: &Path, thread: &str, lines: &[&str]) {
        let dir = home.join("session-notes");
        std::fs::create_dir_all(&dir).unwrap();
        let body = lines
            .iter()
            .map(|t| format!("{{\"text\": \"{t}\", \"created_at\": \"2026-01-01T00:00:00Z\"}}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join(format!("{thread}.jsonl")), body).unwrap();
    }

    #[test]
    fn bm25_recall_picks_relevant_note() {
        let home = tempfile::tempdir().unwrap();
        // 当前会话是 thread-a(排除);thread-b 留有 postgres 发现。
        write_notes(home.path(), "thread-a", &["部署走 github actions"]);
        write_notes(home.path(), "thread-b", &["数据库是 postgres 16"]);
        let hits = recall_cross_session(home.path(), "thread-a", "postgres database", 5);
        assert_eq!(hits.len(), 1, "只应召回 thread-b 的相关笔记:{hits:?}");
        assert_eq!(hits[0].thread_id, "thread-b");
        assert!(hits[0].text.contains("postgres 16"));
    }

    #[test]
    fn excludes_current_thread() {
        let home = tempfile::tempdir().unwrap();
        write_notes(home.path(), "current", &["本会话的 postgres 笔记"]);
        let hits = recall_cross_session(home.path(), "current", "postgres", 5);
        assert!(hits.is_empty(), "当前会话笔记必须被排除:{hits:?}");
    }

    #[test]
    fn zero_relevance_returns_empty() {
        let home = tempfile::tempdir().unwrap();
        write_notes(home.path(), "t1", &["完全无关的笔记内容"]);
        let hits = recall_cross_session(home.path(), "t2", "postgres database tuning", 5);
        assert!(hits.is_empty(), "零相关不得注入噪声:{hits:?}");
    }

    #[test]
    fn empty_query_takes_recent_notes() {
        let home = tempfile::tempdir().unwrap();
        write_notes(home.path(), "t1", &["笔记一", "笔记二"]);
        let hits = recall_cross_session(home.path(), "other", "", 5);
        assert_eq!(hits.len(), 2);
        // 收集序:文件内从新到旧。
        assert_eq!(hits[0].text, "笔记二");
    }

    #[test]
    fn corrupt_lines_and_files_are_tolerated() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("session-notes");
        std::fs::create_dir_all(&dir).unwrap();
        // 坏文件(非 UTF-8 模拟:直接写坏 JSON 行)。
        std::fs::write(dir.join("bad.jsonl"), "not json\n{\n").unwrap();
        // 好文件混坏行。
        std::fs::write(
            dir.join("good.jsonl"),
            "{\"text\": \"postgres 在 16 版本\"}\ngarbage line\n",
        )
        .unwrap();
        let hits = recall_cross_session(home.path(), "x", "postgres", 5);
        assert_eq!(hits.len(), 1, "坏行跳过、好行保留:{hits:?}");
        assert!(hits[0].text.contains("postgres"));
    }

    #[test]
    fn k_limit_respected() {
        let home = tempfile::tempdir().unwrap();
        let lines: Vec<String> = (0..10)
            .map(|i| format!("{{\"text\": \"postgres 笔记 {i}\"}}"))
            .collect();
        let dir = home.path().join("session-notes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("t.jsonl"), lines.join("\n")).unwrap();
        let hits = recall_cross_session(home.path(), "x", "postgres", 3);
        assert_eq!(hits.len(), 3);
    }
}
