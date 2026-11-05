//! 用不允许删除共享的目录句柄固定路径，并在打开页面后检查重解析点。

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const SHARE_READ_WRITE: u32 = 0x1 | 0x2;
const FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

fn check_reparse(file: &File) -> io::Result<()> {
    if file.metadata()?.file_attributes() & ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "KMS reparse point is not allowed",
        ));
    }
    Ok(())
}

fn pin_directory(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .share_mode(SHARE_READ_WRITE)
        .custom_flags(FLAG_BACKUP_SEMANTICS | FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    check_reparse(&file)?;
    if !file.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "KMS path is not a directory",
        ));
    }
    Ok(file)
}

pub(super) struct PinnedDir {
    pub(super) path: PathBuf,
    // 保存所有句柄，使父目录在文件操作结束前不能被重命名或替换。
    _directories: Vec<File>,
}

pub(super) fn pin_root(root: &Path) -> io::Result<PinnedDir> {
    let root = std::fs::canonicalize(root)?;
    let mut directories = Vec::new();
    for ancestor in root.ancestors().collect::<Vec<_>>().into_iter().rev() {
        directories.push(pin_directory(ancestor)?);
    }
    Ok(PinnedDir {
        path: root,
        _directories: directories,
    })
}

pub(super) fn pin_child(parent: &PinnedDir, name: &str) -> io::Result<PinnedDir> {
    let path = parent.path.join(name);
    let child = pin_directory(&path)?;
    let mut directories = parent
        ._directories
        .iter()
        .map(File::try_clone)
        .collect::<io::Result<Vec<_>>>()?;
    directories.push(child);
    Ok(PinnedDir {
        path,
        _directories: directories,
    })
}

pub(super) fn pinned_modified(dir: &PinnedDir) -> u64 {
    dir._directories.last().map_or(0, modified_ms)
}

fn pin_pages(root: &Path, wiki: &str, create: bool) -> io::Result<PinnedDir> {
    let root = pin_root(root)?;
    let wiki = pin_child(&root, wiki)?;
    let pages = wiki.path.join("pages");
    if create {
        match std::fs::create_dir(&pages) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    pin_child(&wiki, "pages")
}

pub(super) fn modified_ms(file: &File) -> u64 {
    file.metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(super) fn open_file(dir: &PinnedDir, name: &str) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .share_mode(SHARE_READ_WRITE)
        .custom_flags(FLAG_OPEN_REPARSE_POINT)
        .open(dir.path.join(name))?;
    check_reparse(&file)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "KMS page is not a regular file",
        ));
    }
    Ok(file)
}

pub(super) fn read_file_at(dir: &PinnedDir, name: &str) -> io::Result<(String, u64)> {
    let mut file = open_file(dir, name)?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    Ok((content, modified_ms(&file)))
}

pub(super) fn write_new_file(dir: &PinnedDir, name: &str, content: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(SHARE_READ_WRITE)
        .custom_flags(FLAG_OPEN_REPARSE_POINT)
        .open(dir.path.join(name))?;
    check_reparse(&file)?;
    file.write_all(content)
}

pub(super) fn read(root: &Path, wiki: &str, page: &str) -> io::Result<(String, u64)> {
    let pages = pin_pages(root, wiki, false)?;
    read_file_at(&pages, &format!("{page}.md"))
}

pub(super) fn write(
    root: &Path,
    wiki: &str,
    page: &str,
    content: &[u8],
    create_only: bool,
) -> io::Result<u64> {
    let pages = pin_pages(root, wiki, true)?;
    write_to_dir(&pages, page, create_only, |file| file.write_all(content))
}

fn write_to_dir(
    pages: &PinnedDir,
    page: &str,
    create_only: bool,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<u64> {
    let target = pages.path.join(format!("{page}.md"));
    let mut temporary = tempfile::NamedTempFile::new_in(&pages.path)?;
    write(temporary.as_file_mut())?;
    temporary.as_file().sync_all()?;

    if !create_only {
        match std::fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_attributes() & ATTRIBUTE_REPARSE_POINT != 0 => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "KMS reparse point is not allowed",
                ));
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "KMS page is not a regular file",
                ));
            }
            Ok(metadata) if metadata.permissions().readonly() => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "KMS page is read-only",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let file = if create_only {
        temporary
            .persist_noclobber(&target)
            .map_err(|error| error.error)?
    } else {
        temporary.persist(&target).map_err(|error| error.error)?
    };
    Ok(modified_ms(&file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_directory_cannot_be_renamed_until_operation_finishes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("wiki/pages")).unwrap();
        let pinned = pin_pages(root.path(), "wiki", false).unwrap();
        assert!(
            std::fs::rename(
                root.path().join("wiki/pages"),
                root.path().join("wiki/moved")
            )
            .is_err()
        );
        drop(pinned);
        std::fs::rename(
            root.path().join("wiki/pages"),
            root.path().join("wiki/moved"),
        )
        .unwrap();
    }

    #[test]
    fn failed_write_preserves_original_and_removes_temporary_file() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("wiki/pages")).unwrap();
        let target = root.path().join("wiki/pages/page.md");
        std::fs::write(&target, "original").unwrap();
        let pages = pin_pages(root.path(), "wiki", false).unwrap();
        assert!(
            write_to_dir(&pages, "page", false, |file| {
                file.write_all(b"partial")?;
                Err(io::Error::other("simulated failure"))
            })
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
        assert_eq!(
            std::fs::read_dir(root.path().join("wiki/pages"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn exclusive_create_does_not_overwrite_existing_page() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("wiki/pages")).unwrap();
        let target = root.path().join("wiki/pages/page.md");
        std::fs::write(&target, "original").unwrap();
        let pages = pin_pages(root.path(), "wiki", false).unwrap();
        assert!(write_to_dir(&pages, "page", true, |file| file.write_all(b"replacement")).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "original");
    }

    #[test]
    fn successful_write_replaces_existing_page() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("wiki/pages")).unwrap();
        let target = root.path().join("wiki/pages/page.md");
        std::fs::write(&target, "original").unwrap();
        let pages = pin_pages(root.path(), "wiki", false).unwrap();
        write_to_dir(&pages, "page", false, |file| file.write_all(b"updated")).unwrap();
        assert_eq!(std::fs::read_to_string(target).unwrap(), "updated");
    }
}
