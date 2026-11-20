#[cfg(not(any(unix, windows)))]
use std::path::Path;

pub(super) enum SaveMode<'a> {
    Overwrite,
    Create,
    IfUnchanged(&'a str),
}

/// 获取路径自 epoch 起的修改时间戳（毫秒）。
#[cfg(not(any(unix, windows)))]
pub(super) fn get_modified_at_ms(path: &Path) -> u64 {
    path.metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 统计目录中的 .md 文件数。
#[cfg(not(any(unix, windows)))]
pub(super) fn count_md_files(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let p = e.path();
                    e.file_type().is_ok_and(|kind| kind.is_file())
                        && p.extension().is_some_and(|ext| ext == "md")
                })
                .count()
        })
        .unwrap_or(0)
}

/// 从 Markdown frontmatter 中提取描述。
pub(super) fn extract_description(content: &str) -> Option<String> {
    let mut lines = content.lines();
    let first_line = lines.next().unwrap_or_default();
    if first_line != "---" {
        return None;
    }
    for line in lines {
        if line == "---" {
            break;
        }
        if line.starts_with("description: ") {
            let desc = line.trim_start_matches("description: ").trim().to_string();
            if let Ok(decoded) = serde_json::from_str::<String>(&desc) {
                return Some(decoded);
            }
            if !desc.is_empty() {
                return Some(desc);
            }
        }
    }
    None
}

/// 从 Markdown frontmatter 中提取标题。
pub(super) fn extract_title(content: &str) -> Option<String> {
    let mut lines = content.lines();
    let first_line = lines.next().unwrap_or_default();
    if first_line != "---" {
        return None;
    }
    for line in lines {
        if line == "---" {
            break;
        }
        if line.starts_with("title: ") {
            let title = line.trim_start_matches("title: ").trim().to_string();
            if !title.is_empty() {
                return Some(title);
            }
        }
    }
    None
}

/// 从 Markdown frontmatter 中提取标签。
pub(super) fn extract_tags(content: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let mut lines = content.lines();
    let first_line = lines.next().unwrap_or_default();
    if first_line != "---" {
        return tags;
    }
    for line in lines {
        if line == "---" {
            break;
        }
        if line.starts_with("tags: ") {
            let tags_str = line.trim_start_matches("tags: ").trim();
            let tags_str = tags_str.trim_matches('[').trim_matches(']');
            tags = tags_str
                .split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
        }
    }
    tags
}
