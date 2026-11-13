//! 固定目录后读取旧内容并原子替换文件。

use std::io::{self, Read, Write};
use std::path::Path;

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
mod metadata;
#[cfg(windows)]
mod metadata_windows;

/// 旧内容上限：与 `read` 工具一致，避免为算 diff 而无界占用内存。
const MAX_DIFF_BYTES: u64 = 32 * 1024 * 1024;

/// 读取已固定句柄中的旧内容。
///
/// `Ok(None)` 表示「文件存在但无法按文本 diff」（非 UTF-8，或超过
/// `MAX_DIFF_BYTES`）：调用方仍应完成写入，但不能把旧内容当作空串，
/// 否则 diff 会谎称文件原本是空的。
fn read_text(file: std::fs::File) -> io::Result<Option<String>> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "target is not a regular file",
        ));
    }
    if metadata.len() > MAX_DIFF_BYTES {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    // 读取期间文件可能被追加:多取 1 字节用于识别超限,而非无界增长。
    file.take(MAX_DIFF_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_DIFF_BYTES {
        return Ok(None);
    }
    Ok(String::from_utf8(bytes).ok())
}

#[cfg(unix)]
mod platform {
    use super::*;
    use rustix::fs::{self, AtFlags, Mode, OFlags};
    use std::ffi::OsString;
    use std::path::Component;

    struct Stage {
        parent: std::fs::File,
        dir: std::fs::File,
        name: String,
        temp: String,
        identity: Option<fs::Stat>,
    }

    impl Stage {
        fn new(parent: &std::fs::File) -> io::Result<Self> {
            let parent = parent.try_clone()?;
            let name = format!(".zhuye-stage-{}", uuid::Uuid::new_v4());
            fs::mkdirat(&parent, name.as_str(), Mode::from_raw_mode(0o700))?;
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
            let dir =
                std::fs::File::from(fs::openat(&parent, name.as_str(), flags, Mode::empty())?);
            let stage = Self {
                parent,
                dir,
                name,
                temp: format!(".zhuye-{}.tmp", uuid::Uuid::new_v4()),
                identity: None,
            };
            // 暂存目录不能继承允许其他用户访问的 ACL。
            #[cfg(target_os = "macos")]
            super::metadata::clear_inherited_acl(&stage.dir)?;
            #[cfg(any(target_os = "linux", target_os = "android"))]
            for attr in ["system.posix_acl_access", "system.posix_acl_default"] {
                match fs::fremovexattr(&stage.dir, attr) {
                    Ok(()) | Err(rustix::io::Errno::NODATA) | Err(rustix::io::Errno::OPNOTSUPP) => {
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            fs::fchmod(&stage.dir, Mode::from_raw_mode(0o700))?;
            Ok(stage)
        }

        fn verify_entry(&self, file: &std::fs::File) -> io::Result<()> {
            let opened = fs::fstat(file)?;
            let entry = fs::statat(&self.dir, self.temp.as_str(), AtFlags::SYMLINK_NOFOLLOW)?;
            if opened.st_dev != entry.st_dev || opened.st_ino != entry.st_ino {
                return Err(io::Error::other("temporary file was replaced"));
            }
            Ok(())
        }

        fn remember(&mut self, file: &std::fs::File) -> io::Result<()> {
            self.identity = Some(fs::fstat(file)?);
            Ok(())
        }
    }

    impl Drop for Stage {
        fn drop(&mut self) {
            if let (Some(opened), Ok(entry)) = (
                self.identity.as_ref(),
                fs::statat(&self.dir, self.temp.as_str(), AtFlags::SYMLINK_NOFOLLOW),
            ) && entry.st_dev == opened.st_dev
                && entry.st_ino == opened.st_ino
            {
                let _ = fs::unlinkat(&self.dir, self.temp.as_str(), AtFlags::empty());
            }
            // 若目录项已被替换，不能删除另一个目录。
            if let (Ok(opened), Ok(entry)) = (
                fs::fstat(&self.dir),
                fs::statat(&self.parent, self.name.as_str(), AtFlags::SYMLINK_NOFOLLOW),
            ) && opened.st_dev == entry.st_dev
                && opened.st_ino == entry.st_ino
            {
                let _ = fs::unlinkat(&self.parent, self.name.as_str(), AtFlags::REMOVEDIR);
            }
        }
    }

    pub(crate) struct PinnedWrite {
        dir: std::fs::File,
        name: OsString,
    }

    impl PinnedWrite {
        pub(crate) fn prepare(path: &Path) -> io::Result<Self> {
            let mut components = path.components();
            if components.next() != Some(Component::RootDir) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "write path must be absolute",
                ));
            }
            let parts: Vec<_> = components.collect();
            let (last, parents) = parts.split_last().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "write path has no file")
            })?;
            let Component::Normal(name) = last else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "write path has no file",
                ));
            };
            let flags = super::super::safe_file::directory_flags();
            let mut dir = std::fs::File::from(fs::open("/", flags, Mode::empty())?);
            for part in parents {
                let Component::Normal(name) = part else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "write path is not normalized",
                    ));
                };
                let child = match fs::openat(&dir, *name, flags, Mode::empty()) {
                    Ok(child) => child,
                    Err(rustix::io::Errno::NOENT) => {
                        match fs::mkdirat(&dir, *name, Mode::from_raw_mode(0o777)) {
                            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                            Err(error) => return Err(error.into()),
                        }
                        fs::openat(&dir, *name, flags, Mode::empty())?
                    }
                    Err(error) => return Err(error.into()),
                };
                dir = std::fs::File::from(child);
            }
            Ok(Self {
                dir,
                name: name.to_os_string(),
            })
        }

        /// `Ok(None)`:目标存在但不是可 diff 的文本；`Ok(Some(_))`:文本内容，
        /// 目标不存在时为空串。
        pub(crate) fn read_existing(&self) -> io::Result<Option<String>> {
            let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
            match fs::openat(&self.dir, &self.name, flags, Mode::empty()) {
                Ok(file) => read_text(std::fs::File::from(file)),
                Err(rustix::io::Errno::NOENT) => Ok(Some(String::new())),
                Err(error) => Err(error.into()),
            }
        }

        pub(crate) fn replace(&self, content: &[u8]) -> io::Result<()> {
            self.replace_with(|file| file.write_all(content))
        }

        pub(super) fn replace_with(
            &self,
            write: impl FnOnce(&mut std::fs::File) -> io::Result<()>,
        ) -> io::Result<()> {
            let read_flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
            let source = match fs::openat(&self.dir, &self.name, read_flags, Mode::empty()) {
                Ok(file) => {
                    let file = std::fs::File::from(file);
                    let metadata = file.metadata()?;
                    if !metadata.is_file() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "target is not a regular file",
                        ));
                    }
                    if metadata.permissions().readonly() {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "target is read-only",
                        ));
                    }
                    // 原子重命名只检查目录权限；同时检查目标自身的写权限。
                    let write_flags =
                        OFlags::WRONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
                    let _write_check =
                        fs::openat(&self.dir, &self.name, write_flags, Mode::empty())?;
                    Some(file)
                }
                Err(rustix::io::Errno::NOENT) => None,
                Err(error) => return Err(error.into()),
            };
            let mut stage = Stage::new(&self.dir)?;
            let temp = stage.temp.clone();
            let temp = temp.as_str();
            let flags =
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW;
            let mut file = std::fs::File::from(fs::openat(
                &self.dir,
                temp,
                flags,
                Mode::from_raw_mode(if source.is_some() { 0o600 } else { 0o666 }),
            )?);
            // 先在目标目录创建空文件以继承其新文件 ACL，再移入私有暂存目录。
            // 移动失败时该目录项仍在本目录，必须清掉，否则工作区留下垃圾文件。
            let moved = stage
                .remember(&file)
                .and_then(|()| fs::renameat(&self.dir, temp, &stage.dir, temp).map_err(Into::into));
            if let Err(error) = moved {
                let _ = fs::unlinkat(&self.dir, temp, AtFlags::empty());
                return Err(error);
            }
            stage.verify_entry(&file)?;
            (|| {
                if let Some(source) = &source {
                    let original = fs::fstat(source)?;
                    let replacement = fs::fstat(&file)?;
                    let owner = (original.st_uid != replacement.st_uid)
                        .then(|| fs::Uid::from_raw(original.st_uid));
                    let group = (original.st_gid != replacement.st_gid)
                        .then(|| fs::Gid::from_raw(original.st_gid));
                    if owner.is_some() || group.is_some() {
                        fs::fchown(&file, owner, group)?;
                    }
                }
                #[cfg(target_os = "macos")]
                if source.is_some() {
                    super::metadata::clear_inherited_acl(&file)?;
                }
                write(&mut file)?;
                if let Some(source) = &source {
                    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
                    super::metadata::copy_metadata(source, &file)?;
                    let original = fs::fstat(source)?;
                    fs::fchmod(&file, Mode::from_raw_mode(original.st_mode & 0o7777))?;
                }
                file.sync_all()?;
                // 确保被发布的目录项仍指向我们写入的句柄。
                stage.verify_entry(&file)?;
                if source.is_some() {
                    fs::renameat(&stage.dir, temp, &self.dir, &self.name)?;
                } else {
                    // 文件初次不存在时，不能覆盖同时出现的新文件。
                    #[cfg(any(
                        target_vendor = "apple",
                        target_os = "linux",
                        target_os = "android"
                    ))]
                    fs::renameat_with(
                        &stage.dir,
                        temp,
                        &self.dir,
                        &self.name,
                        fs::RenameFlags::NOREPLACE,
                    )?;
                    #[cfg(not(any(
                        target_vendor = "apple",
                        target_os = "linux",
                        target_os = "android"
                    )))]
                    {
                        fs::linkat(&stage.dir, temp, &self.dir, &self.name, AtFlags::empty())?;
                        fs::unlinkat(&stage.dir, temp, AtFlags::empty())?;
                    }
                }
                Ok(())
            })()
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::path::PathBuf;

    const SHARE_READ_WRITE: u32 = 0x1 | 0x2;
    const SHARE_DELETE: u32 = 0x4;
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const REPARSE_POINT: u32 = 0x400;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const WRITE_DAC: u32 = 0x0004_0000;
    const WRITE_OWNER: u32 = 0x0008_0000;
    const DELETE: u32 = 0x0001_0000;

    pub(crate) struct PinnedWrite {
        path: PathBuf,
        _directories: Vec<File>,
    }

    impl PinnedWrite {
        pub(crate) fn prepare(path: &Path) -> io::Result<Self> {
            if !path.is_absolute() || path.file_name().is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "write path must be absolute",
                ));
            }
            let parent = path.parent().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "write path has no parent")
            })?;
            let mut dirs = Vec::new();
            for ancestor in parent.ancestors().collect::<Vec<_>>().into_iter().rev() {
                let open_dir = || {
                    OpenOptions::new()
                        .read(true)
                        .share_mode(SHARE_READ_WRITE)
                        .custom_flags(BACKUP_SEMANTICS | OPEN_REPARSE_POINT)
                        .open(ancestor)
                };
                let dir = match open_dir() {
                    Ok(dir) => dir,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        match std::fs::create_dir(ancestor) {
                            Ok(()) => {}
                            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                            Err(error) => return Err(error),
                        }
                        open_dir()?
                    }
                    Err(error) => return Err(error),
                };
                let metadata = dir.metadata()?;
                if !metadata.is_dir() || metadata.file_attributes() & REPARSE_POINT != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "write directory is a reparse point",
                    ));
                }
                dirs.push(dir);
            }
            Ok(Self {
                path: path.to_path_buf(),
                _directories: dirs,
            })
        }

        pub(crate) fn read_existing(&self) -> io::Result<Option<String>> {
            match OpenOptions::new()
                .read(true)
                .share_mode(SHARE_READ_WRITE)
                // 目录目标也要能打开:Windows 打开目录必须带 BACKUP_SEMANTICS,
                // 才能像 Unix 那样走到 read_text 的「不是普通文件」判定,
                // 让上层报 InvalidArgs 而不是一句 Io 噪声。
                .custom_flags(OPEN_REPARSE_POINT | BACKUP_SEMANTICS)
                .open(&self.path)
            {
                Ok(file) => {
                    if file.metadata()?.file_attributes() & REPARSE_POINT != 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "target is a reparse point",
                        ));
                    }
                    read_text(file)
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Some(String::new())),
                Err(error) => Err(error),
            }
        }

        pub(crate) fn replace(&self, content: &[u8]) -> io::Result<()> {
            let step = |name: &'static str, e: io::Error| {
                io::Error::other(format!("safe_write.{name}: {e}"))
            };
            let source = match OpenOptions::new()
                .read(true)
                .write(true)
                .share_mode(SHARE_READ_WRITE | SHARE_DELETE)
                .custom_flags(OPEN_REPARSE_POINT)
                .open(&self.path)
            {
                Ok(file) => {
                    let metadata = file.metadata()?;
                    if metadata.file_attributes() & REPARSE_POINT != 0 || !metadata.is_file() {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "target is not a regular file",
                        ));
                    }
                    if metadata.permissions().readonly() {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "target is read-only",
                        ));
                    }
                    Some(file)
                }
                // NotFound 不能包装(下游按 kind 判新建/覆盖),其余错误带步骤标签。
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(step("open-source", error)),
            };
            let temp = self
                .path
                .parent()
                .unwrap()
                .join(format!(".zhuye-{}.tmp", uuid::Uuid::new_v4()));
            let mut options = OpenOptions::new();
            options
                .write(true)
                .create_new(true)
                .share_mode(SHARE_READ_WRITE)
                .custom_flags(OPEN_REPARSE_POINT);
            let access = GENERIC_WRITE
                | DELETE
                | if source.is_some() {
                    WRITE_DAC | WRITE_OWNER
                } else {
                    0
                };
            options.access_mode(access);
            // 每步带标签:Windows 排障时 87(code 87)可能出自 open/write/sync/
            // rename 任一步,裸的 os error 无法区分,只能一轮轮猜。
            // (step 闭包复用本函数开头的定义。)
            let mut file = options.open(&temp).map_err(|e| step("open-temp", e))?;
            // 目标是否存在决定改名是否允许覆盖(与 Unix 的 NOREPLACE 分支同义)。
            let existed = source.is_some();
            let result = (|| {
                if let Some(source) = &source {
                    super::metadata_windows::copy_dacl(source, &file)
                        .map_err(|e| step("copy-dacl", e))?;
                }
                file.write_all(content).map_err(|e| step("write", e))?;
                file.sync_all().map_err(|e| step("sync", e))?;
                // Windows 不允许替换「仍有打开句柄」的文件:实测旧目标无论以
                // 读写还是只读打开、无论是否共享删除,改名一律返回
                // STATUS_ACCESS_DENIED(0xC0000022);只有先关闭句柄才过得去。
                // Unix 可以边持有旧文件句柄边 rename(POSIX 语义),Windows 不行,
                // 故 DACL 拷贝完成后必须释放旧目标句柄再改名。
                // 目录根仍是已固定的父目录句柄,叶子名写死,路径组件中途被
                // 换成符号链接也改不到工作区之外。
                drop(source);
                let parent = self
                    ._directories
                    .last()
                    .ok_or_else(|| io::Error::other("missing pinned parent"))?;
                let name = self
                    .path
                    .file_name()
                    .ok_or_else(|| io::Error::other("missing file name"))?;
                super::metadata_windows::rename_file(&file, parent, name, existed)
                    .map_err(|e| step("rename", e))
            })();
            if let Err(error) = result {
                // 临时文件句柄始终不共享删除，失败时通过句柄删除，避免重新解析路径。
                if let Err(cleanup) = super::metadata_windows::delete_on_close(&file) {
                    return Err(io::Error::other(format!(
                        "{error}; temporary cleanup failed: {cleanup}"
                    )));
                }
                return Err(error);
            }
            Ok(())
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::*;
    pub(crate) struct PinnedWrite;
    impl PinnedWrite {
        pub(crate) fn prepare(_: &Path) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure writing is unavailable",
            ))
        }
        pub(crate) fn read_existing(&self) -> io::Result<Option<String>> {
            unreachable!()
        }
        pub(crate) fn replace(&self, _: &[u8]) -> io::Result<()> {
            unreachable!()
        }
    }
}

