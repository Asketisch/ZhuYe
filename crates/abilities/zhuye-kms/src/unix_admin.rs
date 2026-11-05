//! 通过根目录句柄创建和删除知识库，不跟随目录项的符号链接。

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use rustix::fd::OwnedFd;
use rustix::fs::{self, AtFlags, Dir, Mode, OFlags};

use super::unix_page;

pub(super) fn create(root_path: &Path, name: &str, index: &[u8]) -> io::Result<u64> {
    std::fs::create_dir_all(root_path)?;
    let root = unix_page::open_root(root_path)?;
    fs::mkdirat(&root, name, Mode::from_raw_mode(0o777))?;
    let wiki = unix_page::open_dir(&root, name)?;
    initialize_wiki(&wiki, index)
}

fn initialize_wiki(wiki: &OwnedFd, index: &[u8]) -> io::Result<u64> {
    match fs::mkdirat(wiki, "pages", Mode::from_raw_mode(0o777)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(error) => return Err(error.into()),
    }
    // 已存在的 pages 可能被并发替换，打开时验证它是实际目录。
    unix_page::open_dir(wiki, "pages")?;
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = fs::openat(wiki, "index.md", flags, Mode::from_raw_mode(0o666))?;
    let mut file = File::from(fd);
    file.write_all(index)?;
    let wiki_file = File::from(rustix::io::dup(wiki)?);
    Ok(unix_page::modified_ms(&wiki_file))
}

fn remove_contents(dir: &OwnedFd) -> io::Result<()> {
    for entry in Dir::read_from(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        match fs::openat(dir, name, flags, Mode::empty()) {
            Ok(child) => {
                remove_contents(&child)?;
                match fs::unlinkat(dir, name, AtFlags::REMOVEDIR) {
                    Ok(()) | Err(rustix::io::Errno::NOENT) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Err(rustix::io::Errno::NOENT) => {}
            Err(rustix::io::Errno::NOTDIR) | Err(rustix::io::Errno::LOOP) => {
                match fs::unlinkat(dir, name, AtFlags::empty()) {
                    Ok(()) | Err(rustix::io::Errno::NOENT) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(super) fn delete(root_path: &Path, name: &str) -> io::Result<()> {
    let root = unix_page::open_root(root_path)?;
    let wiki = unix_page::open_dir(&root, name)?;
    remove_contents(&wiki)?;
    fs::unlinkat(&root, name, AtFlags::REMOVEDIR)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn replacing_directory_during_removal_does_not_delete_outside_files() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("wiki/pages")).unwrap();
        std::fs::write(root.path().join("wiki/pages/own.md"), "inside").unwrap();
        std::fs::write(outside.path().join("secret.md"), "outside").unwrap();
        let root_fd = unix_page::open_root(root.path()).unwrap();
        let wiki_fd = unix_page::open_dir(&root_fd, "wiki").unwrap();
        std::fs::rename(
            root.path().join("wiki/pages"),
            root.path().join("wiki/old-pages"),
        )
        .unwrap();
        symlink(outside.path(), root.path().join("wiki/pages")).unwrap();
        remove_contents(&wiki_fd).unwrap();
        assert_eq!(
            std::fs::read_to_string(outside.path().join("secret.md")).unwrap(),
            "outside"
        );
        assert!(!root.path().join("wiki/pages").exists());
    }

    #[test]
    fn replacing_wiki_path_after_open_does_not_redirect_creation() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("wiki")).unwrap();
        let root_fd = unix_page::open_root(root.path()).unwrap();
        let wiki_fd = unix_page::open_dir(&root_fd, "wiki").unwrap();
        std::fs::rename(root.path().join("wiki"), root.path().join("original")).unwrap();
        symlink(outside.path(), root.path().join("wiki")).unwrap();
        initialize_wiki(&wiki_fd, b"inside").unwrap();
        assert!(!outside.path().join("index.md").exists());
        assert_eq!(
            std::fs::read(root.path().join("original/index.md")).unwrap(),
            b"inside"
        );
    }
}
