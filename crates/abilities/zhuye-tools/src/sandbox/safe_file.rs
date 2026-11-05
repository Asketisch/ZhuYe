//! 固定已验证的文件句柄，避免校验后路径替换改变读取目标。

use std::fs::File;
use std::io;
use std::path::Path;

/// 只读文件及其关联的目录保护句柄；使用期间必须持有整个对象。
pub struct PinnedFile {
    /// 已打开的普通文件；复制句柄时仍需保留此对象。
    pub file: File,
    // Windows 目录句柄不共享删除权限，须持有到请求结束。
    _directories: Vec<File>,
}

#[cfg(unix)]
pub(super) fn directory_flags() -> rustix::fs::OFlags {
    use rustix::fs::OFlags;
    let flags = OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let flags = flags | OFlags::PATH;
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    // Darwin O_SEARCH = O_EXEC | O_DIRECTORY；只需要搜索权限，不要求目录可列举。
    let flags = flags | OFlags::from_bits_retain(0x4000_0000);
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )))]
    let flags = flags | OFlags::RDONLY;
    flags
}

#[cfg(unix)]
/// 打开已经通过工作区边界校验的绝对路径，并拒绝沿途符号链接。
pub fn open_readonly(path: &Path) -> io::Result<PinnedFile> {
    use rustix::fs::{self, Mode, OFlags};
    use std::path::Component;

    let mut parts = path.components();
    if parts.next() != Some(Component::RootDir) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "file path must be absolute",
        ));
    }
    let parts: Vec<_> = parts.collect();
    let (last, parents) = parts
        .split_last()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file path has no file"))?;
    let flags = directory_flags();
    let mut dir = File::from(fs::open("/", flags, Mode::empty())?);
    for part in parents {
        let Component::Normal(name) = part else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file path is not normalized",
            ));
        };
        dir = File::from(fs::openat(&dir, *name, flags, Mode::empty())?);
    }
    let Component::Normal(name) = last else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "file path has no file",
        ));
    };
    let file = File::from(fs::openat(
        &dir,
        *name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )?);
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "file is not a regular file",
        ));
    }
    Ok(PinnedFile {
        file,
        _directories: Vec::new(),
    })
}

#[cfg(windows)]
/// 打开已经通过工作区边界校验的绝对路径，并固定目录和拒绝重解析点。
pub fn open_readonly(path: &Path) -> io::Result<PinnedFile> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    const SHARE_READ_WRITE: u32 = 0x1 | 0x2;
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const REPARSE_POINT: u32 = 0x400;
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "file path must be absolute",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file has no parent"))?;
    let mut dirs = Vec::new();
    for ancestor in parent.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let dir = OpenOptions::new()
            .read(true)
            .share_mode(SHARE_READ_WRITE)
            .custom_flags(BACKUP_SEMANTICS | OPEN_REPARSE_POINT)
            .open(ancestor)?;
        let meta = dir.metadata()?;
        if !meta.is_dir() || meta.file_attributes() & REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "file directory is a reparse point",
            ));
        }
        dirs.push(dir);
    }
    let file = OpenOptions::new()
        .read(true)
        .share_mode(SHARE_READ_WRITE)
        .custom_flags(OPEN_REPARSE_POINT)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.file_attributes() & REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "file is not a regular file",
        ));
    }
    Ok(PinnedFile {
        file,
        _directories: dirs,
    })
}

#[cfg(not(any(unix, windows)))]
/// 不支持安全文件句柄的平台拒绝访问。
pub fn open_readonly(_path: &Path) -> io::Result<PinnedFile> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure file opening is unavailable",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::symlink;

    #[test]
    fn directory_replacement_cannot_redirect_read() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join("dir")).unwrap();
        std::fs::write(workspace.path().join("dir/doc.rs"), "inside").unwrap();
        std::fs::write(outside.path().join("doc.rs"), "outside").unwrap();
        let path = workspace.path().join("dir/doc.rs").canonicalize().unwrap();
        let mut pinned = open_readonly(&path).unwrap();
        std::fs::rename(workspace.path().join("dir"), workspace.path().join("old")).unwrap();
        symlink(outside.path(), workspace.path().join("dir")).unwrap();
        assert!(open_readonly(&path).is_err());
        let mut text = String::new();
        pinned.file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "inside");
    }

    #[test]
    fn leaf_symlink_is_not_followed() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.rs");
        std::fs::write(&target, "content").unwrap();
        let link = root.path().join("link.rs");
        symlink(&target, &link).unwrap();
        let parent = root.path().canonicalize().unwrap();
        assert!(open_readonly(&parent.join("link.rs")).is_err());
    }

    #[test]
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))]
    fn searchable_but_unreadable_directory_remains_usable() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("search-only");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("file.rs"), "content").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o111)).unwrap();
        let path = dir.join("file.rs").canonicalize().unwrap();
        let result = open_readonly(&path);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut document = result.unwrap();
        let mut text = String::new();
        document.file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "content");
    }
}
