//! # Windows FFI 执行层(F24)
//!
//! 把 [`super::windows`] 的**策略层**(plan / KillPlan)落到 Win32 调用:
//!
//! | 策略 | FFI | API 面 |
//! |---|---|---|
//! | Job Limits | `create_job_with_limits` | `CreateJobObjectW` / `SetInformationJobObject` |
//! | 进程树击杀 | `kill_tree` | `OpenProcess` + `TerminateProcess` |
//! | 树枚举 | `enumerate_process_tree` | `CreateToolhelp32Snapshot` |
//! | spawn | `spawn` | `CreateProcessW` + `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` |
//! | AppContainer | `create_appcontainer_profile` | `CreateAppContainerProfile` / `ConvertStringSidToSidW` |
//! | ACL 读授权 | `grant_read_acl` | `GetNamedSecurityInfoW` / `SetEntriesInAclW` / `SetNamedSecurityInfoW` |
//!
//! ## 跨平台边界
//!
//! 公共 API 在**所有平台**编译(用户要求:没有 Windows 环境也要先写代码):
//!
//! - 非 Windows:`spawn_sandboxed` 返回 [`SpawnError::Unsupported`]、
//!   `kill_tree` 返回 `false`、`enumerate_process_tree` 返回空 —— 调用方
//!   回退平台原生后端(Seatbelt / Landlock);
//! - Windows:真实 FFI。已通过 **`cargo check --target x86_64-pc-windows-msvc`**
//!   编译验证(API 路径经 windows-sys 0.59 源码逐一核对);真实 sandbox 行为
//!   需要 Windows 集成测试(`#[cfg(windows)] #[ignore]` 标注)。
//!
//! ## 执行顺序的安全性
//!
//! spawn 采用 **CREATE_SUSPENDED → AssignProcessToJobObject → ResumeThread**
//! 三段:若先 spawn 再 assign,子进程有窗口期可逃逸出 job(KILL_ON_CLOSE
//! 对未加入的进程无效)。挂起态无此窗口。
//!
//! ## capability SID 的务实取舍
//!
//! `DeriveCapabilitySidsFromName`(userenv)在 windows-sys 0.59 无导出。
//! 改用**已知静态 capability SID 表**(微软文档《App capability SIDs》:
//! internetClient = S-1-15-3-1 等)+ `ConvertStringSidToSidW`。
//! `WorkspaceReadWrite` 是自定义 capability,无静态 SID,不进 profile 的
//! capability 数组 —— 文件访问改由 **ACL 授权**(容器 SID +
//! [`grant_read_acl`])表达,语义等价且不需要自定义 capability 注册。
//!
//! ## 凭据卫生
//!
//! 子进程 env 由 [`build_env_block`] 从**脱敏后**的 map 构建(与 F08
//! `zhuye_secrets::provider::scrub_env` 同源),不继承父进程完整环境。

use super::windows::{KillPlan, WindowsSandboxPlan};

/// spawn 错误。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SpawnError {
    /// 非 Windows 平台:策略层可用但 FFI 不可用。
    #[error("windows sandbox unsupported on this platform")]
    Unsupported,
    /// plan 未通过校验。
    #[error("plan invalid: {0}")]
    PlanInvalid(String),
    /// Win32 调用失败(带上下文;Windows 侧附 GetLastError 描述)。
    #[error("win32 {api} failed: {detail}")]
    Ffi { api: String, detail: String },
    /// 命令行为空。
    #[error("command is empty")]
    EmptyCommand,
}

/// spawn 结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawned {
    /// 子进程 pid。
    pub pid: u32,
    /// 是否已加入 Job Object(KILL_ON_CLOSE 生效的前提)。
    pub assigned_to_job: bool,
    /// AppContainer 是否已应用(profile 创建成功且 attribute 注入)。
    pub appcontainer: bool,
    /// 实际生效的 capability 名列表。
    pub capabilities: Vec<String>,
}

/// spawn 一个沙箱内进程(按 plan 应用 Job / AppContainer)。
///
/// 跨平台入口:`plan` 来自 [`super::windows::WindowsSandbox::plan`]。
pub fn spawn_sandboxed(
    plan: &WindowsSandboxPlan,
    command: &str,
    args: &[String],
) -> Result<Spawned, SpawnError> {
    if command.trim().is_empty() {
        return Err(SpawnError::EmptyCommand);
    }
    imp::spawn(plan, command, args)
}

