//! 使用已打开的目录句柄列举并读取知识库。

use super::*;
use rustix::fd::OwnedFd;
use rustix::fs::Dir;

fn names(dir: &OwnedFd) -> std::io::Result<Vec<String>> {
    let entries = Dir::read_from(dir)?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        if let Ok(name) = entry.file_name().to_str()
            && name != "."
            && name != ".."
        {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

fn pages_from_dir(pages: &OwnedFd, wiki: &str) -> std::io::Result<Vec<Page>> {
    let mut result = Vec::new();
    for filename in names(pages)? {
        let Some(name) = filename.strip_suffix(".md") else {
            continue;
        };
        let stat = match rustix::fs::statat(
            pages,
            filename.as_str(),
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(stat) => stat,
            Err(rustix::io::Errno::NOENT) => continue,
            Err(error) => return Err(error.into()),
        };
        if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile {
            continue;
        }
        let (content, modified_at_ms) = match unix_page::read_file_at(pages, &filename) {
            Ok(page) => page,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        result.push(Page {
            name: name.to_string(),
            wiki: wiki.to_string(),
            title: extract_title(&content),
            tags: extract_tags(&content),
            content,
            modified_at_ms,
        });
    }
    Ok(result)
}

fn count_pages(pages: &OwnedFd) -> std::io::Result<usize> {
    let mut count = 0;
    for name in names(pages)? {
        if !name.ends_with(".md") {
            continue;
        }
        let stat =
            match rustix::fs::statat(pages, name.as_str(), rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                Err(rustix::io::Errno::NOENT) => continue,
                Err(error) => return Err(error.into()),
            };
        if rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::RegularFile {
            count += 1;
        }
    }
    Ok(count)
}

pub(super) fn list_pages(root: &std::path::Path, wiki: &str) -> Result<Vec<Page>, KmsError> {
    let root = unix_page::open_root(root)?;
    let wiki_dir = unix_page::open_dir(&root, wiki).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            KmsError::WikiNotFound {
                name: wiki.to_string(),
            }
        } else {
            error.into()
        }
    })?;
    match unix_page::open_dir(&wiki_dir, "pages") {
        Ok(pages) => Ok(pages_from_dir(&pages, wiki)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn list_wikis(root_path: &std::path::Path) -> Result<Vec<WikiInfo>, KmsError> {
    let root = unix_page::open_root(root_path)?;
    let entries = names(&root)?;
    let mut result = Vec::new();
    for name in entries {
        let wiki = match unix_page::open_dir(&root, &name) {
            Ok(wiki) => wiki,
            Err(error) if matches!(error.raw_os_error(), Some(code) if code == rustix::io::Errno::NOENT.raw_os_error() || code == rustix::io::Errno::NOTDIR.raw_os_error() || code == rustix::io::Errno::LOOP.raw_os_error()) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let description = match unix_page::read_file_at(&wiki, "index.md") {
            Ok((content, _)) => extract_description(&content),
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) =>
            {
                None
            }
            Err(error) => return Err(error.into()),
        };
        let page_count = match unix_page::open_dir(&wiki, "pages") {
            Ok(pages) => count_pages(&pages)?,
            Err(error) if matches!(error.raw_os_error(), Some(code) if code == rustix::io::Errno::NOENT.raw_os_error() || code == rustix::io::Errno::NOTDIR.raw_os_error() || code == rustix::io::Errno::LOOP.raw_os_error()) => {
                0
            }
            Err(error) => return Err(error.into()),
        };
        let modified_at_ms = rustix::io::dup(&wiki)
            .ok()
            .map(std::fs::File::from)
            .map_or(0, |file| unix_page::modified_ms(&file));
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
    let root = unix_page::open_root(root_path)?;
    let entries = names(&root)?;
    let mut result = Vec::new();
    for name in entries {
        let wiki = match unix_page::open_dir(&root, &name) {
            Ok(wiki) => wiki,
            Err(error) if matches!(error.raw_os_error(), Some(code) if code == rustix::io::Errno::NOENT.raw_os_error() || code == rustix::io::Errno::NOTDIR.raw_os_error() || code == rustix::io::Errno::LOOP.raw_os_error()) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let pages = match unix_page::open_dir(&wiki, "pages") {
            Ok(pages) => pages,
            Err(error) if matches!(error.raw_os_error(), Some(code) if code == rustix::io::Errno::NOENT.raw_os_error() || code == rustix::io::Errno::NOTDIR.raw_os_error() || code == rustix::io::Errno::LOOP.raw_os_error()) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
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
    use std::os::unix::fs::symlink;

    #[test]
    fn enumerating_after_directory_replacement_reads_original_directory() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("wiki/pages")).unwrap();
        std::fs::write(root.path().join("wiki/pages/inside.md"), "inside").unwrap();
        std::fs::write(outside.path().join("secret.md"), "outside").unwrap();
        let root_fd = unix_page::open_root(root.path()).unwrap();
        let wiki_fd = unix_page::open_dir(&root_fd, "wiki").unwrap();
        let pages_fd = unix_page::open_dir(&wiki_fd, "pages").unwrap();
        std::fs::rename(
            root.path().join("wiki/pages"),
            root.path().join("wiki/old-pages"),
        )
        .unwrap();
        symlink(outside.path(), root.path().join("wiki/pages")).unwrap();
        let pages = pages_from_dir(&pages_fd, "wiki").unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].content, "inside");
        assert_eq!(count_pages(&pages_fd).unwrap(), 1);
    }
}