pub(crate) use platform::PinnedWrite;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn staged_file(root: &Path) -> io::Result<std::path::PathBuf> {
        let stage = std::fs::read_dir(root)?
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".zhuye-stage-")
            })
            .expect("temporary stage")
            .path();
        Ok(std::fs::read_dir(stage)?
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .expect("temporary file")
            .path())
    }

    #[test]
    fn pinned_directory_cannot_be_redirected_after_preparation() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join("dir")).unwrap();
        std::fs::write(workspace.path().join("dir/file.txt"), "inside").unwrap();
        std::fs::write(outside.path().join("file.txt"), "outside").unwrap();
        let path = workspace
            .path()
            .join("dir/file.txt")
            .canonicalize()
            .unwrap();
        let target = PinnedWrite::prepare(&path).unwrap();
        std::fs::rename(workspace.path().join("dir"), workspace.path().join("old")).unwrap();
        symlink(outside.path(), workspace.path().join("dir")).unwrap();
        assert!(PinnedWrite::prepare(&path).is_err());
        assert_eq!(target.read_existing().unwrap().as_deref(), Some("inside"));
        target.replace(b"updated").unwrap();
        assert_eq!(
            std::fs::read_to_string(outside.path().join("file.txt")).unwrap(),
            "outside"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("old/file.txt")).unwrap(),
            "updated"
        );
    }

    #[test]
    fn leaf_replaced_by_symlink_is_not_read_or_written() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = workspace.path().join("file.txt");
        let victim = outside.path().join("victim.txt");
        std::fs::write(&path, "inside").unwrap();
        std::fs::write(&victim, "outside").unwrap();
        let target = PinnedWrite::prepare(&path.canonicalize().unwrap()).unwrap();
        std::fs::remove_file(&path).unwrap();
        symlink(&victim, &path).unwrap();
        assert!(target.read_existing().is_err());
        assert!(target.replace(b"unsafe").is_err());
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "outside");
    }

    #[test]
    fn replacement_preserves_permissions_and_rejects_read_only_targets() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        std::fs::write(&path, "original").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let path = path.canonicalize().unwrap();
        let target = PinnedWrite::prepare(&path).unwrap();
        target.replace(b"updated").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        assert_eq!(
            target.replace(b"rejected").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "updated");
    }

    #[test]
    fn failed_write_leaves_original_and_cleans_temporary_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        std::fs::write(&path, "original").unwrap();
        let target = PinnedWrite::prepare(&path.canonicalize().unwrap()).unwrap();
        assert!(
            target
                .replace_with(|file| {
                    file.write_all(b"partial")?;
                    Err(io::Error::other("injected failure"))
                })
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "original");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn concurrent_creation_is_not_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap().join("file.txt");
        let target = PinnedWrite::prepare(&path).unwrap();
        assert!(
            target
                .replace_with(|file| {
                    file.write_all(b"ours")?;
                    std::fs::write(&path, "concurrent")?;
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "concurrent");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn replaced_temporary_entry_cannot_be_published() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        let victim = outside.path().join("victim.txt");
        std::fs::write(&path, "original").unwrap();
        std::fs::write(&victim, "outside").unwrap();
        let target = PinnedWrite::prepare(&path.canonicalize().unwrap()).unwrap();
        let result = target.replace_with(|file| {
            file.write_all(b"ours")?;
            let temp = staged_file(root.path())?;
            std::fs::remove_file(&temp)?;
            symlink(&victim, &temp)?;
            Ok(())
        });
        assert!(result.is_err(), "replaced temporary entry was published");
        assert!(
            !std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "original");
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "outside");
    }

    #[test]
    fn cleanup_does_not_remove_a_replaced_temporary_entry() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        std::fs::write(&path, "original").unwrap();
        let target = PinnedWrite::prepare(&path.canonicalize().unwrap()).unwrap();
        let mut replacement = None;
        assert!(
            target
                .replace_with(|file| {
                    file.write_all(b"ours")?;
                    let temp = staged_file(root.path())?;
                    std::fs::remove_file(&temp)?;
                    std::fs::write(&temp, "unrelated")?;
                    replacement = Some(temp);
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "original");
        assert_eq!(
            std::fs::read_to_string(replacement.unwrap()).unwrap(),
            "unrelated"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn replacement_preserves_acl_and_extended_attributes() {
        use std::process::Command;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        std::fs::write(&path, "original").unwrap();
        let user = Command::new("id").arg("-un").output().unwrap();
        let user = String::from_utf8(user.stdout).unwrap().trim().to_string();
        assert!(
            Command::new("chmod")
                .arg("+a")
                .arg(format!("{user} allow read"))
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("xattr")
                .args(["-w", "com.zhuye.test", "sentinel"])
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let before = Command::new("ls").arg("-le").arg(&path).output().unwrap();
        let target = PinnedWrite::prepare(&path.canonicalize().unwrap()).unwrap();
        target.replace(b"updated").unwrap();
        let after = Command::new("ls").arg("-le").arg(&path).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&before.stdout)
                .lines()
                .skip(1)
                .collect::<Vec<_>>(),
            String::from_utf8_lossy(&after.stdout)
                .lines()
                .skip(1)
                .collect::<Vec<_>>()
        );
        let xattr = Command::new("xattr")
            .args(["-p", "com.zhuye.test"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(xattr.status.success());
        assert_eq!(String::from_utf8_lossy(&xattr.stdout).trim(), "sentinel");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn temporary_file_does_not_inherit_extra_acl() {
        use std::process::Command;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        std::fs::write(&path, "private").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            Command::new("chmod")
                .args(["+a", "everyone allow read,file_inherit"])
                .arg(root.path())
                .status()
                .unwrap()
                .success()
        );
        let target = PinnedWrite::prepare(&path.canonicalize().unwrap()).unwrap();
        target
            .replace_with(|file| {
                let temp = staged_file(root.path())?;
                let acl = Command::new("ls").arg("-le").arg(temp).output()?;
                assert!(acl.status.success());
                assert_eq!(String::from_utf8_lossy(&acl.stdout).lines().count(), 1);
                file.write_all(b"updated")
            })
            .unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "updated");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn new_file_retains_parent_inherited_acl() {
        use std::process::Command;
        let root = tempfile::tempdir().unwrap();
        assert!(
            Command::new("chmod")
                .args(["+a", "everyone allow read,file_inherit"])
                .arg(root.path())
                .status()
                .unwrap()
                .success()
        );
        let control = root.path().join("control.txt");
        std::fs::write(&control, "control").unwrap();
        let path = root.path().canonicalize().unwrap().join("new.txt");
        PinnedWrite::prepare(&path)
            .unwrap()
            .replace(b"created")
            .unwrap();
        let control_acl = Command::new("ls").arg("-le").arg(control).output().unwrap();
        let created_acl = Command::new("ls").arg("-le").arg(path).output().unwrap();
        assert!(control_acl.status.success() && created_acl.status.success());
        assert_eq!(
            String::from_utf8_lossy(&control_acl.stdout)
                .lines()
                .skip(1)
                .collect::<Vec<_>>(),
            String::from_utf8_lossy(&created_acl.stdout)
                .lines()
                .skip(1)
                .collect::<Vec<_>>()
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn overwrite_and_read_only_target() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        std::fs::write(&path, "original").unwrap();
        let path = path.canonicalize().unwrap();
        let target = PinnedWrite::prepare(&path).unwrap();
        target.replace(b"updated").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "updated");
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions).unwrap();
        assert!(target.replace(b"rejected").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "updated");
        // 测试清理只是恢复默认可写;Clippy 1.100 在 Windows 上拒绝
        // `set_readonly(false)`,收尾临时目录由 RAII 删除,无需恢复。
    }
}
