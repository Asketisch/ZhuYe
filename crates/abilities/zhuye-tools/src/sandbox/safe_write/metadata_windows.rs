//! 保留 Windows 文件的 DACL，并在写入临时文件前应用。

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::ptr;
use windows_sys::Win32::Foundation::{HANDLE, LocalFree, NTSTATUS, RtlNtStatusToDosError};
use windows_sys::Win32::Security::Authorization::{
    GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, GetSecurityDescriptorControl,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
    UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_RENAME_INFORMATION, FileRenameInformation, NtSetInformationFile,
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

/// 用目录句柄做「句柄根相对改名」。
///
/// 必须走 NT 层 `NtSetInformationFile`(FileRenameInformation,类 10)。
/// Win32 的 `SetFileInformationByHandle` 封装层**整体拒绝非 NULL 的
/// `RootDirectory`**:实测 class 3(FileRenameInfo)与 class 22
/// (FileRenameInfoEx)带目录句柄一律返回 ERROR_INVALID_PARAMETER(code 87),
/// 与 Flags 怎么填无关 —— 上一版把锅甩给「Ex 类与非 Ex 类的差别」是误判,
/// 差别其实在封装层本身。NT 层同名结构体布局一致,却接受句柄根,这是本模块
/// 「不重新解析路径」安全不变量在 Windows 上的落点:改名只认已固定的父目录
/// 句柄 + 叶子名,路径组件中途被换成符号链接也改不到工作区之外。
///
/// `replace` 决定目标已存在时是否覆盖(class 10 的 `ReplaceIfExists`)。
/// 目标不存在且 `replace=false` 时不覆盖后来出现的同名文件,与 Unix 的
/// `renameat` + `NOREPLACE` 分支同义。
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
    let offset = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
    let size = offset
        .checked_add(bytes as usize + 2)
        .map(|size| size.max(std::mem::size_of::<FILE_RENAME_INFORMATION>()))
        .and_then(|size| u32::try_from(size).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file name is too long"))?;
    // usize 对齐足以容纳 FILE_RENAME_INFORMATION 的句柄和尾随 UTF-16 字符串。
    let mut storage = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    // SAFETY: storage 已按结构体对齐并分配足够空间，句柄及 UTF-16 数据在调用期间有效。
    unsafe {
        (*info).Anonymous.ReplaceIfExists = replace;
        (*info).RootDirectory = parent.as_raw_handle() as HANDLE;
        (*info).FileNameLength = bytes;
        ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
        let mut status_block: IO_STATUS_BLOCK = std::mem::zeroed();
        let status = NtSetInformationFile(
            file.as_raw_handle() as HANDLE,
            &mut status_block,
            info.cast_const().cast(),
            size,
            FileRenameInformation,
        );
        if status != 0 {
            return Err(nt_status_error(status));
        }
    }
    Ok(())
}

/// NTSTATUS → Win32 错误码 → `io::Error`,与 Windows 其余路径保持一致的
/// `ErrorKind` 口径(下游按 kind 区分新建/覆盖/权限)。
fn nt_status_error(status: NTSTATUS) -> io::Error {
    // SAFETY: RtlNtStatusToDosError 只做状态码换算,不触碰调用方指针。
    let code = unsafe { RtlNtStatusToDosError(status) };
    io::Error::from_raw_os_error(code as i32)
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
