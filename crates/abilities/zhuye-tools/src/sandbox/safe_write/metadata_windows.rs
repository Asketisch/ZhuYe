//! 保留 Windows 文件的 DACL，并在写入临时文件前应用。

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::ptr;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, GetSecurityDescriptorControl,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
    UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_DISPOSITION_INFO, FILE_RENAME_INFO, FileDispositionInfo, FileRenameInfoEx,
    SetFileInformationByHandle,
};

struct Descriptor(*mut std::ffi::c_void);

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: 指针由 GetSecurityInfo 分配，生命周期内仅在此释放一次。
        unsafe {
            LocalFree(self.0);
        }
    }
}

pub(super) fn copy_dacl(source: &File, destination: &File) -> io::Result<()> {
    let mut dacl = ptr::null_mut();
    let mut owner = ptr::null_mut();
    let mut group = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    // SAFETY: 文件句柄有效，输出指针指向本函数内的变量，返回的缓冲区由 LocalFree 释放。
    let status = unsafe {
        GetSecurityInfo(
            source.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION,
            &mut owner,
            &mut group,
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let descriptor = Descriptor(descriptor);
    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: descriptor 由成功的 GetSecurityInfo 返回并由守卫持有。
    if unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let inheritance = if control & SE_DACL_PROTECTED != 0 {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };
    // SAFETY: DACL 指针由 descriptor 持有，目标句柄在调用期间有效。
    let status = unsafe {
        SetSecurityInfo(
            destination.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION
                | OWNER_SECURITY_INFORMATION
                | GROUP_SECURITY_INFORMATION
                | inheritance,
            owner,
            group,
            dacl,
            ptr::null(),
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    Ok(())
}

pub(super) fn rename_file(
    file: &File,
    parent: &File,
    name: &OsStr,
    replace: bool,
) -> io::Result<()> {
    let name: Vec<u16> = name.encode_wide().collect();
    let bytes = name
        .len()
        .checked_mul(2)
        .and_then(|len| u32::try_from(len).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file name is too long"))?;
    let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let size = offset
        .checked_add(bytes as usize + 2)
        .map(|size| size.max(std::mem::size_of::<FILE_RENAME_INFO>()))
        .and_then(|size| u32::try_from(size).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file name is too long"))?;
    // usize 对齐足以容纳 FILE_RENAME_INFO 的句柄和尾随 UTF-16 字符串。
    let mut storage = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: storage 已按结构体对齐并分配足够空间，句柄及 UTF-16 数据在调用期间有效。
    //
    // 必须用 FileRenameInformationEx(类 22)而非 FileRenameInformation(类 3):
    // 非 Ex 类**不接受非 NULL 的 RootDirectory** —— 传句柄根直接返回
    // ERROR_INVALID_PARAMETER(code 87),这是 write/edit/plan_write 全部
    // 落盘在 Windows 上恒红的根因。Ex 类(Windows 10 1607+)才支持
    // 「RootDirectory 句柄 + 相对文件名」的目录句柄根改名,即本模块依赖的
    // 不重新解析路径的安全不变量。union 按 std 同款口径写 32 位 Flags 域,
    // 替换语义走 FILE_RENAME_FLAG_REPLACE_IF_EXISTS 位(windows-sys 未导出
    // 该常量,本地定义;不加 POSIX_SEMANTICS,保持 NTFS 式大小写不敏感替换,
    // 与 Unix 路径 renameat 的行为对齐)。
    const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 0x0000_0001;
    let result = unsafe {
        (*info).Anonymous.Flags = if replace { FILE_RENAME_FLAG_REPLACE_IF_EXISTS } else { 0 };
        (*info).RootDirectory = parent.as_raw_handle();
        (*info).FileNameLength = bytes;
        ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
        SetFileInformationByHandle(file.as_raw_handle(), FileRenameInfoEx, info.cast(), size)
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(super) fn delete_on_close(file: &File) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: info 的布局与指定类别匹配，文件句柄在调用期间有效。
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
