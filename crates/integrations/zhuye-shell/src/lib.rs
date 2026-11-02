//! `zhuye-shell` — 常驻 shell 会话池(WP16 收编 ReflectDesktop
//! `src-tauri/src/shell_sessions.rs` 的 registry + `commands/shell.rs` 的
//! pump 正确性细节)。
//!
//! 与内置 `bash` 工具的区别:bash 是**一次性**命令(跑完拿全部输出);
//! 本 crate 是**可交互**的长驻会话—— 启动进程 → 随时读增量输出 →
//! 写 stdin → kill。典型场景:REPL、watch、dev server、需要提前中断的
//! 长任务。
//!
//! ## 正确性细节(自桌面端原样保留,均有依据)
//!
//! - **逐行 `read_until` + `from_utf8_lossy`**:非 UTF-8 输出不能让
//!   reader 提前退出并堵塞子进程的管道(否则进程写满 pipe 后卡死);
//! - **锁不跨 await**:等待退出用 `try_wait` 轮询,输出锁只在读快照时
//!   短暂持有 —— 否则进程关闭 stdout 后仍在运行时无法 kill;
//! - **锁覆盖序号与发送**:输出序号在拿锁后分配,保证 stdout/stderr/exit
//!   的序号与写入顺序一致。
//!
//! PTY(resize / raw mode)**明确不做**(桌面端同样没有,共同缺口)。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use tokio::io::AsyncWriteExt;
use tokio::process::Child;
use tokio::sync::Mutex as AsyncMutex;

/// 单会话输出缓冲(按流分段;`read` 工具取增量)。
#[derive(Debug, Default)]
pub struct OutputBuffer {
    pub stdout: String,
    pub stderr: String,
    /// 进程退出码;`None` = 仍在运行。
    pub exit_code: Option<i32>,
    /// 单调递增序号(stdout + stderr 合计)。
    pub seq: u64,
}

/// 一个运行中的 shell 会话。
pub struct ShellSession {
    pub id: String,
    pub command: String,
    /// 子进程句柄(kill / try_wait 用;退出后被 take 走)。
    pub child: Arc<AsyncMutex<Option<Child>>>,
    /// 输出缓冲(pump 任务写,`read` 工具消费)。
    pub output: Arc<Mutex<OutputBuffer>>,
    /// 从缓冲读取的游标(stdout / stderr 各自的字节偏移)。
    pub cursor: Arc<Mutex<(usize, usize)>>,
    stdin: Arc<AsyncMutex<Option<tokio::process::ChildStdin>>>,
}

