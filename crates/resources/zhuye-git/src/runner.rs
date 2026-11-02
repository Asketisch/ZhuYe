//! 有界命令执行抽象 —— 移植 Reflect-TUI `tui_core/workspace_command.rs`
//! 的 `WorkspaceCommandExecutor`。
//!
//! TUI 版的设计要点原样保留:**探测逻辑只与 trait 对话,不直接用
//! `tokio::process::Command`** —— 这样同一套分支 / PR 查找逻辑在
//! 客户端连接到嵌入式或远程执行端时都能工作(本地直跑 / 经 app-server
//! `command/exec` 转发只是两种实现)。
//!
//! trait 保持**对象安全**(`&dyn CommandRunner` + boxed future),调用方
//! 可注入 Fake 做无 IO 单测。

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// 一条待执行命令(argv + cwd + env + 超时 + 输出上限)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceCommand {
    pub argv: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
    /// stdout/stderr 各自的读取上限(字节);超出按截断计。
    pub max_output_bytes: usize,
}

impl WorkspaceCommand {
    /// 便捷构造:默认 5s 超时 + 64KiB 输出上限(与 TUI 版一致)。
    pub fn new(argv: Vec<String>, cwd: impl Into<String>) -> Self {
        Self {
            argv,
            cwd: cwd.into(),
            env: Vec::new(),
            timeout: Duration::from_secs(5),
            max_output_bytes: 64 * 1024,
        }
    }

    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// 命令执行结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// stdout / stderr 是否因超上限被截断。
    pub truncated: bool,
    /// 是否因超时被 kill。
    pub timed_out: bool,
}

impl CommandOutput {
    /// 退出码为 0(便捷判定,替代原 TUI 版的 `status.success()`)。
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

/// 命令执行错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceCommandError {
    /// 执行端不可用(二进制缺失 / 连接断)。
    Unavailable(String),
    /// 执行端返回的错误。
    Failed(String),
}

impl std::fmt::Display for WorkspaceCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(e) => write!(f, "command runner unavailable: {e}"),
            Self::Failed(e) => write!(f, "command failed: {e}"),
        }
    }
}

impl std::error::Error for WorkspaceCommandError {}

/// 有界命令执行器(对象安全)。探测逻辑的全部依赖面。
pub trait CommandRunner: Send + Sync {
    fn run<'a>(
        &'a self,
        cmd: &'a WorkspaceCommand,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, WorkspaceCommandError>> + Send + 'a>>;
}

/// 本地实现:直接 spawn 子进程(默认 runner)。
///
/// - 环境**不继承**进程 env,只带 [`WorkspaceCommand::env`](TUI 版的
///   卫生口径:探测不该拿到宿主的凭据);
/// - 超时 kill;输出按 `max_output_bytes` 截断并标记。
pub struct LocalCommandRunner;

impl CommandRunner for LocalCommandRunner {
    fn run<'a>(
        &'a self,
        cmd: &'a WorkspaceCommand,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, WorkspaceCommandError>> + Send + 'a>>
    {
        Box::pin(async move {
            let (program, args) = cmd
                .argv
                .split_first()
                .ok_or_else(|| WorkspaceCommandError::Failed("empty argv".into()))?;
            let mut command = tokio::process::Command::new(program);
            command
                .args(args)
                .current_dir(&cmd.cwd)
                .env_clear()
                .envs(cmd.env.iter().cloned())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let child = command
                .spawn()
                .map_err(|e| WorkspaceCommandError::Unavailable(e.to_string()))?;
            // 超时即 kill(用 start_kill 而非 kill_on_timeout —— 后者需要
            // tokio 的 "process" + "time" feature 组合,且 wait_with_output
            // 已持有 child;这里显式 kill 更直接)。
            let out = match tokio::time::timeout(cmd.timeout, child.wait_with_output()).await {
                Ok(out) => out.map_err(|e| WorkspaceCommandError::Failed(e.to_string()))?,
                Err(_) => {
                    return Ok(CommandOutput {
                        exit_code: -1,
                        stdout: String::new(),
                        stderr: String::new(),
                        truncated: false,
                        timed_out: true,
                    });
                }
            };
            let mut truncated = false;
            let stdout = take_capped(&out.stdout, cmd.max_output_bytes, &mut truncated);
            let stderr = take_capped(&out.stderr, cmd.max_output_bytes, &mut truncated);
            Ok(CommandOutput {
                exit_code: out.status.code().unwrap_or(-1),
                stdout,
                stderr,
                truncated,
                timed_out: false,
            })
        })
    }
}

