//! Linux 子进程断网过滤器。
//!
//! 安装前把继承的非标准文件描述符设为 exec 时关闭；过滤器阻止创建或
//! 使用网络套接字，并封堵 io_uring、进程注入和 TUN 设备路径。

use std::io;

const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const TUNSETIFF: u32 = 0x4004_54ca;

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

/// 在 fork 前构造程序，避免子进程在过滤器构造时分配内存。
pub(super) fn build() -> Vec<libc::sock_filter> {
    let architecture = if cfg!(target_arch = "x86_64") {
        AUDIT_ARCH_X86_64
    } else if cfg!(target_arch = "aarch64") {
        AUDIT_ARCH_AARCH64
    } else {
        // 不认识的平台无法证明系统调用号，拒绝启动。
        return Vec::new();
    };
    let load = libc::BPF_LD as u16 | libc::BPF_W as u16 | libc::BPF_ABS as u16;
    let eq = libc::BPF_JMP as u16 | libc::BPF_JEQ as u16 | libc::BPF_K as u16;
    let ret = libc::BPF_RET as u16 | libc::BPF_K as u16;
    let deny = SECCOMP_RET_ERRNO | libc::EPERM as u32;
    let mut program = vec![
        stmt(load, 4),
        jump(eq, architecture, 1, 0),
        stmt(ret, SECCOMP_RET_KILL_PROCESS),
        stmt(load, 0),
    ];
    if cfg!(target_arch = "x86_64") {
        // x32 ABI 使用相同架构标识，但系统调用号带有高位。
        program.push(jump(
            libc::BPF_JMP as u16 | libc::BPF_JGE as u16 | libc::BPF_K as u16,
            0x4000_0000,
            0,
            1,
        ));
        program.push(stmt(ret, deny));
    }
    // TUN 设备可通过 ioctl 创建网络接口，不依赖 socket 系统调用。
    program.extend([
        jump(eq, libc::SYS_ioctl as u32, 0, 3),
        stmt(load, 24),
        jump(eq, TUNSETIFF, 0, 1),
        stmt(ret, deny),
        stmt(load, 0),
    ]);
    for syscall in [
        libc::SYS_socket,
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_sendto,
        libc::SYS_sendmsg,
        libc::SYS_sendmmsg,
        libc::SYS_recvmsg,
        libc::SYS_recvmmsg,
        libc::SYS_io_uring_setup,
        libc::SYS_pidfd_getfd,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
    ] {
        program.push(jump(eq, syscall as u32, 0, 1));
        program.push(stmt(ret, deny));
    }
    program.push(stmt(ret, SECCOMP_RET_ALLOW));
    program
}

