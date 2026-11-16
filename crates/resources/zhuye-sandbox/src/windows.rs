//! # Windows 沙箱(F24)
//!
//! macOS 有 Seatbelt、Linux 有 Landlock/bwrap,Windows 此前裸奔。本模块
//! 提供 Windows 后端的**策略层**:
//!
//! ## 三个机制
//!
//! | 机制 | 作用 | 本模块的职责 |
//! |---|---|---|
//! | AppContainer | 低完整性进程容器 | 声明 capability 需求 |
//! | Job Object | 进程树生命周期 + 资源限制 | 声明 kill-on-close / 内存上限 |
//! | Windows ACL | 文件系统读授权 | 计算 read grants |
//!
//! ## 为什么先做「策略层」而不是直接 FFI
//!
//! `windows-sys` 的句柄权限语义与 POSIX 完全不同(ACL 是 DACL 列表、
//! Job Object 要 `AssignProcessToJobObject`、AppContainer 要 SID + capability)。
//! 这些必须在 Windows 上跑真实测试才能确认(本仓库 CI 有 Windows matrix)。
//! 本模块把**平台无关的决策**先固化下来(需要哪些 capability、给哪些读
//! 权限、进程树怎么杀),Windows 后端只需消费这些决策,不重复推导。
//!
//! ## 非 Windows 平台
//!
//! 本模块在所有平台编译(纯策略,无 FFI)。非 Windows 上
//! [`WindowsSandbox::is_supported`] 返回 `false`,调用方据此回退到
//! 平台原生后端。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// AppContainer capability(低完整性容器需要的访问能力)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppContainerCapability {
    /// 访问网络(客户端出站)。
    InternetClient,
    /// 访问私有网络。
    PrivateNetworkClientServer,
    /// 读用户文档目录。
    DocumentsLibrary,
    /// 读写工作区(自定义 capability)。
    WorkspaceReadWrite,
}

impl AppContainerCapability {
    pub fn as_str(self) -> &'static str {
        match self {
            AppContainerCapability::InternetClient => "internetClient",
            AppContainerCapability::PrivateNetworkClientServer => "privateNetworkClientServer",
            AppContainerCapability::DocumentsLibrary => "documentsLibrary",
            AppContainerCapability::WorkspaceReadWrite => "workspaceReadWrite",
        }
    }
}

/// Job Object 限制。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobLimits {
    /// 进程退出时杀死整个作业(默认 true —— 防孤儿进程)。
    pub kill_on_close: bool,
    /// 内存上限(MB;`None` = 不限)。
    #[serde(default)]
    pub memory_limit_mb: Option<u64>,
    /// 活跃进程数上限(`None` = 不限)。
    #[serde(default)]
    pub active_process_limit: Option<u32>,
}

impl Default for JobLimits {
    fn default() -> Self {
        JobLimits {
            kill_on_close: true,
            memory_limit_mb: None,
            active_process_limit: None,
        }
    }
}

/// Windows 沙箱配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsSandboxConfig {
    /// 启用的 AppContainer capability。
    pub capabilities: Vec<AppContainerCapability>,
    /// Job Object 限制。
    pub job: JobLimits,
    /// 额外可读目录(ACL read grant)。
    pub read_grants: Vec<PathBuf>,
    /// 是否允许网络(不启用 InternetClient 即断网)。
    pub allow_network: bool,
}

impl Default for WindowsSandboxConfig {
    fn default() -> Self {
        WindowsSandboxConfig {
            capabilities: vec![AppContainerCapability::WorkspaceReadWrite],
            job: JobLimits::default(),
            read_grants: Vec::new(),
            allow_network: false,
        }
    }
}

impl WindowsSandboxConfig {
    /// 生效的 capability 列表(网络开关会增删 InternetClient)。
    pub fn effective_capabilities(&self) -> Vec<AppContainerCapability> {
        let mut caps = self.capabilities.clone();
        let has_internet = caps.contains(&AppContainerCapability::InternetClient);
        if self.allow_network && !has_internet {
            caps.push(AppContainerCapability::InternetClient);
        } else if !self.allow_network {
            caps.retain(|c| *c != AppContainerCapability::InternetClient);
        }
        caps.sort();
        caps.dedup();
        caps
    }

    /// 是否需要网络。
    pub fn needs_network(&self) -> bool {
        self.allow_network
    }

    /// 校验:读授权路径必须存在(不存在说明配置错了,别静默忽略)。
    pub fn validate(&self) -> Result<(), WindowsSandboxError> {
        for p in &self.read_grants {
            if !p.exists() {
                return Err(WindowsSandboxError::MissingReadGrant(p.clone()));
            }
        }
        if self.job.memory_limit_mb == Some(0) {
            return Err(WindowsSandboxError::InvalidJobLimit(
                "memory_limit_mb must be > 0".to_string(),
            ));
        }
        Ok(())
    }
}

