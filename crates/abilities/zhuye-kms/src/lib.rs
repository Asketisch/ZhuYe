//! 知识管理系统 (KMS) —— 基于 grep 的本地知识库 wiki。
//!
//! 知识库以 Markdown 文件存储于 `~/.zhuye/kms/<name>/`
//! 各页面位于 `pages/*.md`。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

mod browse;
mod dream;
mod helpers;
mod path_guard;
#[cfg(unix)]
mod unix_admin;
#[cfg(unix)]
mod unix_browse;
#[cfg(unix)]
mod unix_page;
#[cfg(windows)]
mod windows_admin;
#[cfg(windows)]
mod windows_browse;
#[cfg(windows)]
mod windows_page;
#[cfg(not(any(unix, windows)))]
use helpers::count_md_files;
#[cfg(not(any(unix, windows)))]
use helpers::get_modified_at_ms;
use helpers::{SaveMode, extract_description, extract_tags, extract_title};
use path_guard::{reject_links, validate_name};

/// 知识库的元数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WikiInfo {
    /// Wiki 名称（目录名）。
    pub name: String,
    /// 可选描述。
    pub description: Option<String>,
    /// Wiki 目录的路径。
    pub path: PathBuf,
    /// Wiki 中的页面数。
    pub page_count: usize,
    /// 最后修改时间戳（自 epoch 起的毫秒数）。
    pub modified_at_ms: u64,
}

/// 知识库中的单个页面。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    /// 页面名称（不含 .md 的文件名）。
    pub name: String,
    /// 此页面所属的 Wiki。
    pub wiki: String,
    /// 页面内容（Markdown）。
    pub content: String,
    /// 来自 frontmatter 的可选标题。
    pub title: Option<String>,
    /// 来自 frontmatter 的可选标签。
    pub tags: Vec<String>,
    /// 最后修改时间戳（自 epoch 起的毫秒数）。
    pub modified_at_ms: u64,
}

/// 知识库 grep 的搜索结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    /// Wiki 名称。
    pub wiki: String,
    /// 页面名称。
    pub page: String,
    /// 匹配的行。
    pub lines: Vec<String>,
    /// 行数。
    pub line_count: usize,
}

/// /dream 命令的会话挖掘结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DreamResult {
    /// 提取的洞察。
    pub insights: Vec<String>,
    /// 已分析的会话数。
    pub sessions_analyzed: usize,
    /// 洞察保存到的 Wiki 名称。
    pub wiki: String,
    /// 洞察保存到的页面名称。
    pub page: String,
}

/// KMS 操作的错误类型。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KmsError {
    /// 未找到 Wiki。
    WikiNotFound {
        /// Wiki 名称。
        name: String,
    },
    /// 未找到页面。
    PageNotFound {
        /// Wiki 名称。
        wiki: String,
        /// 页面名称。
        page: String,
    },
    /// Wiki 已存在。
    WikiAlreadyExists {
        /// Wiki 名称。
        name: String,
    },
    /// 页面在编辑期间已改变或被删除。
    PageChanged {
        /// Wiki 名称。
        wiki: String,
        /// 页面名称。
        page: String,
    },
    /// I/O 错误。
    IoError {
        /// 错误信息。
        message: String,
    },
}

impl std::fmt::Display for KmsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KmsError::WikiNotFound { name } => write!(f, "Wiki not found: {}", name),
            KmsError::PageNotFound { wiki, page } => write!(f, "Page not found: {}/{}", wiki, page),
            KmsError::WikiAlreadyExists { name } => write!(f, "Wiki already exists: {}", name),
            KmsError::PageChanged { wiki, page } => {
                write!(f, "Page changed or was deleted: {}/{}", wiki, page)
            }
            KmsError::IoError { message } => write!(f, "I/O error: {}", message),
        }
    }
}

impl std::error::Error for KmsError {}

impl From<std::io::Error> for KmsError {
    fn from(e: std::io::Error) -> Self {
        KmsError::IoError {
            message: e.to_string(),
        }
    }
}

/// 处理所有知识库操作的 KMS 管理器。
pub struct KnowledgeManager {
    /// 所有知识库的根目录。
    root: PathBuf,
    write_lock: parking_lot::Mutex<()>,
}

impl KnowledgeManager {
    /// 使用指定的根目录创建新的 KnowledgeManager。
    pub fn new(root: PathBuf) -> Self {
        std::fs::create_dir_all(&root).ok();
        Self {
            root,
            write_lock: parking_lot::Mutex::new(()),
        }
    }