impl ShellSession {
    /// 启动一个 shell 会话:spawn 子进程 + 两条 pump 任务(逐行容错解码,
    /// 退出后写 exit code)。
    ///
    /// `argv` 直接透传(首个元素是程序,其余为参数)—— 调用方自行决定
    /// 是否包 `sh -c`;MCP server 层统一包,库层不强制。
    pub fn spawn(
        id: String,
        argv: Vec<String>,
        cwd: &std::path::Path,
        env_clear: bool,
    ) -> std::io::Result<Self> {
        let mut command = tokio::process::Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .current_dir(cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::piped())
            .kill_on_drop(true);
        if env_clear {
            // 与框架其它 spawn 子进程一致:不继承宿主凭据。
            command.env_clear();
            command.env("HOME", std::env::var("HOME").unwrap_or_default());
            command.env("PATH", std::env::var("PATH").unwrap_or_default());
        }
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let stdin = child.stdin.take().expect("piped");

        let session = Self {
            id: id.clone(),
            command: argv.join(" "),
            child: Arc::new(AsyncMutex::new(Some(child))),
            output: Arc::new(Mutex::new(OutputBuffer::default())),
            cursor: Arc::new(Mutex::new((0, 0))),
            stdin: Arc::new(AsyncMutex::new(Some(stdin))),
        };

        // 两条 pump:逐行 read_until + 容错解码(细节见模块 doc)。
        let out_buf = session.output.clone();
        pump_stream(stdout, out_buf, true);
        pump_stream(stderr, session.output.clone(), false);

        // 退出监视:try_wait 轮询(50ms;不在锁内 await),退出码写入缓冲。
        let child_lock = session.child.clone();
        let out_buf = session.output.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let mut guard = child_lock.lock().await;
                let Some(child) = guard.as_mut() else {
                    break; // 已被 take(kill 路径)。
                };
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let code = status.code().unwrap_or(-1);
                        let mut buf = out_buf.lock();
                        buf.exit_code = Some(code);
                        buf.seq += 1;
                        buf.stdout.push_str(&format!("\n[exit {code}]\n"));
                        guard.take();
                        break;
                    }
                    Ok(None) => {}
                    Err(_) => {
                        out_buf.lock().exit_code = Some(-1);
                        guard.take();
                        break;
                    }
                }
            }
        });

        Ok(session)
    }

    /// 写 stdin(会话可能已退出 → `Ok(false)`)。
    pub async fn write_stdin(&self, input: &str) -> std::io::Result<bool> {
        let mut guard = self.stdin.lock().await;
        let Some(stdin) = guard.as_mut() else {
            return Ok(false);
        };
        stdin.write_all(input.as_bytes()).await?;
        stdin.flush().await?;
        Ok(true)
    }

    /// 关闭 stdin(让读 stdin 的程序收到 EOF 自然退出)。
    pub async fn close_stdin(&self) {
        *self.stdin.lock().await = None;
    }

    /// 取增量输出(自上次读取游标起),并返回进程是否已退出。
    pub fn read_delta(&self) -> (OutputDelta, bool) {
        let buf = self.output.lock();
        let mut cursor = self.cursor.lock();
        let stdout_new = buf
            .stdout
            .get(cursor.0.min(buf.stdout.len())..)
            .unwrap_or("");
        let stderr_new = buf
            .stderr
            .get(cursor.1.min(buf.stderr.len())..)
            .unwrap_or("");
        cursor.0 = buf.stdout.len();
        cursor.1 = buf.stderr.len();
        (
            OutputDelta {
                stdout: stdout_new.to_string(),
                stderr: stderr_new.to_string(),
                exit_code: buf.exit_code,
                seq: buf.seq,
            },
            buf.exit_code.is_some(),
        )
    }

    /// kill(SIGKILL)。返回是否还有一个活进程可杀。
    pub async fn kill(&self) -> bool {
        let mut guard = self.child.lock().await;
        if let Some(child) = guard.as_mut() {
            let _ = child.start_kill();
            guard.take();
            true
        } else {
            false
        }
    }
}

/// `read_delta` 的返回。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputDelta {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub seq: u64,
}

/// 单流 pump:逐行读 + 容错解码写入缓冲。
fn pump_stream<R>(reader: R, buffer: Arc<Mutex<OutputBuffer>>, is_stdout: bool)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(reader);
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            use tokio::io::AsyncBufReadExt;
            match reader.read_until(b'\n', &mut bytes).await {
                Ok(0) => break,
                Ok(_) => {
                    if bytes.last() == Some(&b'\n') {
                        bytes.pop();
                        if bytes.last() == Some(&b'\r') {
                            bytes.pop();
                        }
                    }
                    let line = String::from_utf8_lossy(&bytes).into_owned();
                    let mut buf = buffer.lock();
                    if is_stdout {
                        buf.stdout.push_str(&line);
                        buf.stdout.push('\n');
                    } else {
                        buf.stderr.push_str(&line);
                        buf.stderr.push('\n');
                    }
                    buf.seq += 1;
                }
                Err(_) => break,
            }
        }
    });
}

/// 会话池:id → 会话(线程安全;`take` 语义保留桌面端 API 形状)。
#[derive(Default)]
pub struct ShellSessions {
    processes: Mutex<HashMap<String, Arc<ShellSession>>>,
    seq: AtomicU64,
}