/// 只在 fork 后、exec 前调用；失败必须阻止命令启动。
pub(super) fn install(program: &[libc::sock_filter]) -> io::Result<()> {
    if program.is_empty() {
        return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP));
    }
    // CLOSE_RANGE_CLOEXEC 不直接关闭 Rust 进程启动时用于报告错误的管道。
    const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
    let result =
        unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, CLOSE_RANGE_CLOEXEC) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let filter = libc::sock_fprog {
        len: program.len() as u16,
        filter: program.as_ptr().cast_mut(),
    };
    if unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &filter) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn linux_child_enforces_network_policy_and_preserves_local_processes() {
        const CHILD: &str = "ZHUYE_NETWORK_FILTER_TEST_CHILD";
        if let Ok(mode) = std::env::var(CHILD) {
            let addr = std::env::var("ZHUYE_NETWORK_FILTER_TEST_ADDRESS").unwrap();
            let result = std::net::TcpStream::connect(addr);
            if mode == "deny" {
                assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EPERM));
                assert_eq!(
                    std::net::UdpSocket::bind("127.0.0.1:0")
                        .unwrap_err()
                        .raw_os_error(),
                    Some(libc::EPERM)
                );
                assert_eq!(
                    std::os::unix::net::UnixStream::connect("/tmp/nonexistent-socket")
                        .unwrap_err()
                        .raw_os_error(),
                    Some(libc::EPERM)
                );
                assert_eq!(
                    unsafe {
                        libc::syscall(libc::SYS_io_uring_setup, 1, std::ptr::null_mut::<u8>())
                    },
                    -1
                );
                assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
                let inherited_fd: i32 = std::env::var("ZHUYE_NETWORK_FILTER_TEST_FD")
                    .unwrap()
                    .parse()
                    .unwrap();
                assert_eq!(unsafe { libc::fcntl(inherited_fd, libc::F_GETFD) }, -1);
                assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
                assert!(
                    std::process::Command::new("/bin/true")
                        .status()
                        .unwrap()
                        .success()
                );
            } else {
                assert!(result.is_ok());
            }
            return;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        use std::os::fd::AsRawFd;
        // 模拟宿主泄露未设置 CLOEXEC 的套接字，子进程不能继承它。
        let inherited_fd = unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_DUPFD, 100) };
        assert!(inherited_fd >= 100);
        let workspace = tempfile::tempdir().unwrap();
        for mode in ["deny", "allow"] {
            let mut sandbox = super::super::OsSandbox::enabled(Vec::new());
            sandbox.allow_network = mode == "allow";
            let mut setup = sandbox.landlock_pre_exec(workspace.path().to_owned());
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child.arg("--exact").arg("os_sandbox::network_filter::tests::linux_child_enforces_network_policy_and_preserves_local_processes")
                .arg("--nocapture").env(CHILD, mode)
                .env("ZHUYE_NETWORK_FILTER_TEST_ADDRESS", listener.local_addr().unwrap().to_string())
                .env("ZHUYE_NETWORK_FILTER_TEST_FD", inherited_fd.to_string());
            unsafe {
                child.pre_exec(move || setup());
            }
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "mode={mode}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert_eq!(unsafe { libc::close(inherited_fd) }, 0);
    }

    #[test]
    fn invalid_filter_fails_before_command_runs() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("must-not-exist");
        let mut child = std::process::Command::new("/bin/sh");
        child.arg("-c").arg("touch \"$1\"").arg("sh").arg(&marker);
        unsafe {
            child.pre_exec(|| install(&[]));
        }
        assert!(child.spawn().is_err());
        assert!(!marker.exists());
    }

    #[test]
    fn landlock_blocks_direct_truncate_outside_workspace() {
        use std::os::unix::ffi::OsStrExt;
        const CHILD: &str = "ZHUYE_LANDLOCK_TRUNCATE_TEST_CHILD";
        if let Some(path) = std::env::var_os(CHILD) {
            let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            let result = unsafe { libc::truncate(path.as_ptr(), 0) };
            assert_eq!(result, -1, "敏感路径截断必须被拒绝");
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::EACCES)
            );
            let inside = std::env::var_os("ZHUYE_LANDLOCK_TRUNCATE_TEST_INSIDE").unwrap();
            let inside = std::ffi::CString::new(inside.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::truncate(inside.as_ptr(), 0) }, 0);
            assert!(
                std::fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/null")
                    .is_ok()
            );
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("protected.txt");
        let inside = workspace.path().join("editable.txt");
        std::fs::write(&file, "原始内容").unwrap();
        std::fs::write(&inside, "可截断").unwrap();
        let mut sandbox = super::super::OsSandbox::enabled(Vec::new());
        sandbox.allow_network = true;
        let mut setup = sandbox.landlock_pre_exec(workspace.path().to_owned());
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child.arg("--exact").arg("os_sandbox::network_filter::tests::landlock_blocks_direct_truncate_outside_workspace")
            .arg("--nocapture").env(CHILD, &file)
            .env("ZHUYE_LANDLOCK_TRUNCATE_TEST_INSIDE", &inside);
        unsafe {
            child.pre_exec(move || setup());
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read_to_string(file).unwrap(), "原始内容");
        assert_eq!(std::fs::metadata(inside).unwrap().len(), 0);
    }

    #[test]
    fn landlock_missing_writable_root_fails_before_command_runs() {
        use std::os::unix::process::CommandExt;
        let workspace = tempfile::tempdir().unwrap();
        let marker = workspace.path().join("must-not-exist");
        for strict in [false, true] {
            for allow_network in [false, true] {
                let mut sandbox =
                    super::super::OsSandbox::enabled(vec![workspace.path().join("missing")]);
                sandbox.strict = strict;
                sandbox.allow_network = allow_network;
                let mut setup = sandbox.landlock_pre_exec(workspace.path().to_owned());
                let mut child = std::process::Command::new("/bin/sh");
                child.arg("-c").arg("touch \"$1\"").arg("sh").arg(&marker);
                unsafe {
                    child.pre_exec(move || setup());
                }
                assert!(
                    child.spawn().is_err(),
                    "strict={strict} allow_network={allow_network}"
                );
                assert!(!marker.exists());
            }
        }
    }

    #[test]
    fn unavailable_landlock_rejects_non_strict_network_enabled_sandbox() {
        if super::super::landlock_kernel_supported() {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let marker = workspace.path().join("must-not-run");
        let mut sandbox = super::super::OsSandbox::enabled(Vec::new());
        sandbox.strict = false;
        sandbox.allow_network = true;
        assert!(sandbox.enforce_or_fail().is_err());
        let mut setup = sandbox.landlock_pre_exec(workspace.path().to_owned());
        let mut child = std::process::Command::new("/bin/sh");
        child.arg("-c").arg("touch \"$1\"").arg("sh").arg(&marker);
        unsafe {
            child.pre_exec(move || setup());
        }
        assert!(child.spawn().is_err());
        assert!(!marker.exists());
    }

    #[test]
    fn landlock_blocks_write_through_symlink_outside_workspace() {
        use std::os::unix::process::CommandExt;
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let protected = outside.path().join("protected");
        std::fs::write(&protected, "original").unwrap();
        std::os::unix::fs::symlink(&protected, workspace.path().join("link")).unwrap();
        let sandbox = super::super::OsSandbox::enabled(Vec::new());
        let mut setup = sandbox.landlock_pre_exec(workspace.path().to_owned());
        let mut child = std::process::Command::new("/bin/sh");
        child
            .arg("-c")
            .arg("printf changed > \"$1\"")
            .arg("sh")
            .arg(workspace.path().join("link"));
        unsafe {
            child.pre_exec(move || setup());
        }
        assert!(!child.status().unwrap().success());
        assert_eq!(std::fs::read_to_string(protected).unwrap(), "original");
    }
}
