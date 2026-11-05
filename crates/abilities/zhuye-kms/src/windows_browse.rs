//! 固定目录句柄后列举和读取 Windows 知识库。

use super::*;
use std::os::windows::fs::MetadataExt;
use windows_page::{PinnedDir, pin_child, pin_root, read_file_at};

fn names(dir: &PinnedDir) -> std::io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&dir.path)? {
        if let Ok(name) = entry?.file_name().into_string() {
            names.push(name);
        }
    }
    Ok(names)
}

fn child_directory(parent: &PinnedDir, name: &str) -> std::io::Result<Option<PinnedDir>> {
    let metadata = match std::fs::symlink_metadata(parent.path.join(name)) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if metadata.file_attributes() & 0x400 != 0 || !metadata.is_dir() {
        return Ok(None);
    }
    pin_child(parent, name).map(Some)
}

fn pages_from_dir(dir: &PinnedDir, wiki: &str) -> std::io::Result<Vec<Page>> {
    let mut pages = Vec::new();
    for filename in names(dir)? {
        let Some(name) = filename.strip_suffix(".md") else {
            continue;
        };
        let metadata = match std::fs::symlink_metadata(dir.path.join(&filename)) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if metadata.file_attributes() & 0x400 != 0 || !metadata.is_file() {
            continue;
        }
        let (content, modified_at_ms) = match read_file_at(dir, &filename) {
            Ok(page) => page,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        pages.push(Page {
            name: name.to_string(),
            wiki: wiki.to_string(),
            title: extract_title(&content),
            tags: extract_tags(&content),
            content,
            modified_at_ms,
        });
    }
    Ok(pages)
}

fn count_pages(dir: &PinnedDir) -> std::io::Result<usize> {
    let mut count = 0;
    for name in names(dir)? {
        if !name.ends_with(".md") {
            continue;
        }
        let metadata = match std::fs::symlink_metadata(dir.path.join(name)) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if metadata.file_attributes() & 0x400 == 0 && metadata.is_file() {
            count += 1;
        }
    }
    Ok(count)
}

pub(super) fn list_pages(root_path: &std::path::Path, name: &str) -> Result<Vec<Page>, KmsError> {
    let root = pin_root(root_path)?;
    let wiki = pin_child(&root, name).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            KmsError::WikiNotFound {
                name: name.to_string(),
            }
        } else {
            error.into()
        }
    })?;
    match pin_child(&wiki, "pages") {
        Ok(pages) => Ok(pages_from_dir(&pages, name)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn list_wikis(root_path: &std::path::Path) -> Result<Vec<WikiInfo>, KmsError> {
    let root = pin_root(root_path)?;
    let entries = names(&root)?;
    let mut result = Vec::new();
    for name in entries {
        let Some(wiki) = child_directory(&root, &name)? else {
            continue;
        };
        let index = match std::fs::symlink_metadata(wiki.path.join("index.md")) {
            Ok(metadata) if metadata.file_attributes() & 0x400 == 0 && metadata.is_file() => true,
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        let description = if index {
            extract_description(&read_file_at(&wiki, "index.md")?.0)
        } else {
            None
        };
        let page_count = match child_directory(&wiki, "pages")? {
            Some(pages) => count_pages(&pages)?,
            None => 0,
        };
        let modified_at_ms = windows_page::pinned_modified(&wiki);
        result.push(WikiInfo {
            path: root_path.join(&name),
            name,
            description,
            page_count,
            modified_at_ms,
        });
    }
    Ok(result)
}

pub(super) fn search(
    root_path: &std::path::Path,
    query: &str,
) -> Result<Vec<SearchResult>, KmsError> {
    let root = pin_root(root_path)?;
    let entries = names(&root)?;
    let mut result = Vec::new();
    for name in entries {
        let Some(wiki) = child_directory(&root, &name)? else {
            continue;
        };
        let Some(pages) = child_directory(&wiki, "pages")? else {
            continue;
        };
        let items = pages_from_dir(&pages, &name)?;
        for page in items {
            let lines: Vec<String> = page
                .content
                .lines()
                .filter(|line| line.contains(query))
                .map(str::to_owned)
                .collect();
            if !lines.is_empty() {
                result.push(SearchResult {
                    wiki: name.clone(),
                    page: page.name,
                    line_count: lines.len(),
                    lines,
                });
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_pages_remain_stable_during_enumeration() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("wiki/pages")).unwrap();
        std::fs::write(root.path().join("wiki/pages/page.md"), "original").unwrap();
        let root_dir = pin_root(root.path()).unwrap();
        let wiki = pin_child(&root_dir, "wiki").unwrap();
        let pages = pin_child(&wiki, "pages").unwrap();
        assert!(
            std::fs::rename(
                root.path().join("wiki/pages"),
                root.path().join("wiki/moved")
            )
            .is_err()
        );
        assert_eq!(
            pages_from_dir(&pages, "wiki").unwrap()[0].content,
            "original"
        );
    }
}
