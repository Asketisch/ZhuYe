//! 用目录句柄和不跟随符号链接的打开方式访问 KMS 页面。

use rustix::fd::OwnedFd;
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

pub(super) fn open_root(root: &Path) -> io::Result<OwnedFd> {
    let dir_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    Ok(fs::open(root, dir_flags, Mode::empty())?)
}

pub(super) fn open_dir(parent: &OwnedFd, name: &str) -> io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    Ok(fs::openat(parent, name, flags, Mode::empty())?)
}

fn open_pages(root: &Path, wiki: &str, create: bool) -> io::Result<OwnedFd> {
    let root = open_root(root)?;
    let wiki = open_dir(&root, wiki)?;
    if create {
        match fs::mkdirat(&wiki, "pages", Mode::from_raw_mode(0o777)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
    }
    open_dir(&wiki, "pages")
}

pub(super) fn modified_ms(file: &File) -> u64 {
    file.metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_millis() as u64)
}

pub(super) fn read_file_at(parent: &OwnedFd, name: &str) -> io::Result<(String, u64)> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let fd = fs::openat(parent, name, flags, Mode::empty())?;
    let mut file = File::from(fd);
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "KMS page is not a regular file",
        ));
    }
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    Ok((content, modified_ms(&file)))
}

pub(super) fn read(root: &Path, wiki: &str, page: &str) -> io::Result<(String, u64)> {
    let pages = open_pages(root, wiki, false)?;
    read_file_at(&pages, &format!("{page}.md"))
}

#[cfg(target_os = "macos")]
fn clear_inherited_acl(file: &File) -> io::Result<()> {
    use std::ffi::{c_int, c_void};
    use std::os::fd::AsRawFd;

    unsafe extern "C" {
        fn acl_init(count: c_int) -> *mut c_void;
        fn acl_set_fd(fd: c_int, acl: *mut c_void) -> c_int;
        fn acl_free(acl: *mut c_void) -> c_int;
    }

    // SAFETY: acl_init 创建的 ACL 在调用结束前有效，并且恰好释放一次。
    unsafe {
        let acl = acl_init(0);
        if acl.is_null() {
            return Err(io::Error::last_os_error());
        }
        let result = if acl_set_fd(file.as_raw_fd(), acl) == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        };
        acl_free(acl);
        result
    }
}

#[cfg(target_os = "macos")]
fn copy_metadata(source: &File, destination: &File) -> io::Result<()> {
    clear_inherited_acl(destination)?;
    let state = fs::copyfile_state_alloc()?;
    // SAFETY: state 由 copyfile_state_alloc 分配，调用期间保持有效且仅释放一次。
    let copied = unsafe {
        fs::fcopyfile(
            source,
            destination,
            state,
            fs::CopyfileFlags::ACL | fs::CopyfileFlags::XATTR,
        )
    };
    // SAFETY: state 此前已成功分配且尚未释放。
    let freed = unsafe { fs::copyfile_state_free(state) };
    copied?;
    freed?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn copy_metadata(source: &File, destination: &File) -> io::Result<()> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    fn read_buffer(
        mut read: impl FnMut(&mut [u8]) -> rustix::io::Result<usize>,
    ) -> io::Result<Vec<u8>> {
        let mut buffer = vec![0; 4096];
        loop {
            match read(&mut buffer) {
                Ok(length) => {
                    buffer.truncate(length);
                    return Ok(buffer);
                }
                Err(rustix::io::Errno::RANGE) if buffer.len() < 1024 * 1024 => {
                    buffer.resize(buffer.len() * 2, 0)
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    let names = match read_buffer(|buffer| fs::flistxattr(source, buffer)) {
        Ok(names) => names,
        // 同目录临时文件位于同一文件系统；不支持 xattr 时没有可复制的属性。
        Err(error) if error.raw_os_error() == Some(rustix::io::Errno::OPNOTSUPP.raw_os_error()) => {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    // 临时文件可能继承默认 ACL；原文件没有该 ACL 时不能意外引入额外权限。
    match fs::fremovexattr(destination, "system.posix_acl_access") {
        Ok(()) | Err(rustix::io::Errno::NODATA) | Err(rustix::io::Errno::OPNOTSUPP) => {}
        Err(error) => return Err(error.into()),
    }
    for name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let name = OsStr::from_bytes(name);
        let value = read_buffer(|buffer| fs::fgetxattr(source, name, buffer))?;
        fs::fsetxattr(destination, name, &value, fs::XattrFlags::empty())?;
    }
    Ok(())
}

fn write_to_dir(
    pages: &OwnedFd,
    page: &str,
    create_only: bool,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<u64> {
    let target = format!("{page}.md");
    let mode = match fs::statat(pages, target.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) if create_only => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "KMS page already exists",
            ));
        }
        Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => {
            Some(Mode::from_raw_mode(stat.st_mode & 0o7777))
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "KMS page is not a regular file",
            ));
        }
        Err(rustix::io::Errno::NOENT) => None,
        Err(error) => return Err(error.into()),
    };
    let source = if mode.is_some() {
        // 重命名只检查目录权限，因此须先检查现有文件自身的写权限。
        let flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        let file = File::from(fs::openat(pages, target.as_str(), flags, Mode::empty())?);
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "KMS page is not a regular file",
            ));
        }
        if metadata.permissions().readonly() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "KMS page is read-only",
            ));
        }
        Some(file)
    } else {
        None
    };
    let mode = match &source {
        Some(file) => Some(Mode::from_raw_mode(fs::fstat(file)?.st_mode & 0o7777)),
        None => None,
    };
    let temp = format!(".kms-{}.tmp", uuid::Uuid::new_v4());
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut file = File::from(fs::openat(
        pages,
        temp.as_str(),
        flags,
        Mode::from_raw_mode(if source.is_some() { 0o600 } else { 0o666 }),
    )?);
    let result = (|| {
        if let Some(source) = &source {
            let original = fs::fstat(source)?;
            let replacement = fs::fstat(&file)?;
            let owner =
                (original.st_uid != replacement.st_uid).then(|| fs::Uid::from_raw(original.st_uid));
            let group =
                (original.st_gid != replacement.st_gid).then(|| fs::Gid::from_raw(original.st_gid));
            if owner.is_some() || group.is_some() {
                fs::fchown(&file, owner, group)?;
            }
        }
        #[cfg(target_os = "macos")]
        if let Some(source) = &source {
            copy_metadata(source, &file)?;
        }
        write(&mut file)?;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if let Some(source) = &source {
            copy_metadata(source, &file)?;
        }
        if let Some(mode) = mode {
            fs::fchmod(&file, mode)?;
        }
        file.sync_all()?;
        if create_only {
            fs::linkat(
                pages,
                temp.as_str(),
                pages,
                target.as_str(),
                AtFlags::empty(),
            )?;
        } else {
            fs::renameat(pages, temp.as_str(), pages, target.as_str())?;
        }
        Ok(modified_ms(&file))
    })();
    let _ = fs::unlinkat(pages, temp.as_str(), AtFlags::empty());
    result
}

pub(super) fn write(
    root: &Path,
    wiki: &str,
    page: &str,
    content: &[u8],
    create_only: bool,
) -> io::Result<u64> {
    let pages = open_pages(root, wiki, true)?;
    write_to_dir(&pages, page, create_only, |file| file.write_all(content))
}

#[cfg(test)]
mod tests;