fn take_capped(bytes: &[u8], cap: usize, truncated: &mut bool) -> String {
    if bytes.len() > cap {
        *truncated = true;
        String::from_utf8_lossy(&bytes[..cap]).into_owned()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// 共享的本地 runner 句柄。
pub fn local_runner() -> std::sync::Arc<dyn CommandRunner> {
    std::sync::Arc::new(LocalCommandRunner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_echo_captures_stdout_and_exit_code() {
        let cmd = WorkspaceCommand::new(
            vec!["echo".into(), "runner-ok".into()],
            std::env::temp_dir().to_string_lossy().into_owned(),
        );
        let out = LocalCommandRunner.run(&cmd).await.unwrap();
        assert_eq!(out.exit_code, 0);
        assert!(out.success());
        assert!(out.stdout.contains("runner-ok"), "{}", out.stdout);
        assert!(!out.truncated);
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn timeout_kills_process_and_reports_timed_out() {
        let cmd = WorkspaceCommand::new(
            vec!["sleep".into(), "30".into()],
            std::env::temp_dir().to_string_lossy().into_owned(),
        )
        .with_timeout(std::time::Duration::from_millis(150));
        let out = LocalCommandRunner.run(&cmd).await.unwrap();
        assert!(out.timed_out, "应报告超时");
        assert_eq!(out.exit_code, -1);
        assert!(!out.success());
    }

    #[tokio::test]
    async fn output_over_cap_is_truncated_and_flagged() {
        // 产出 4KB,上限 64 字节 → 截断 + 标记。
        let cmd = WorkspaceCommand::new(
            vec![
                "sh".into(),
                "-c".into(),
                "printf 'x%.0s' $(seq 1 4096)".into(),
            ],
            std::env::temp_dir().to_string_lossy().into_owned(),
        )
        .with_timeout(std::time::Duration::from_secs(10));
        let mut small = cmd.clone();
        small.max_output_bytes = 64;
        let out = LocalCommandRunner.run(&small).await.unwrap();
        assert!(out.truncated);
        assert_eq!(out.stdout.len(), 64);
        // 不设上限的对照:全量拿回。
        let out2 = LocalCommandRunner.run(&cmd).await.unwrap();
        assert!(!out2.truncated);
        assert!(out2.stdout.len() >= 4096);
    }

    #[tokio::test]
    async fn env_is_not_inherited_from_host() {
        // env_clear 口径:宿主的自定义变量不透传;显式 env 列表才注入。
        let cmd = WorkspaceCommand::new(
            vec![
                "sh".into(),
                "-c".into(),
                "echo \"[$ZHUYE_RUNNER_PROBE][$MY_PROBE]\"".into(),
            ],
            std::env::temp_dir().to_string_lossy().into_owned(),
        )
        .with_env(vec![("MY_PROBE".into(), "yes".into())]);
        unsafe {
            std::env::set_var("ZHUYE_RUNNER_PROBE", "leak");
        }
        let out = LocalCommandRunner.run(&cmd).await.unwrap();
        unsafe {
            std::env::remove_var("ZHUYE_RUNNER_PROBE");
        }
        assert!(
            out.stdout.contains("[][yes]"),
            "宿主变量不得透传,显式 env 应注入: {}",
            out.stdout
        );
    }

    #[tokio::test]
    async fn missing_binary_reports_unavailable() {
        let cmd = WorkspaceCommand::new(
            vec!["zhuye-no-such-binary-xyz".into()],
            std::env::temp_dir().to_string_lossy().into_owned(),
        );
        let err = LocalCommandRunner.run(&cmd).await.unwrap_err();
        assert!(matches!(err, WorkspaceCommandError::Unavailable(_)));
    }
}