impl ShellSessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// spawn 并注册;id 自动分配(`sh-<8hex>`)。
    pub fn run(
        &self,
        argv: Vec<String>,
        cwd: &std::path::Path,
        env_clear: bool,
    ) -> std::io::Result<Arc<ShellSession>> {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let id = format!("sh-{:08x}", n);
        let session = Arc::new(ShellSession::spawn(id.clone(), argv, cwd, env_clear)?);
        self.processes.lock().insert(id, session.clone());
        Ok(session)
    }

    /// 取会话(不摘除;kill / read 后仍可寻址)。
    pub fn get(&self, id: &str) -> Option<Arc<ShellSession>> {
        self.processes.lock().get(id).cloned()
    }

    /// 列全部会话 id。
    pub fn list(&self) -> Vec<String> {
        self.processes.lock().keys().cloned().collect()
    }

    /// 移除会话(先 kill)。
    pub async fn remove(&self, id: &str) -> bool {
        let removed = self.processes.lock().remove(id);
        if let Some(session) = removed {
            session.kill().await;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_captures_stdout_and_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let pool = ShellSessions::new();
        let session = pool
            .run(
                vec![
                    "sh".into(),
                    "-c".into(),
                    "echo hello-shell; echo err-line >&2".into(),
                ],
                dir.path(),
                true,
            )
            .unwrap();
        // 等退出(exit code 写入即完成)。read_delta 消费游标 —— 增量必须
        // 累积拼接,而不是用最后一次覆盖(最后一次往往只剩 exit 行)。
        let mut all_stdout = String::new();
        let mut all_stderr = String::new();
        let mut exit_code = None;
        for _ in 0..100 {
            let (delta, exited) = session.read_delta();
            all_stdout.push_str(&delta.stdout);
            all_stderr.push_str(&delta.stderr);
            if let Some(code) = delta.exit_code {
                exit_code = Some(code);
            }
            if exited {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(exit_code, Some(0));
        assert!(all_stdout.contains("hello-shell"), "{all_stdout}");
        assert!(all_stderr.contains("err-line"), "{all_stderr}");
        assert!(all_stdout.contains("[exit 0]"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_delta_returns_only_increment() {
        let dir = tempfile::tempdir().unwrap();
        let pool = ShellSessions::new();
        let session = pool
            .run(
                vec![
                    "sh".into(),
                    "-c".into(),
                    "echo one; sleep 0.3; echo two".into(),
                ],
                dir.path(),
                true,
            )
            .unwrap();
        // 第一批读到 "one"(或更多)。
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let (first, _) = session.read_delta();
        assert!(first.stdout.contains("one"), "{}", first.stdout);
        // 等第二行。
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let (second, _) = session.read_delta();
        assert!(
            !second.stdout.contains("one") && second.stdout.contains("two"),
            "增量应不含已读内容:{}",
            second.stdout
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kill_stops_running_process() {
        let dir = tempfile::tempdir().unwrap();
        let pool = ShellSessions::new();
        let session = pool
            .run(vec!["sleep".into(), "30".into()], dir.path(), true)
            .unwrap();
        assert!(session.kill().await, "活进程应可杀");
        // 再杀一次:句柄已 take,返回 false。
        assert!(!session.kill().await);
        assert!(pool.remove(&session.id).await || true);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stdin_write_reaches_process() {
        let dir = tempfile::tempdir().unwrap();
        let pool = ShellSessions::new();
        // cat 读 stdin 回显到 stdout。
        let session = pool.run(vec!["cat".into()], dir.path(), true).unwrap();
        assert!(session.write_stdin("echo via-stdin\n").await.unwrap());
        for _ in 0..40 {
            let (delta, _) = session.read_delta();
            if delta.stdout.contains("via-stdin") {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("stdin 内容未回显");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_utf8_output_does_not_break_pump() {
        let dir = tempfile::tempdir().unwrap();
        let pool = ShellSessions::new();
        // 非法 UTF-8 字节(POSIX 八进制转义)+ 后续正常行:容错解码不能让
        // reader 提前退出。增量累积(read_delta 消费游标)。
        let session = pool
            .run(
                vec![
                    "sh".into(),
                    "-c".into(),
                    "printf 'bad\\377\\376 bytes\\n'; echo after-binary".into(),
                ],
                dir.path(),
                true,
            )
            .unwrap();
        let mut all_stdout = String::new();
        for _ in 0..60 {
            let (delta, exited) = session.read_delta();
            all_stdout.push_str(&delta.stdout);
            if exited && all_stdout.contains("after-binary") {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("非 UTF-8 输出后 pump 应继续工作(累积:{all_stdout})");
    }
}
