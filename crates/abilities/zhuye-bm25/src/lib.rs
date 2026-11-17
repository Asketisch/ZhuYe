//! BM25 相关性排序 —— 供 `grep` 工具的 `rank=bm25` 模式使用。
//!
//! 依赖 `rankfns` 提供的 Okapi BM25 核函数;本模块负责分词、语料统计与
//! 候选行打分,不涉及倒排索引(符合 v1 P1 的「ripgrep + BM25 排序」范围)。

use rankfns::{bm25_idf_plus1, bm25_tf};

/// 单条 BM25 命中:分数 + 原始行文本 + 1-based 行号。
#[derive(Debug, Clone, PartialEq)]
pub struct Bm25Hit {
    pub score: f64,
    pub line_no: usize,
    pub line: String,
}

/// 将查询与文本规范化为 BM25 词项(小写、按非字母数字切分)。
///
/// v1.11 记忆升级:词法升级为「ASCII 词 + CJK bigram」双路 ——
/// ASCII 字母 / 数字 / 下划线连成小写词项(行为与旧版一致);
/// CJK 表意文字与假名串产出**相邻二元组**(bigram),单字串退化为
/// 单字词项。纯中文查询此前经 ASCII 过滤后**零词项**(BM25 直接
/// 空转),是记忆与 notes 两路召回的共同能力缺口。二元组边界
/// 不跨越词类(「使用PostgreSQL」→ `使用` + `postgresql`,
/// 不产出 `用p`)。全角 ASCII(FF01..FF5E)先折叠为半角再入词。
pub fn tokenize(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut ascii_run = String::new();
    let mut cjk_run: Vec<char> = Vec::new();
    for ch in text.chars() {
        // 全角 ASCII 折叠到半角(全半角混写按同一词项检索)。
        let ch = fold_fullwidth(ch);
        if ch.is_ascii_alphanumeric() || ch == '_' {
            flush_cjk(&mut cjk_run, &mut terms);
            ascii_run.push(ch.to_ascii_lowercase());
        } else if is_cjk(ch) {
            flush_ascii(&mut ascii_run, &mut terms);
            cjk_run.push(ch);
        } else {
            flush_ascii(&mut ascii_run, &mut terms);
            flush_cjk(&mut cjk_run, &mut terms);
        }
    }
    flush_ascii(&mut ascii_run, &mut terms);
    flush_cjk(&mut cjk_run, &mut terms);
    terms
}

/// 全角 ASCII(U+FF01..U+FF5E)折叠为对应半角;其余字符原样返回。
/// 公开给下游(zhuye-memory 的 FTS 预分词列)复用同一套词法。
pub fn fold_fullwidth(ch: char) -> char {
    let c = ch as u32;
    if (0xFF01..=0xFF5E).contains(&c) {
        char::from_u32(c - 0xFEE0).unwrap_or(ch)
    } else {
        ch
    }
}

/// CJK 判定:汉字基本区 / 扩展 A / 兼容表意 / 扩展 B..F,以及平假名 /
/// 片假名。CJK 标点(U+3000..U+303F)与全角标点**不是**词项字符,
/// 作为分隔符处理(「数据库。」→ `数据` `据库`,不吞句号)。
/// 公开给下游(zhuye-memory 的 FTS 单字列)复用同一套词法。
pub fn is_cjk(ch: char) -> bool {
    let c = ch as u32;
    (0x4E00..=0x9FFF).contains(&c)          // CJK 统一表意文字(基本区)
        || (0x3400..=0x4DBF).contains(&c)   // 扩展 A
        || (0xF900..=0xFAFF).contains(&c)   // 兼容表意文字
        || (0x20000..=0x3134F).contains(&c) // 扩展 B..F(含兼容补充)
        || (0x3040..=0x309F).contains(&c)   // 平假名
        || (0x30A0..=0x30FF).contains(&c) // 片假名
}

/// 落盘 ASCII 词项缓冲(空则无操作)。
fn flush_ascii(run: &mut String, terms: &mut Vec<String>) {
    if !run.is_empty() {
        terms.push(std::mem::take(run));
    }
}

/// 落盘 CJK 词项缓冲:长度 ≥2 的串产出相邻 bigram(`数据库` →
/// `数据` `据库`);单字串退化为单字词项。索引与查询共用本函数,
/// 两侧词项集合对齐。
fn flush_cjk(run: &mut Vec<char>, terms: &mut Vec<String>) {
    match run.len() {
        0 => {}
        1 => terms.push(run[0].to_string()),
        _ => {
            for pair in run.windows(2) {
                terms.push(pair.iter().collect());
            }
        }
    }
    run.clear();
}