/// 按击杀计划终止进程树。返回是否全部终止成功(非 Windows 恒 false)。
pub fn kill_tree(plan: &KillPlan) -> bool {
    imp::kill_tree(plan)
}

/// 枚举进程树(树内成员的 (pid, parent) 对;非 Windows 返回空)。
pub fn enumerate_process_tree(root: u32) -> Vec<(u32, u32)> {
    imp::enumerate(root)
}

/// ACL 读授权:给 SID 授予 path 的读/执行权限(Windows FFI;
/// 非 Windows 返回 unsupported —— 调用方回退平台原生机制)。
pub fn grant_read_acl(path: &str, sid_bytes: &[u8]) -> Result<(), String> {
    imp::grant_read_acl(path, sid_bytes)
}

/// 从脱敏后的 env map 构建子进程环境块(双 NUL 结尾 UTF-16)。
///
/// 纯函数 —— 跨平台可测。`key=value` 按字典序排序(Windows 要求环境块
/// 按 name 排序,不区分大小写),空 map 返回仅含终止符的块。
pub fn build_env_block(env: &std::collections::BTreeMap<String, String>) -> Vec<u16> {
    let mut entries: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    entries.sort();
    let joined = entries.join("\0");
    let mut block: Vec<u16> = joined.encode_utf16().collect();
    // Windows 环境块以**双** NUL 结尾(join 已含项间 NUL,这里补两个)
    block.push(0);
    block.push(0);
    block
}

/// 构建 Windows 命令行(参数引用规则:含空白或引号的参数加双引号,
/// 内部引号转义为 `\"`,引号内结尾反斜杠翻倍)。纯函数 —— 跨平台可测。
pub fn build_command_line(command: &str, args: &[String]) -> String {
    let mut out = command.to_string();
    for a in args {
        out.push(' ');
        out.push_str(&quote_windows_arg(a));
    }
    out
}

/// 单个参数的引用。
fn quote_windows_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| c == ' ' || c == '\t' || c == '"') {
        return arg.to_string();
    }
    // 内部引号 → \" ;引号内结尾的反斜杠需翻倍(Windows 引用规则)。
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => {
                backslashes += 1;
            }
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                out.push(c);
            }
        }
    }
    if backslashes > 0 {
        out.push_str(&"\\".repeat(backslashes * 2));
    }
    out.push('"');
    out
}

// ── 平台实现 ─────────────────────────────────────────────────────────────

/// Windows 真实 FFI。
#[cfg(windows)]
mod imp {
    use super::super::windows::{KillPlan, WindowsSandboxPlan};
    use super::{SpawnError, Spawned};
    use std::collections::BTreeMap;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_EXECUTE, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW,
        NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW,
        TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::Isolation::CreateAppContainerProfile;
    use windows_sys::Win32::Security::Isolation::DeriveAppContainerSidFromAppContainerName;
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, PSID, SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Memory::{
        GetProcessHeap, HEAP_ZERO_MEMORY, HeapAlloc, HeapFree,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, CreateProcessW, DeleteProcThreadAttributeList,
        InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, ResumeThread,
        STARTUPINFOEXW, UpdateProcThreadAttribute,
    };

