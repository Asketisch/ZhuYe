//! 知识库列表和搜索。

use super::*;

impl KnowledgeManager {
    /// 列出所有知识库。
    pub fn list_wikis(&self) -> Result<Vec<WikiInfo>, KmsError> {
        #[cfg(unix)]
        {
            unix_browse::list_wikis(&self.root)
        }
        #[cfg(windows)]
        {
            windows_browse::list_wikis(&self.root)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let mut wikis = Vec::new();
            let entries = std::fs::read_dir(&self.root)?;

            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if reject_links(&self.root, std::path::Path::new(&entry.file_name())).is_err() {
                    continue;
                }
                if !path.is_dir() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let name = name.to_string();

                let pages_dir = path.join("pages");
                let page_count =
                    if reject_links(&self.root, &std::path::Path::new(&name).join("pages")).is_ok()
                    {
                        count_md_files(&pages_dir)
                    } else {
                        0
                    };

                let description =
                    if reject_links(&self.root, &std::path::Path::new(&name).join("index.md"))
                        .is_ok()
                        && path.join("index.md").exists()
                    {
                        std::fs::read_to_string(path.join("index.md"))
                            .ok()
                            .as_deref()
                            .and_then(extract_description)
                    } else {
                        None
                    };

                let modified_at_ms = get_modified_at_ms(&path);

                wikis.push(WikiInfo {
                    name: name.clone(),
                    description,
                    path: path.clone(),
                    page_count,
                    modified_at_ms,
                });
            }
            Ok(wikis)
        }
    }

    /// 在所有知识库中搜索内容。
    pub fn search(&self, query: &str) -> Result<Vec<SearchResult>, KmsError> {
        #[cfg(unix)]
        {
            unix_browse::search(&self.root, query)
        }
        #[cfg(windows)]
        {
            windows_browse::search(&self.root, query)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let mut results = Vec::new();
            let entries = std::fs::read_dir(&self.root)?;

            for entry in entries.filter_map(|e| e.ok()) {
                let wiki_path = entry.path();
                if reject_links(
                    &self.root,
                    &std::path::Path::new(&entry.file_name()).join("pages"),
                )
                .is_err()
                {
                    continue;
                }
                if !wiki_path.is_dir() {
                    continue;
                }
                let Some(wiki_name) = wiki_path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let wiki_name = wiki_name.to_string();

                let pages_dir = wiki_path.join("pages");
                let Ok(page_entries) = std::fs::read_dir(&pages_dir) else {
                    continue;
                };

                for page_entry in page_entries.filter_map(|e| e.ok()) {
                    let page_path = page_entry.path();
                    if reject_links(
                        &self.root,
                        &std::path::Path::new(&entry.file_name())
                            .join("pages")
                            .join(page_entry.file_name()),
                    )
                    .is_err()
                    {
                        continue;
                    }
                    if !page_path.is_file() || page_path.extension().is_none_or(|e| e != "md") {
                        continue;
                    }
                    let Some(page_name) = page_path.file_stem().and_then(|s| s.to_str()) else {
                        continue;
                    };
                    let page_name = page_name.to_string();
                    let Ok(content) = std::fs::read_to_string(&page_path) else {
                        continue;
                    };

                    let mut matching_lines: Vec<String> = Vec::new();
                    for line in content.lines() {
                        if line.contains(query) {
                            matching_lines.push(line.to_string());
                        }
                    }

                    if !matching_lines.is_empty() {
                        let lc = matching_lines.len();
                        results.push(SearchResult {
                            wiki: wiki_name.clone(),
                            page: page_name,
                            lines: matching_lines,
                            line_count: lc,
                        });
                    }
                }
            }

            Ok(results)
        }
    }

    /// 列出知识库中的所有页面。
    pub fn list_pages(&self, wiki: &str) -> Result<Vec<Page>, KmsError> {
        validate_name(wiki)?;
        reject_links(&self.root, &std::path::Path::new(wiki).join("pages"))?;
        #[cfg(unix)]
        {
            unix_browse::list_pages(&self.root, wiki)
        }
        #[cfg(windows)]
        {
            windows_browse::list_pages(&self.root, wiki)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let wiki_path = self.wiki_path(wiki);
            if !wiki_path.exists() {
                return Err(KmsError::WikiNotFound {
                    name: wiki.to_string(),
                });
            }

            let pages_dir = wiki_path.join("pages");
            let mut pages = Vec::new();

            let Ok(entries) = std::fs::read_dir(&pages_dir) else {
                return Ok(pages);
            };

            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if reject_links(
                    &self.root,
                    &std::path::Path::new(wiki)
                        .join("pages")
                        .join(entry.file_name()),
                )
                .is_err()
                {
                    continue;
                }
                if !path.is_file() || path.extension().is_none_or(|e| e != "md") {
                    continue;
                }
                let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Ok(content) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let title = extract_title(&content);
                let tags = extract_tags(&content);
                let modified_at_ms = get_modified_at_ms(&path);

                pages.push(Page {
                    name: name.to_string(),
                    wiki: wiki.to_string(),
                    content,
                    title,
                    tags,
                    modified_at_ms,
                });
            }

            Ok(pages)
        }
    }
}