/// 对候选行集合按 BM25 打分并降序排列。
///
/// `query_terms` 为空时返回空 Vec。`candidates` 每项为 `(line_no, line_text)`。
pub fn rank_lines(
    query_terms: &[String],
    candidates: &[(usize, String)],
    k1: f64,
    b: f64,
) -> Vec<Bm25Hit> {
    if query_terms.is_empty() || candidates.is_empty() {
        return Vec::new();
    }

    let k1 = k1 as f32;
    let b = b as f32;
    let n_docs = candidates.len();
    let avg_dl = candidates
        .iter()
        .map(|(_, line)| tokenize(line).len())
        .sum::<usize>() as f32
        / n_docs as f32;

    // 每个 query term 在多少篇「文档」(行) 里出现过。
    let mut df = vec![0usize; query_terms.len()];
    let mut doc_tokens: Vec<Vec<String>> = Vec::with_capacity(n_docs);
    for (_, line) in candidates {
        let tokens = tokenize(line);
        for (i, term) in query_terms.iter().enumerate() {
            if tokens.iter().any(|t| t == term) {
                df[i] += 1;
            }
        }
        doc_tokens.push(tokens);
    }

    let mut scored: Vec<Bm25Hit> = candidates
        .iter()
        .enumerate()
        .map(|(doc_idx, (line_no, line))| {
            let dl = doc_tokens[doc_idx].len() as f32;
            let mut score = 0.0_f64;
            for (i, term) in query_terms.iter().enumerate() {
                let tf = doc_tokens[doc_idx].iter().filter(|t| *t == term).count() as f32;
                if tf == 0.0 {
                    continue;
                }
                let idf = bm25_idf_plus1(n_docs as u32, df[i] as u32);
                score += f64::from(idf * bm25_tf(tf, dl, avg_dl, k1, b));
            }
            Bm25Hit {
                score,
                line_no: *line_no,
                line: line.clone(),
            }
        })
        .filter(|h| h.score > 0.0)
        .collect();

    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_splits_on_non_alnum() {
        assert_eq!(
            tokenize("Hello World_foo-bar"),
            vec!["hello", "world_foo", "bar"]
        );
    }

    #[test]
    fn tokenize_pure_chinese_yields_bigrams() {
        // 纯中文不再零词项:相邻二元组。
        assert_eq!(tokenize("数据库配置"), vec!["数据", "据库", "库配", "配置"]);
        // 单字串退化为单字词项。
        assert_eq!(tokenize("库"), vec!["库"]);
    }

    #[test]
    fn tokenize_mixed_runs_do_not_cross_class() {
        // 中英边界不产出跨类 bigram(`用p` 不存在)。
        assert_eq!(
            tokenize("使用PostgreSQL数据库"),
            vec!["使用", "postgresql", "数据", "据库"]
        );
    }

    #[test]
    fn tokenize_fullwidth_folds_to_ascii() {
        // 全角字母 / 数字折叠为半角后与 ASCII 同词。
        assert_eq!(tokenize("ＺｈｕＹｅ１６"), vec!["zhuye16"]);
    }

    #[test]
    fn tokenize_cjk_punctuation_is_separator() {
        // CJK 标点是分隔符,不进词项;句读切断 bigram。
        assert_eq!(tokenize("数据库。索引,"), vec!["数据", "据库", "索引"]);
    }

    #[test]
    fn chinese_query_ranks_chinese_line() {
        // 回归:中文查询此前零词项(rank_lines 返回空),现在能命中中文候选。
        let ranked = rank_lines(
            &tokenize("数据库连接池"),
            &[
                (1, "部署配置见 deploy.md".into()),
                (2, "数据库用 postgres 16".into()),
            ],
            1.2,
            0.75,
        );
        assert!(
            ranked.iter().any(|h| h.line_no == 2),
            "中文查询必须命中中文行:{ranked:?}"
        );
    }

    #[test]
    fn rank_lines_orders_by_relevance() {
        let query = tokenize("async fn main");
        let candidates = vec![
            (1, "let x = 1;".to_string()),
            (2, "async fn main() { println!(\"hi\"); }".to_string()),
            (3, "async fn main() { do_work().await; }".to_string()),
        ];
        let ranked = rank_lines(&query, &candidates, 1.2, 0.75);
        assert_eq!(ranked.len(), 2);
        assert!(ranked[0].score >= ranked[1].score);
        assert!(ranked.iter().any(|h| h.line_no == 2));
        assert!(ranked.iter().any(|h| h.line_no == 3));
    }

    #[test]
    fn rank_lines_empty_query_returns_empty() {
        assert!(rank_lines(&[], &[(1, "x".into())], 1.2, 0.75).is_empty());
    }
}
