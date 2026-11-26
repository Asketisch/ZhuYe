//! 固定 KMS 根目录后创建或删除知识库。

use super::windows_page::{pin_child, pin_root, pinned_modified, write_new_file};
use std::io;
use std::path::Path;

pub(super) fn create(root_path: &Path, name: &str, index: &[u8]) -> io::Result<u64> {
    std::fs::create_dir_all(root_path)?;
    let root = pin_root(root_path)?;
    let path = root.path.join(name);
    std::fs::create_dir(&path)?;
    let wiki = pin_child(&root, name)?;
    match std::fs::create_dir(wiki.path.join("pages")) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let _pages = pin_child(&wiki, "pages")?;
    write_new_file(&wiki, "index.md", index)?;
    Ok(pinned_modified(&wiki))
}

pub(super) fn delete(root_path: &Path, name: &str) -> io::Result<()> {
    let root = pin_root(root_path)?;
    // Windows 标准库的递归删除不跟随符号链接，并通过句柄遍历子项。
    // 根目录保持固定；不固定待删除目录自身，否则会阻止其被删除。
    std::fs::remove_dir_all(root.path.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn creates_and_removes_wiki_under_pinned_root() {
        let root = tempfile::tempdir().unwrap();
        create(root.path(), "wiki", b"index").unwrap();
        assert_eq!(
            std::fs::read(root.path().join("wiki/index.md")).unwrap(),
            b"index"
        );
        assert_eq!(
            create(root.path(), "wiki", b"other").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        delete(root.path(), "wiki").unwrap();
        assert!(!root.path().join("wiki").exists());
    }

    #[test]
    fn deletion_does_not_follow_directory_link() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.md"), "outside").unwrap();
        if let Err(error) =
            std::os::windows::fs::symlink_dir(outside.path(), root.path().join("wiki"))
        {
            if error.raw_os_error() == Some(1314) {
                eprintln!("无法创建符号链接：当前账户没有创建权限");
                return;
            }
            panic!("创建测试符号链接失败: {error}");
        }
        let _ = delete(root.path(), "wiki");
        assert_eq!(
            std::fs::read_to_string(outside.path().join("secret.md")).unwrap(),
            "outside"
        );
    }
}