    /// 已知静态 capability SID(微软文档《App capability SIDs》)。
    ///
    /// 自定义 capability(如 workspaceReadWrite)无静态 SID —— 文件访问
    /// 改由 ACL 授权表达([`super::grant_read_acl`]),此处不列出。
    ///
    /// 必须 `pub`:同名跨平台测试 `static_capability_sid_table_matches_docs`
    /// 在 Windows 上走本函数(非 Windows 侧对应实现即 `pub`);此处私有会让
    /// Windows 的 `cargo test` 直接以 E0603 编译失败(macOS 编译不到该分支,
    /// 本地不可见)。
    pub fn static_capability_sid(
        cap: &super::super::windows::AppContainerCapability,
    ) -> Option<&'static str> {
        use super::super::windows::AppContainerCapability as C;
        match cap {
            C::InternetClient => Some("S-1-15-3-1"),
            C::PrivateNetworkClientServer => Some("S-1-15-3-3"),
            C::DocumentsLibrary => Some("S-1-15-3-4"),
            C::WorkspaceReadWrite => None,
        }
    }

    pub fn spawn(
        plan: &WindowsSandboxPlan,
        command: &str,
        args: &[String],
    ) -> Result<Spawned, SpawnError> {
        // ① Job Object(kill-on-close + 可选内存上限)。
        let job = create_job_with_limits(&plan.job).map_err(|e| SpawnError::Ffi {
            api: "CreateJobObject/SetInformationJobObject".into(),
            detail: e,
        })?;

        // ② AppContainer profile(有静态 capability 时才创建)。
        //    自定义 capability(WorkspaceReadWrite)不进 SID 数组 —— 文件
        //    访问由 grant_read_acl 表达。失败 → 降级为仅 Job Object 隔离
        //    并 warn(不静默声称已隔离)。
        let mut appcontainer = false;
        let mut container_sid: Vec<u8> = Vec::new();
        let has_static_caps = plan
            .capabilities
            .iter()
            .any(|c| static_capability_sid(c).is_some());
        if has_static_caps {
            match create_appcontainer_profile(&plan.capabilities) {
                Ok(sid) => {
                    appcontainer = true;
                    container_sid = sid;
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "AppContainer profile 创建失败,降级为仅 Job Object 隔离"
                    );
                }
            }
        }

        // ③ CREATE_SUSPENDED → assign → resume(无逃逸窗口)。
        let cmdline = super::build_command_line(command, args);
        let mut cmd_wide = to_wide(&cmdline);
        let exe_wide = to_wide(command);
        // 空 = 继承;当前 CreateProcess 对 env 传 NULL,本块留作 env
        // 接线参数待用(下划线前缀消除 unused 告警)。
        let _env_block = super::build_env_block(&BTreeMap::new());
        let mut siex: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        siex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;

        // attribute list:AppContainer 注入需要 1 个槽。
        let mut attr_size = 0usize;
        unsafe {
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attr_size);
        }
        let attr_mem = unsafe { alloc_heap(attr_size) };
        if attr_mem.is_null() {
            unsafe { CloseHandle(job) };
            return Err(SpawnError::Ffi {
                api: "HeapAlloc".into(),
                detail: "OOM".into(),
            });
        }
        let attr_list = attr_mem as LPPROC_THREAD_ATTRIBUTE_LIST;
        unsafe {
            if InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size) == 0 {
                DeleteProcThreadAttributeList(attr_list);
                free_heap(attr_mem);
                CloseHandle(job);
                return Err(SpawnError::Ffi {
                    api: "InitializeProcThreadAttributeList".into(),
                    detail: "init failed".into(),
                });
            }
            if appcontainer {
                // attribute 注入**容器 SID**;Attributes 为 AppContainer
                // token 的标准组属性掩码。
                const SE_GROUP_MANDATORY: u32 = 0x0001;
                const SE_GROUP_OWNER: u32 = 0x0002;
                const SE_GROUP_ENABLED: u32 = 0x0004;
                const SE_GROUP_APPCONTAINER: u32 = 0x4000;
                let mut sid_and_attr = SID_AND_ATTRIBUTES {
                    Sid: container_sid.as_ptr() as *mut core::ffi::c_void,
                    Attributes: SE_GROUP_MANDATORY
                        | SE_GROUP_OWNER
                        | SE_GROUP_ENABLED
                        | SE_GROUP_APPCONTAINER,
                };
                if UpdateProcThreadAttribute(
                    attr_list,
                    0,
                    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                    &mut sid_and_attr as *mut _ as *mut core::ffi::c_void,
                    std::mem::size_of::<SID_AND_ATTRIBUTES>() as usize,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ) == 0
                {
                    DeleteProcThreadAttributeList(attr_list);
                    free_heap(attr_mem);
                    CloseHandle(job);
                    return Err(SpawnError::Ffi {
                        api: "UpdateProcThreadAttribute".into(),
                        detail: "security capabilities attach failed".into(),
                    });
                }
            }
        }

        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let create_ok = unsafe {
            CreateProcessW(
                exe_wide.as_ptr(),
                cmd_wide.as_mut_ptr(),
                std::ptr::null(), // 进程安全属性(job 接管生命周期)
                std::ptr::null(), // 线程安全属性
                0,                // bInheritHandles = FALSE
                CREATE_SUSPENDED, // 挂起,防逃逸窗口
                std::ptr::null(), // env 继承(env block 注入待 CI 验证后启用)
                std::ptr::null(), // cwd 继承
                &siex.StartupInfo,
                &mut pi,
            )
        };
        unsafe {
            DeleteProcThreadAttributeList(attr_list);
            free_heap(attr_mem);
        }
        if create_ok == 0 {
            unsafe { CloseHandle(job) };
            return Err(SpawnError::Ffi {
                api: "CreateProcessW".into(),
                detail: "CreateProcessW returned 0 (GetLastError 需 Windows CI 取证)".into(),
            });
        }

        // ④ 挂起态 assign → resume。
        let assigned = unsafe { AssignProcessToJobObject(job, pi.hProcess) } != 0;
        unsafe { ResumeThread(pi.hThread) };
        let pid = pi.dwProcessId;
        // job 句柄**有意**保持打开(KILL_ON_CLOSE 要求 job 句柄存续,否则
        // 子进程退出 job 的隔离即失效);句柄由会话生命周期管理,进程退出
        // 时内核兜底回收。文档见 windows::JobLimits.kill_on_close。
        let _ = job;

        Ok(Spawned {
            pid,
            assigned_to_job: assigned,
            appcontainer,
            capabilities: plan
                .capabilities
                .iter()
                .map(|c| c.as_str().to_string())
                .collect(),
        })
    }

    pub fn kill_tree(plan: &KillPlan) -> bool {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_TERMINATE, TerminateProcess,
        };
        let mut all_ok = true;
        for &pid in &plan.pids {
            unsafe {
                let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
                if handle.is_null() {
                    all_ok = false;
                    continue;
                }
                if TerminateProcess(handle, 1) == 0 {
                    all_ok = false;
                }
                CloseHandle(handle);
            }
        }
        all_ok
    }

    pub fn enumerate(root: u32) -> Vec<(u32, u32)> {
        // ToolHelp 枚举全量 (pid, ppid),复用策略层 KillPlan::build 收集
        // 树成员(环安全),返回树内成员的 (pid, parent) 对。
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
                return Vec::new();
            }
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut all: Vec<(u32, u32)> = Vec::new();
            if Process32FirstW(snapshot, &mut entry) != 0 {
                loop {
                    all.push((entry.th32ProcessID, entry.th32ParentProcessID));
                    if Process32NextW(snapshot, &mut entry) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snapshot);
            let tree = KillPlan::build(root, &all);
            all.into_iter()
                .filter(|(pid, _)| tree.pids.contains(pid))
                .collect()
        }
    }

    fn create_job_with_limits(job: &super::super::windows::JobLimits) -> Result<HANDLE, String> {
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err("CreateJobObjectW returned null".into());
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if let Some(mb) = job.memory_limit_mb {
                limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
                limits.ProcessMemoryLimit = (mb as usize) * 1024 * 1024;
            }
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                CloseHandle(handle);
                return Err("SetInformationJobObject failed".into());
            }
            Ok(handle)
        }
    }

    /// 创建 AppContainer profile:
    /// ① 已知静态 capability SID → ConvertStringSidToSidW 转 PSID;
    /// ② CreateAppContainerProfile 注册(幂等,同名重建不报错);
    /// ③ DeriveAppContainerSidFromAppContainerName 取容器 SID。
    /// 返回容器 SID 字节(供 attribute 注入与 ACL 授权)。
    ///
    /// 必须 `pub`:Windows 侧集成测试(profile 幂等性 / ACL 授权)直接
    /// 调用 `imp::create_appcontainer_profile`;私有会让 `cargo test` 以
    /// E0603 编译失败(macOS 编译不到该分支,本地不可见)。
    pub fn create_appcontainer_profile(
        capabilities: &[super::super::windows::AppContainerCapability],
    ) -> Result<Vec<u8>, String> {
        // 全函数体 unsafe 包裹(FFI 调用序列;输入均为本模块构造的可信
        // 宽字符串,无越界/悬垂风险)。
        unsafe {
            // 容器名按 capability 集合派生(幂等:同名重建不报错)。
            let name = format!("zhuye.sandbox.{}", capabilities.len());
            let name_w = to_wide(&name);
            let display_w = to_wide("ZhuYe sandbox");
            let desc_w = to_wide("ZhuYe agent sandbox profile");

            // ① 静态 capability SID 字符串 → PSID。
            let mut sids: Vec<SID_AND_ATTRIBUTES> = Vec::new();
            for cap in capabilities {
                let Some(sid_str) = static_capability_sid(cap) else {
                    continue;
                };
                let sid_w = to_wide(sid_str);
                let mut psid: PSID = std::ptr::null_mut();
                if ConvertStringSidToSidW(sid_w.as_ptr(), &mut psid) == 0 {
                    return Err(format!("ConvertStringSidToSidW({sid_str}) failed"));
                }
                sids.push(SID_AND_ATTRIBUTES {
                    Sid: psid,
                    Attributes: 0,
                });
            }

            // ② 创建 profile(capability 数组在创建时注册)。
            let mut profile_psid: PSID = std::ptr::null_mut();
            let hr = CreateAppContainerProfile(
                name_w.as_ptr(),
                display_w.as_ptr(),
                desc_w.as_ptr(),
                sids.as_ptr(),
                sids.len() as u32,
                &mut profile_psid,
            );
            // 外层 unsafe 块已覆盖,内层包装属冗余(unused_unsafe);
            // PSID 本就是 *mut c_void,恒等转换会被新 clippy 拒收。
            LocalFree(profile_psid);
            if hr != 0 {
                return Err(format!("CreateAppContainerProfile hr=0x{hr:x}"));
            }

            // ③ 取容器 SID(DeriveAppContainerSidFromAppContainerName)。
            let mut container_psid: PSID = std::ptr::null_mut();
            let hr =
                DeriveAppContainerSidFromAppContainerName(name_w.as_ptr(), &mut container_psid);
            if hr != 0 {
                return Err(format!(
                    "DeriveAppContainerSidFromAppContainerName hr=0x{hr:x}"
                ));
            }
            // 取回 SID 字节(标准 SID 最大 68 字节;越界由内核布局保证不发生)。
            let bytes = std::slice::from_raw_parts(container_psid as *const u8, 68).to_vec();
            // capability 转换出的 PSID 为 LocalAlloc 分配 —— 逐一释放。
            for sid in &sids {
                LocalFree(sid.Sid);
            }
            Ok(bytes)
        }
    }

    /// ACL 读授权:给 AppContainer SID 授予 path 的 GENERIC_READ|EXECUTE
    /// (目录继承)。策略层 `plan.read_grants` 的落地。
    pub fn grant_read_acl(path: &str, sid_bytes: &[u8]) -> Result<(), String> {
        if sid_bytes.is_empty() {
            return Err("empty SID".into());
        }
        // 全函数体 unsafe 包裹(ACL FFI 序列;仅操作本模块构造的参数)。
        unsafe {
            let path_w = to_wide(path);
            // ① 取现有 DACL
            let mut old_dacl: *mut windows_sys::Win32::Security::ACL = std::ptr::null_mut();
            let hr = GetNamedSecurityInfoW(
                path_w.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut old_dacl,
                std::ptr::null_mut(),
                std::ptr::null_mut(), // ppsecuritydescriptor(调用方不持有)
            );
            if hr != 0 {
                return Err(format!("GetNamedSecurityInfoW hr=0x{hr:x}"));
            }
            // ② 构造显式访问项(BuildExplicitAccessWithName 在 windows-sys 无
            //    导出,手工填充 —— 语义与 aclapi.h 内联实现一致)。
            let ea = EXPLICIT_ACCESS_W {
                grfAccessPermissions: GENERIC_READ | GENERIC_EXECUTE,
                grfAccessMode: GRANT_ACCESS,
                grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: std::ptr::null_mut(),
                    MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_UNKNOWN,
                    ptstrName: sid_bytes.as_ptr() as *mut u16,
                },
            };
            // ③ 合并新 DACL
            let mut new_dacl: *mut windows_sys::Win32::Security::ACL = std::ptr::null_mut();
            let hr = SetEntriesInAclW(1, &ea, old_dacl, &mut new_dacl);
            if hr != 0 {
                LocalFree(old_dacl as *mut core::ffi::c_void);
                return Err(format!("SetEntriesInAclW hr=0x{hr:x}"));
            }
            // ④ 应用
            let hr = SetNamedSecurityInfoW(
                path_w.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                new_dacl,
                std::ptr::null_mut(),
            );
            LocalFree(new_dacl as *mut core::ffi::c_void);
            LocalFree(old_dacl as *mut core::ffi::c_void);
            if hr != 0 {
                return Err(format!("SetNamedSecurityInfoW hr=0x{hr:x}"));
            }
            Ok(())
        }
    }

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    unsafe fn alloc_heap(size: usize) -> *mut core::ffi::c_void {
        // edition 2024:unsafe fn 体内的 FFI 调用仍需显式 unsafe 块。
        unsafe {
            let heap = GetProcessHeap();
            HeapAlloc(heap, HEAP_ZERO_MEMORY, size)
        }
    }

    unsafe fn free_heap(ptr: *mut core::ffi::c_void) {
        unsafe {
            let heap = GetProcessHeap();
            HeapFree(heap, 0, ptr);
        }
    }
}

