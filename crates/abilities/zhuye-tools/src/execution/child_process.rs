//! 按命令隔离 Unix 进程组，在取消或任务丢弃时清理仍在组内的后代。
//!
//! 自行创建新会话或进程组的后代不受此机制覆盖；Windows 仍依赖直接子进程终止。

use std::io;
use tokio::process::{Child, Command};

/// 与子进程共同持有其进程组，确保丢弃时先终止进程组再丢弃子进程。
pub struct GroupedChild {
    child: Child,
    #[cfg(unix)]
    group: Option<rustix::process::Pid>,
}

impl GroupedChild {
    /// 在独立进程组中启动命令；调用方仍须配置标准流与环境。
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(unix)]
        command.process_group(0);
        #[allow(unused_mut)]
        let mut child = command.spawn()?;
        #[cfg(unix)]
        let group = match child.id().and_then(|id| i32::try_from(id).ok()) {
            Some(id) if id > 1 => rustix::process::Pid::from_raw(id),
            _ => None,
        };
        #[cfg(unix)]
        if group.is_none() {
            let _ = child.start_kill();
            return Err(io::Error::other("无法取得安全的子进程组 ID"));
        }
        Ok(Self {
            child,
            #[cfg(unix)]
            group,
        })
    }

    /// 先向进程组发送终止信号，再终止直接子进程；调用方应继续等待回收。
    pub fn terminate(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group.take() {
            if let Err(error) =
                rustix::process::kill_process_group(group, rustix::process::Signal::KILL)
            {
                if error != rustix::io::Errno::SRCH {
                    tracing::warn!(%error, "终止子进程组失败");
                }
            }
        }
        let _ = self.child.start_kill();
    }

    /// 取出标准输出管道。
    pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.child.stdout.take()
    }

    /// 取出标准错误管道。
    pub fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.child.stderr.take()
    }

    /// 等待并回收直接子进程，在返回前解除进程组保护，避免误杀复用的 PID。
    pub async fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        let result = self.child.wait().await;
        #[cfg(unix)]
        {
            self.group = None;
        }
        result
    }
}

impl Drop for GroupedChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.group.is_some() {
            self.terminate();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 宿主丢弃执行 future 时，进程组析构仍必须阻止后代继续写入。
    #[tokio::test]
    async fn dropping_execution_future_terminates_group() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().to_owned();
        let ready = cwd.join("ready");
        let escaped = cwd.join("escaped");
        let task = tokio::spawn(async move {
            let mut command = Command::new("/bin/sh");
            command
                .current_dir(cwd)
                .arg("-c")
                .arg("(printf ready > ready; sleep 1; printf escaped > escaped) & wait");
            let _child = GroupedChild::spawn(&mut command).unwrap();
            std::future::pending::<()>().await;
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert!(!escaped.exists());
    }
}