    /// 使用默认主目录创建新的 KnowledgeManager。
    pub fn with_default_home() -> Self {
        let root = zhuye_config::data_path_or_relative(&["kms"]);
        Self::new(root)
    }

    /// 获取 Wiki 目录的路径。
    pub fn wiki_path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// 获取 Wiki 内页面文件的路径。
    pub fn page_path(&self, wiki: &str, page: &str) -> PathBuf {
        self.root
            .join(wiki)
            .join("pages")
            .join(format!("{}.md", page))
    }

    /// 创建新的知识库。
    pub fn create_wiki(
        &self,
        name: &str,
        description: Option<String>,
    ) -> Result<WikiInfo, KmsError> {
        let _guard = self.write_lock.lock();
        validate_name(name)?;
        reject_links(&self.root, std::path::Path::new(name))?;
        let wiki_path = self.wiki_path(name);
        let mut index_content = String::from("---\n");
        if let Some(desc) = &description {
            // JSON 字符串同时是合法的 YAML 双引号标量，能够保留换行和空白。
            let encoded = serde_json::to_string(desc).expect("serializing a string cannot fail");
            index_content.push_str(&format!("description: {encoded}\n"));
        }
        index_content.push_str("---\n");
        #[cfg(unix)]
        let modified_at_ms = unix_admin::create(&self.root, name, index_content.as_bytes())
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    KmsError::WikiAlreadyExists {
                        name: name.to_string(),
                    }
                } else {
                    error.into()
                }
            })?;
        #[cfg(windows)]
        let modified_at_ms = windows_admin::create(&self.root, name, index_content.as_bytes())
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    KmsError::WikiAlreadyExists {
                        name: name.to_string(),
                    }
                } else {
                    error.into()
                }
            })?;
        #[cfg(not(any(unix, windows)))]
        let modified_at_ms = {
            std::fs::create_dir_all(&self.root)?;
            if let Err(error) = std::fs::create_dir(&wiki_path) {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    return Err(KmsError::WikiAlreadyExists {
                        name: name.to_string(),
                    });
                }
                return Err(error.into());
            }
            std::fs::create_dir_all(wiki_path.join("pages"))?;
            std::fs::write(wiki_path.join("index.md"), index_content)?;
            get_modified_at_ms(&wiki_path)
        };

        Ok(WikiInfo {
            name: name.to_string(),
            description,
            path: wiki_path,
            page_count: 0,
            modified_at_ms,
        })
    }

    /// 删除知识库。
    pub fn delete_wiki(&self, name: &str) -> Result<(), KmsError> {
        let _guard = self.write_lock.lock();
        validate_name(name)?;
        reject_links(&self.root, std::path::Path::new(name))?;
        #[cfg(unix)]
        {
            unix_admin::delete(&self.root, name).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    KmsError::WikiNotFound {
                        name: name.to_string(),
                    }
                } else {
                    error.into()
                }
            })
        }
        #[cfg(windows)]
        {
            windows_admin::delete(&self.root, name).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    KmsError::WikiNotFound {
                        name: name.to_string(),
                    }
                } else {
                    error.into()
                }
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let wiki_path = self.wiki_path(name);
            if !wiki_path.exists() {
                return Err(KmsError::WikiNotFound {
                    name: name.to_string(),
                });
            }
            std::fs::remove_dir_all(&wiki_path)?;
            Ok(())
        }
    }

    /// 将页面保存到知识库。
    pub fn save_page(
        &self,
        wiki: &str,
        page: &str,
        content: &str,
        title: Option<String>,
        tags: Vec<String>,
    ) -> Result<Page, KmsError> {
        self.save_page_inner(wiki, page, content, title, tags, SaveMode::Overwrite)
    }

    /// 创建页面；同名页面已存在时不覆盖。
    pub fn create_page(
        &self,
        wiki: &str,
        page: &str,
        content: &str,
        title: Option<String>,
        tags: Vec<String>,
    ) -> Result<Page, KmsError> {
        self.save_page_inner(wiki, page, content, title, tags, SaveMode::Create)
    }

    /// 仅当页面仍与读取时一致时保存。
    pub fn save_page_if_unchanged(
        &self,
        wiki: &str,
        page: &str,
        content: &str,
        title: Option<String>,
        tags: Vec<String>,
        expected: &str,
    ) -> Result<Page, KmsError> {
        self.save_page_inner(
            wiki,
            page,
            content,
            title,
            tags,
            SaveMode::IfUnchanged(expected),
        )
    }

    fn save_page_inner(
        &self,
        wiki: &str,
        page: &str,
        content: &str,
        title: Option<String>,
        tags: Vec<String>,
        mode: SaveMode<'_>,
    ) -> Result<Page, KmsError> {
        let _guard = self.write_lock.lock();
        validate_name(wiki)?;
        validate_name(page)?;
        if let SaveMode::IfUnchanged(expected) = mode {
            match self.get_page(wiki, page) {
                Ok(current) if current.content == expected => {}
                Ok(_) | Err(KmsError::PageNotFound { .. }) | Err(KmsError::WikiNotFound { .. }) => {
                    return Err(KmsError::PageChanged {
                        wiki: wiki.to_string(),
                        page: page.to_string(),
                    });
                }
                Err(error) => return Err(error),
            }
        }
        let create_only = matches!(mode, SaveMode::Create);
        reject_links(
            &self.root,
            &std::path::Path::new(wiki)
                .join("pages")
                .join(format!("{page}.md")),
        )?;
        let wiki_path = self.wiki_path(wiki);
        if !wiki_path.exists() {
            return Err(KmsError::WikiNotFound {
                name: wiki.to_string(),
            });
        }

        #[cfg(not(any(unix, windows)))]
        std::fs::create_dir_all(wiki_path.join("pages"))?;

        // 无显式元数据时原样保存，以支持对包含 frontmatter 的完整页面往返编辑。
        let full_content = if title.is_none() && tags.is_empty() {
            content.to_string()
        } else {
            let mut result = String::from("---\n");
            if let Some(t) = &title {
                result.push_str(&format!("title: {}\n", t));
            }
            if !tags.is_empty() {
                result.push_str(&format!("tags: [{}]\n", tags.join(", ")));
            }
            result.push_str("---\n");
            result.push_str(content);
            result
        };

        #[cfg(unix)]
        let modified_at_ms =
            unix_page::write(&self.root, wiki, page, full_content.as_bytes(), create_only)?;
        #[cfg(windows)]
        let modified_at_ms =
            windows_page::write(&self.root, wiki, page, full_content.as_bytes(), create_only)?;
        #[cfg(not(any(unix, windows)))]
        let modified_at_ms = {
            let page_path = self.page_path(wiki, page);
            if create_only {
                use std::io::Write;
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&page_path)?;
                if let Err(error) = file.write_all(full_content.as_bytes()) {
                    drop(file);
                    let _ = std::fs::remove_file(&page_path);
                    return Err(error.into());
                }
            } else {
                std::fs::write(&page_path, &full_content)?;
            }

            get_modified_at_ms(&page_path)
        };

        let title = extract_title(&full_content);
        let tags = extract_tags(&full_content);
        Ok(Page {
            name: page.to_string(),
            wiki: wiki.to_string(),
            content: full_content,
            title,
            tags,
            modified_at_ms,
        })
    }

    /// 从知识库获取页面。
    pub fn get_page(&self, wiki: &str, page: &str) -> Result<Page, KmsError> {
        validate_name(wiki)?;
        validate_name(page)?;
        reject_links(
            &self.root,
            &std::path::Path::new(wiki)
                .join("pages")
                .join(format!("{page}.md")),
        )?;
        #[cfg(unix)]
        let (content, modified_at_ms) =
            unix_page::read(&self.root, wiki, page).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    KmsError::PageNotFound {
                        wiki: wiki.to_string(),
                        page: page.to_string(),
                    }
                } else {
                    error.into()
                }
            })?;
        #[cfg(windows)]
        let (content, modified_at_ms) =
            windows_page::read(&self.root, wiki, page).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    KmsError::PageNotFound {
                        wiki: wiki.to_string(),
                        page: page.to_string(),
                    }
                } else {
                    error.into()
                }
            })?;
        #[cfg(not(any(unix, windows)))]
        let (content, modified_at_ms) = {
            let page_path = self.page_path(wiki, page);
            if !page_path.exists() {
                return Err(KmsError::PageNotFound {
                    wiki: wiki.to_string(),
                    page: page.to_string(),
                });
            }

            let content = std::fs::read_to_string(&page_path)?;
            let modified_at_ms = get_modified_at_ms(&page_path);
            (content, modified_at_ms)
        };
        let title = extract_title(&content);
        let tags = extract_tags(&content);

        Ok(Page {
            name: page.to_string(),
            wiki: wiki.to_string(),
            content,
            title,
            tags,
            modified_at_ms,
        })
    }
}

#[cfg(test)]
mod tests;
