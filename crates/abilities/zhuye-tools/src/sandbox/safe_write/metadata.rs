//! 保存替换文件的 ACL 和扩展属性。

use rustix::fs;
use std::fs::File;
use std::io;

#[cfg(target_os = "macos")]
pub(super) fn clear_inherited_acl(file: &File) -> io::Result<()> {
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
pub(super) fn copy_metadata(source: &File, destination: &File) -> io::Result<()> {
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
pub(super) fn copy_metadata(source: &File, destination: &File) -> io::Result<()> {
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