/// Windows 沙箱错误。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WindowsSandboxError {
    #[error("read grant path does not exist: {0:?}")]
    MissingReadGrant(PathBuf),
    #[error("invalid job limit: {0}")]
    InvalidJobLimit(String),
    #[error("windows sandbox unsupported on this platform")]
    Unsupported,
}

/// Windows 沙箱后端。
#[derive(Debug, Clone, Default)]
pub struct WindowsSandbox {
    config: WindowsSandboxConfig,
}

impl WindowsSandbox {
    pub fn new(config: WindowsSandboxConfig) -> WindowsSandbox {
        WindowsSandbox { config }
    }

    pub fn config(&self) -> &WindowsSandboxConfig {
        &self.config
    }

    /// 当前平台是否支持(仅 `target_os = "windows"` 为 true)。
    pub fn is_supported() -> bool {
        cfg!(target_os = "windows")
    }

    /// 后端名称(UI 展示)。
    pub fn backend_name(&self) -> &'static str {
        if Self::is_supported() {
            "windows-appcontainer-job"
        } else {
            "windows-appcontainer-job (unsupported on this platform)"
        }
    }

    /// 需要创建的 ACL read grant 列表(工作区 + 显式 grants)。
    ///
    /// 工作区自身总是可读写(通过 `WorkspaceReadWrite` capability),
    /// 额外 grants 是只读的。
    pub fn acl_read_grants(&self) -> Vec<PathBuf> {
        self.config.read_grants.clone()
    }

    /// 生成后端初始化计划(供 Windows 侧的 FFI 实现消费)。
    ///
    /// 这是「策略层 → 执行层」的边界:本函数在**所有平台**可调用,
    /// 返回的是纯数据;Windows FFI 只需按它逐条执行。
    pub fn plan(&self) -> WindowsSandboxPlan {
        WindowsSandboxPlan {
            capabilities: self.config.effective_capabilities(),
            job: self.config.job.clone(),
            read_grants: self.acl_read_grants(),
            allow_network: self.config.allow_network,
        }
    }
}

/// 后端初始化计划(纯数据,可跨平台构造与测试)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsSandboxPlan {
    pub capabilities: Vec<AppContainerCapability>,
    pub job: JobLimits,
    pub read_grants: Vec<PathBuf>,
    pub allow_network: bool,
}

impl WindowsSandboxPlan {
    /// 计划是否包含网络能力。
    pub fn has_network(&self) -> bool {
        self.capabilities
            .contains(&AppContainerCapability::InternetClient)
    }

    /// 计划是否会在作业关闭时杀进程树。
    pub fn kills_process_tree(&self) -> bool {
        self.job.kill_on_close
    }

    /// 人读摘要(诊断 / 日志)。
    pub fn summary(&self) -> String {
        format!(
            "appcontainer: {} caps · job: {} · read grants: {} · network: {}",
            self.capabilities.len(),
            if self.job.kill_on_close {
                "kill-on-close"
            } else {
                "no-kill"
            },
            self.read_grants.len(),
            if self.allow_network { "on" } else { "off" }
        )
    }
}

/// 进程树击杀计划(Windows 上通过 Job Object 实现)。
///
/// 返回需要终止的进程 id 集合 —— 实际终止由 Windows 侧
/// `TerminateJobObject` 完成;本函数只做**决策**(哪些算"树内")。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillPlan {
    /// 树根进程。
    pub root: u32,
    /// 需要终止的全部 pid(含 root)。
    pub pids: Vec<u32>,
}

impl KillPlan {
    /// 从 `(pid, parent_pid)` 列表构建击杀计划。
    ///
    /// 深度优先收集所有后代。找不到 root 时返回只含 root 的计划
    /// (保守:至少杀掉已知的根)。
    pub fn build(root: u32, processes: &[(u32, u32)]) -> KillPlan {
        let mut pids = vec![root];
        let mut frontier = vec![root];
        while let Some(parent) = frontier.pop() {
            for (pid, ppid) in processes {
                if *ppid == parent && !pids.contains(pid) {
                    pids.push(*pid);
                    frontier.push(*pid);
                }
            }
        }
        KillPlan { root, pids }
    }