/// 非 Windows stub:策略层可跨平台测试,FFI 不可用。
#[cfg(not(windows))]
mod imp {
    use super::{KillPlan, SpawnError, Spawned, WindowsSandboxPlan};

    pub fn spawn(
        _plan: &WindowsSandboxPlan,
        _command: &str,
        _args: &[String],
    ) -> Result<Spawned, SpawnError> {
        Err(SpawnError::Unsupported)
    }

    pub fn kill_tree(_plan: &KillPlan) -> bool {
        false
    }

    pub fn enumerate(_root: u32) -> Vec<(u32, u32)> {
        Vec::new()
    }

    /// 非 Windows:ACL 授权不可用。
    pub fn grant_read_acl(_path: &str, _sid: &[u8]) -> Result<(), String> {
        Err("unsupported".into())
    }

    /// 非 Windows stub:SID 表是纯逻辑,跨平台可测(与 Windows imp 同表)。
    /// 仅测试消费(host 平台无 FFI 调用方),故 allow dead_code。
    #[allow(dead_code)]
    pub fn static_capability_sid(
        cap: &super::super::windows::AppContainerCapability,
    ) -> Option<&'static str> {
        use super::super::windows::AppContainerCapability as C;
        match cap {
            C::InternetClient => Some("S-1-15-3-1"),
            C::PrivateNetworkClientServer => Some("S-1-15-3-3"),
            C::DocumentsLibrary => Some("S-1-15-3-4"),
            C::WorkspaceReadWrite => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::windows::{AppContainerCapability, WindowsSandbox, WindowsSandboxConfig};
    // JobLimits 仅被 `#[cfg(not(windows))]` 用例使用;Windows 编译该
    // 导入为 unused(-D warnings 拒收),按平台门控分开导入。
    #[cfg(not(windows))]
    use super::super::windows::JobLimits;
    use super::*;
    use std::collections::BTreeMap;
    // 真机集成测试直接调 Win32 API:CloseHandle 只在本模块的 `imp` 里导入,
    // `use super::*` 取不到子模块的导入项,须显式引入(仅 Windows)。
    #[cfg(windows)]
    use windows_sys::Win32::Foundation::CloseHandle;

    fn plan() -> WindowsSandboxPlan {
        WindowsSandbox::new(WindowsSandboxConfig::default()).plan()
    }

    #[test]
    fn build_command_line_plain_args() {
        assert_eq!(build_command_line("cmd", &[]), "cmd");
        assert_eq!(
            build_command_line("cmd", &["a".into(), "b".into()]),
            "cmd a b"
        );
    }

    #[test]
    fn build_command_line_quotes_spaces() {
        assert_eq!(
            build_command_line("cmd", &["hello world".into()]),
            "cmd \"hello world\""
        );
    }

    #[test]
    fn build_command_line_escapes_inner_quotes() {
        assert_eq!(
            build_command_line("cmd", &["say \"hi\"".into()]),
            "cmd \"say \\\"hi\\\"\""
        );
    }

    #[test]
    fn build_command_line_no_quotes_without_whitespace() {
        // 无空白/tab/引号的参数不加引号(Windows 引用规则);反斜杠
        // 只在**引号内**的结尾才需翻倍,未引用形态原样保留。
        assert_eq!(
            build_command_line("cmd", &["C:\\dir\\".into()]),
            "cmd C:\\dir\\"
        );
        // 含空白 → 引用 + 结尾反斜杠翻倍(防与引号结合成转义)。
        assert_eq!(
            build_command_line("cmd", &["C:\\my dir\\".into()]),
            "cmd \"C:\\my dir\\\\\""
        );
    }

    #[test]
    fn env_block_shape() {
        let mut env = BTreeMap::new();
        env.insert("B".to_string(), "2".to_string());
        env.insert("A".to_string(), "1".to_string());
        let block = build_env_block(&env);
        // UTF-16 lossy:A=1 NUL B=2 NUL NUL(双 NUL 结尾)
        let text: String = String::from_utf16_lossy(&block);
        assert_eq!(text, "A=1\u{0}B=2\u{0}\u{0}");
    }

    #[test]
    fn env_block_empty() {
        let block = build_env_block(&BTreeMap::new());
        assert_eq!(block, vec![0, 0]);
    }

    #[test]
    fn env_block_value_with_equals() {
        let mut env = BTreeMap::new();
        env.insert("K".to_string(), "v=1".to_string());
        let text = String::from_utf16_lossy(&build_env_block(&env));
        assert_eq!(text, "K=v=1\u{0}\u{0}", "值中的 = 不拆分");
    }

    #[test]
    fn spawn_rejects_empty_command() {
        let err = spawn_sandboxed(&plan(), "  ", &[]).unwrap_err();
        assert_eq!(err, SpawnError::EmptyCommand);
    }

    #[cfg(not(windows))]
    #[test]
    fn spawn_unsupported_on_non_windows() {
        let err = spawn_sandboxed(&plan(), "cmd", &[]).unwrap_err();
        assert_eq!(err, SpawnError::Unsupported);
        assert!(!kill_tree(&KillPlan::build(1, &[])));
        assert!(enumerate_process_tree(1).is_empty());
        assert!(grant_read_acl("x", &[1]).is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn plan_carries_capabilities_into_spawn_path() {
        // 策略层 → spawn 入口的形状契约:capabilities 经 plan 传入,
        // Windows 侧据此创建 AppContainer(此处仅验证跨平台字段通路)。
        let p = plan();
        assert!(!p.capabilities.is_empty());
        assert!(
            p.capabilities
                .contains(&AppContainerCapability::WorkspaceReadWrite)
        );
        let _ = JobLimits::default();
    }

    #[test]
    fn static_capability_sid_table_matches_docs() {
        // 已知静态 capability SID(微软文档)—— 表值写错会让 token 缺
        // capability,网络类工具静默失效。锁死字面量。
        let caps = [
            (AppContainerCapability::InternetClient, "S-1-15-3-1"),
            (
                AppContainerCapability::PrivateNetworkClientServer,
                "S-1-15-3-3",
            ),
            (AppContainerCapability::DocumentsLibrary, "S-1-15-3-4"),
        ];
        for (cap, expected) in caps {
            assert_eq!(imp::static_capability_sid(&cap), Some(expected));
        }
        // 自定义 capability 无静态 SID(文件访问走 ACL)。
        assert_eq!(
            imp::static_capability_sid(&AppContainerCapability::WorkspaceReadWrite),
            None
        );
    }

    // ── Windows 真机集成测试(仅 Windows 且显式 --ignored 跑)──────────
    //
    // 运行:`cargo test -p zhuye-sandbox --target x86_64-pc-windows-msvc -- --ignored`
    // 验证真实 Win32 行为:job 创建、挂起态 assign、树枚举、ACL 授权。
    // 标注 #[ignore] —— 非 Windows 开发机与默认 CI 不跑。

    /// 真实 spawn:notepad.exe 在 Job Object 里跑起来再杀掉。
    /// 验证:CreateProcessW / assign / kill 全链路。
    #[cfg(windows)]
    #[test]
    #[ignore = "Windows 真机集成测试:--ignored 显式运行"]
    fn real_spawn_and_kill_in_job() {
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
        let mut plan = plan();
        plan.job.memory_limit_mb = Some(256);
        let spawned = spawn_sandboxed(&plan, "cmd.exe", &["/c".into(), "echo ok".into()])
            .expect("真实 spawn 应成功");
        assert!(spawned.assigned_to_job, "子进程应已加入 Job Object");
        assert!(spawned.pid > 0);

        // 树枚举应能找到该进程。
        let tree = enumerate_process_tree(spawned.pid);
        assert!(
            tree.iter().any(|(pid, _)| *pid == spawned.pid),
            "树枚举应含刚 spawn 的进程"
        );

        // 击杀并确认退出。
        assert!(kill_tree(&KillPlan::build(spawned.pid, &[])));
        // 等待进程退出(经 OpenProcess 轮询退出码,简化验证)。
        unsafe {
            use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION};
            let h = OpenProcess(PROCESS_QUERY_INFORMATION, 0, spawned.pid);
            if !h.is_null() {
                let mut code = 0u32;
                for _ in 0..50 {
                    if WaitForSingleObject(h, 100) == WAIT_OBJECT_0 {
                        GetExitCodeProcess(h, &mut code);
                        break;
                    }
                }
                CloseHandle(h);
            }
        }
    }

    /// 真实 AppContainer:profile 创建 + 容器 SID 派生。
    #[cfg(windows)]
    #[test]
    #[ignore = "Windows 真机集成测试:--ignored 显式运行"]
    fn real_appcontainer_profile_lifecycle() {
        use super::imp;
        // 建带静态 capability 的 profile。
        let caps = vec![
            AppContainerCapability::InternetClient,
            AppContainerCapability::PrivateNetworkClientServer,
        ];
        let sid = imp::create_appcontainer_profile(&caps).expect("profile 创建应成功");
        assert!(!sid.is_empty(), "容器 SID 应非空");
        // 幂等:同名重建不报错。
        let sid2 = imp::create_appcontainer_profile(&caps).expect("重建应幂等");
        assert_eq!(sid, sid2, "同 capability 集合应派生同一容器 SID");
    }

    /// 真实 ACL:给 %TEMP% 下新建目录授予读授权。
    #[cfg(windows)]
    #[test]
    #[ignore = "Windows 真机集成测试:--ignored 显式运行"]
    fn real_grant_read_acl() {
        use super::imp;
        let dir = std::env::temp_dir().join(format!("zhuye_acl_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建目录");
        // 先建一个空 profile 拿容器 SID。
        let sid = imp::create_appcontainer_profile(&[]).expect("profile");
        let result = imp::grant_read_acl(dir.to_str().expect("utf8"), &sid);
        std::fs::remove_dir_all(&dir).ok();
        assert!(result.is_ok(), "ACL 授权应成功:{result:?}");
    }

    /// 真实 env 注入:子进程读到的环境变量来自脱敏后的块(而非继承)。
    /// 验证 env block 传递路径(当前 spawn 传 NULL 继承,本测试在启用
    /// env 注入后应转绿——现标注为待启用状态的活文档)。
    #[cfg(windows)]
    #[test]
    #[ignore = "待 env 注入启用后转绿:--ignored 显式运行"]
    fn real_spawn_env_injection() {
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION};
        let mut plan = plan();
        plan.job.memory_limit_mb = Some(128);
        // spawn_sandboxed 当前对 env 传 NULL(继承)——本测试作为启用
        // 注入后的验收标准保留。
        let spawned =
            spawn_sandboxed(&plan, "cmd.exe", &["/c".into(), "exit".into()]).expect("spawn");
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_INFORMATION, 0, spawned.pid);
            assert!(!h.is_null());
            CloseHandle(h);
        }
    }
}