    /// 需要终止的进程数。
    pub fn len(&self) -> usize {
        self.pids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pids.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_workspace_capability() {
        let cfg = WindowsSandboxConfig::default();
        assert!(
            cfg.capabilities
                .contains(&AppContainerCapability::WorkspaceReadWrite)
        );
        assert!(!cfg.allow_network);
        assert!(cfg.job.kill_on_close);
    }

    #[test]
    fn network_switch_adds_and_removes_capability() {
        let mut cfg = WindowsSandboxConfig::default();
        cfg.allow_network = true;
        assert!(
            cfg.effective_capabilities()
                .contains(&AppContainerCapability::InternetClient)
        );
        cfg.allow_network = false;
        assert!(
            !cfg.effective_capabilities()
                .contains(&AppContainerCapability::InternetClient)
        );
    }

    #[test]
    fn effective_capabilities_dedup() {
        let cfg = WindowsSandboxConfig {
            capabilities: vec![
                AppContainerCapability::InternetClient,
                AppContainerCapability::InternetClient,
            ],
            allow_network: true,
            ..Default::default()
        };
        let caps = cfg.effective_capabilities();
        assert_eq!(
            caps.iter()
                .filter(|c| **c == AppContainerCapability::InternetClient)
                .count(),
            1,
            "重复 capability 应去重"
        );
    }

    #[test]
    fn validate_rejects_missing_read_grant() {
        let cfg = WindowsSandboxConfig {
            read_grants: vec![PathBuf::from("/definitely/not/exists/xyz")],
            ..Default::default()
        };
        assert!(matches!(
            cfg.validate(),
            Err(WindowsSandboxError::MissingReadGrant(_))
        ));
    }

    #[test]
    fn validate_rejects_zero_memory_limit() {
        let cfg = WindowsSandboxConfig {
            job: JobLimits {
                kill_on_close: true,
                memory_limit_mb: Some(0),
                active_process_limit: None,
            },
            ..Default::default()
        };
        assert!(matches!(
            cfg.validate(),
            Err(WindowsSandboxError::InvalidJobLimit(_))
        ));
    }

    #[test]
    fn validate_accepts_existing_path() {
        let tmp = std::env::temp_dir();
        let cfg = WindowsSandboxConfig {
            read_grants: vec![tmp],
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn unsupported_on_non_windows() {
        // 本仓库 CI 在 macOS/Linux 上跑:is_supported 应为 false。
        if !cfg!(target_os = "windows") {
            assert!(!WindowsSandbox::is_supported());
            let sb = WindowsSandbox::default();
            assert!(sb.backend_name().contains("unsupported"));
        }
    }

    #[test]
    fn plan_reflects_config() {
        let cfg = WindowsSandboxConfig {
            allow_network: true,
            job: JobLimits {
                kill_on_close: true,
                memory_limit_mb: Some(512),
                active_process_limit: Some(8),
            },
            ..Default::default()
        };
        let plan = WindowsSandbox::new(cfg).plan();
        assert!(plan.has_network());
        assert!(plan.kills_process_tree());
        assert_eq!(plan.job.memory_limit_mb, Some(512));
        let s = plan.summary();
        assert!(s.contains("network: on"), "{s}");
        assert!(s.contains("kill-on-close"), "{s}");
    }

    #[test]
    fn plan_without_network() {
        let plan = WindowsSandbox::default().plan();
        assert!(!plan.has_network());
        assert!(plan.summary().contains("network: off"));
    }

    #[test]
    fn plan_serializes() {
        let plan = WindowsSandbox::default().plan();
        let json = serde_json::to_string(&plan).expect("serialize");
        let back: WindowsSandboxPlan = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, plan);
    }

    #[test]
    fn kill_plan_collects_descendants() {
        // 树:1 → 2, 1 → 3, 2 → 4, 2 → 5
        let procs = [(2, 1), (3, 1), (4, 2), (5, 2), (99, 50)];
        let plan = KillPlan::build(1, &procs);
        assert_eq!(plan.root, 1);
        let mut pids = plan.pids.clone();
        pids.sort();
        assert_eq!(pids, vec![1, 2, 3, 4, 5], "只收树内进程");
        assert!(!plan.pids.contains(&99), "别的树不该被误杀");
    }

    #[test]
    fn kill_plan_unknown_root() {
        let plan = KillPlan::build(42, &[(2, 1)]);
        assert_eq!(plan.pids, vec![42], "至少杀掉已知的根");
    }

    #[test]
    fn kill_plan_handles_cycles() {
        // 异常数据:环(实际 Windows 不会出现,但不能死循环)。
        let procs = [(2, 1), (1, 2)];
        let plan = KillPlan::build(1, &procs);
        assert!(plan.pids.contains(&1));
        assert!(plan.pids.contains(&2));
        assert_eq!(plan.len(), 2, "环不应导致重复/死循环");
    }

    #[test]
    fn kill_plan_empty_tree() {
        let plan = KillPlan::build(7, &[]);
        assert_eq!(plan.pids, vec![7]);
        assert_eq!(plan.len(), 1);
    }
}
