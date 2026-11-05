//! # zhuye-audit — 审计日志(v1.9 S5)
//!
//! 与 `zhuye-telemetry`(可观测性)不同,本 crate 提供**审计语义**:
//! "谁在什么时候对什么状态做了什么改动"必须留下不可绕过的记录。
//! 对标 PI-Desktop `host-core/src/audit.rs` 与 openhuman
//! `security/{audit,audit_record,scrub}.rs`。
//!
//! ## 三个核心性质
//!
//! 1. **与状态变更同事务提交**:[`AuditLog::audit_and_commit`] 把
//!    "写审计记录"与"执行状态变更"绑定在一个调用里 —— 状态变更的
//!    成败写进审计结果,且**审计写失败 = 整体失败**(fail-closed:
//!    不允许出现"改了状态但没有审计"的路径;调用方应把该错误视为
//!    致命并回滚/终止)。
//!    诚实说明:JSONL 追加与任意宿主状态之间没有内核级事务,
//!    "同事务"靠的是**提交顺序绑定 + 失败即报错**的调用约定,
//!    不假装有跨介质原子性。
//! 2. **递归脱敏**:detail 是任意 JSON;落盘前对全部字符串叶子过
//!    `zhuye-sanitize` 密钥 pattern,并对键名敏感
//!    (`token` / `secret` / `key` / `password` / `credential` /
//!    `authorization` 等)的值整段替换 —— 深层嵌套也逃不掉。
//! 3. **指纹**:每条记录带 `fingerprint` = **脱敏前**原始 detail 的
//!    SHA-256。同一命令/参数在两条记录里指纹一致,审计可以关联
//!    "相同操作发生了 N 次"而无需保存原文;与
//!    `zhuye-sanitize::prompt_guard::fingerprint` 同算法。
//!
//! ## 存储
//!
//! 单文件 JSONL(audit.jsonl),每行一条 [`AuditRecord`];`seq`
//! 单调递增,进程重启及多进程并发写入时在文件锁内续号。
//! 初次打开会检查历史,并按既有最大序号和记录数量恢复基线。
//!
//! ## 抽象
//!
//! [`AuditSink`] trait 把"写到哪"与"记什么"解耦:文件 sink 是默认,
//! 测试/内存场景用 `Vec` sink;fail-closed 语义在 trait 层面成立
//! (sink `Err` → 提交失败)。

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use fs2::FileExt;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 审计事件类别。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    /// 工具调用(bash/write/edit …)。
    ToolCall,
    /// 审批裁决(allow/deny/expire)。
    Approval,
    /// 权限规则变更(增删改 permission 规则)。
    PermissionChange,
    /// 配置变更。
    ConfigChange,
    /// 密钥访问(读取/写入/删除加密密钥)。
    SecretAccess,
    /// 沙箱/闸门裁决(不可信来源命令的 Force/Reject)。
    SandboxDecision,
    /// 会话生命周期(创建/恢复/退役)。
    SessionLifecycle,
    /// 宿主自定义类别。
    Custom(String),
}

/// 审计结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    /// 请求已发出(等待裁决/执行)。
    Requested,
    /// 已放行/批准。
    Allowed,
    /// 已拒绝。
    Denied,
    /// 已超时过期(S6)。
    Expired,
    /// 已执行成功。
    Executed,
    /// 执行失败。
    Failed,
}

/// 调用方提交的一条审计事实(未脱敏;由 [`AuditLog`] 负责脱敏)。
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub kind: AuditKind,
    /// 主语:工具名 / 规则 id / 密钥名 / 会话 id 等。
    pub subject: String,
    pub outcome: AuditOutcome,
    /// 任意 JSON detail(将递归脱敏;指纹按脱敏前内容计算)。
    pub detail: serde_json::Value,
}

/// 落盘的一条审计记录(已脱敏 + 指纹 + 序号 + 时间戳)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    /// 单调递增序号(跨进程续号)。
    pub seq: u64,
    /// RFC 3339 UTC 时间戳。
    pub ts: String,
    pub kind: AuditKind,
    pub subject: String,
    pub outcome: AuditOutcome,
    /// 已递归脱敏的 detail。
    pub detail: serde_json::Value,
    /// 脱敏前原始 detail 的 SHA-256(小写 hex)。
    pub fingerprint: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// fail-closed:状态变更成功但审计写失败 —— 调用方必须当作整体
    /// 失败处理(回滚/终止),不允许"无审计的状态变更"存活。
    #[error("audit write failed after state mutation (fail-closed): {0}")]
    WriteFailed(String),
}

/// 审计落点抽象。
pub trait AuditSink: Send + Sync {
    /// 追加一条记录(JSONL 单行)。实现必须把多次写串行化。
    fn append(&self, line: &str) -> Result<(), AuditError>;
}

/// 文件 sink:audit.jsonl 追加写。
pub struct FileSink {
    file: Mutex<std::fs::File>,
}

impl FileSink {
    /// 打开(不存在则创建)追加句柄。
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<FileSink> {
        Self::open_inner(path.as_ref(), false)
    }

    fn open_inner(path: &Path, read: bool) -> std::io::Result<FileSink> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(read)
            .append(true)
            .open(path)?;
        Ok(FileSink {
            file: Mutex::new(file),
        })
    }

    // 同一锁覆盖读序号和追加,不同进程也必须按同一顺序操作。
    fn with_lock<T>(
        &self,
        f: impl FnOnce(&mut std::fs::File) -> Result<T, AuditError>,
    ) -> Result<T, AuditError> {
        let mut file = self.file.lock();
        FileExt::lock_exclusive(&*file)?;
        let result = f(&mut file);
        let unlock = FileExt::unlock(&*file).map_err(AuditError::from);
        match (result, unlock) {
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
            (Ok(value), Ok(())) => Ok(value),
        }
    }

    // 检查历史并兼容旧版本留下的序号重复或不连续。
    fn baseline(&self) -> Result<u64, AuditError> {
        self.with_lock(|file| {
            file.seek(SeekFrom::Start(0))?;
            let mut reader = BufReader::new(file);
            let mut line = String::new();
            let mut count = 0u64;
            let mut max_seq = 0u64;
            loop {
                line.clear();
                if reader.read_line(&mut line)? == 0 {
                    break;
                }
                if !line.ends_with('\n') {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "审计记录末尾不完整",
                    )
                    .into());
                }
                if !line.trim().is_empty() {
                    let record: AuditRecord = serde_json::from_str(&line)?;
                    max_seq = max_seq.max(record.seq);
                    count = count
                        .checked_add(1)
                        .ok_or_else(|| std::io::Error::other("审计序号溢出"))?;
                }
            }
            Ok(max_seq.max(count))
        })
    }

    // 从文件末尾向前读取最后一条完整记录,避免每次写入都扫描全文件。
    fn last_seq(file: &mut std::fs::File) -> Result<u64, AuditError> {
        let len = file.metadata()?.len();
        if len == 0 {
            return Ok(0);
        }
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            return Err(
                std::io::Error::new(std::io::ErrorKind::InvalidData, "审计记录末尾不完整").into(),
            );
        }
        let mut end = len - 1;
        // 旧版允许空行;跳过尾部空白,再定位最后一条实际记录。
        while end > 0 {
            file.seek(SeekFrom::Start(end - 1))?;
            file.read_exact(&mut last)?;
            if !last[0].is_ascii_whitespace() {
                break;
            }
            end -= 1;
        }
        if end == 0 {
            return Ok(0);
        }
        let mut chunks = Vec::new();
        while end > 0 {
            let start = end.saturating_sub(8192);
            let mut chunk = vec![0; (end - start) as usize];
            file.seek(SeekFrom::Start(start))?;
            file.read_exact(&mut chunk)?;
            if let Some(index) = chunk.iter().rposition(|b| *b == b'\n') {
                chunks.push(chunk[index + 1..].to_vec());
                break;
            }
            chunks.push(chunk);
            end = start;
        }
        chunks.reverse();
        let bytes: Vec<u8> = chunks.into_iter().flatten().collect();
        if bytes.is_empty() {
            return Err(
                std::io::Error::new(std::io::ErrorKind::InvalidData, "审计记录末行为空").into(),
            );
        }
        let record: AuditRecord = serde_json::from_slice(&bytes)?;
        Ok(record.seq)
    }

    fn append_sequenced(&self, record: &mut AuditRecord, baseline: u64) -> Result<(), AuditError> {
        self.with_lock(|file| {
            record.seq = Self::last_seq(file)?
                .max(baseline)
                .checked_add(1)
                .ok_or_else(|| std::io::Error::other("审计序号溢出"))?;
            let mut line = serde_json::to_vec(record)?;
            line.push(b'\n');
            file.write_all(&line)?;
            file.flush()?;
            Ok(())
        })
    }
}

impl AuditSink for FileSink {
    fn append(&self, line: &str) -> Result<(), AuditError> {
        self.with_lock(|file| {
            let mut bytes = Vec::with_capacity(line.len() + 1);
            bytes.extend_from_slice(line.as_bytes());
            bytes.push(b'\n');
            file.write_all(&bytes)?;
            file.flush()?;
            Ok(())
        })
    }
}

/// 测试/内存 sink。
#[derive(Default)]
pub struct VecSink {
    lines: Mutex<Vec<String>>,
}

impl AuditSink for VecSink {
    fn append(&self, line: &str) -> Result<(), AuditError> {
        self.lines.lock().push(line.to_string());
        Ok(())
    }
}

impl VecSink {
    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().clone()
    }
}

/// 恒失败 sink(fail-closed 语义测试用)。
pub struct FailingSink;

impl AuditSink for FailingSink {
    fn append(&self, _line: &str) -> Result<(), AuditError> {
        Err(AuditError::WriteFailed("injected sink failure".into()))
    }
}

/// 敏感键名(小写匹配,包含即整值替换)。与 zhuye-sanitize 的
/// 密钥 pattern 互补:pattern 抓值形态,键名抓"位置"。
const SENSITIVE_KEY_SUBSTRINGS: [&str; 9] = [
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "api_key",
    "apikey",
    "authorization",
    "private_key",
];

/// 递归脱敏一个 JSON 值:
/// - 对象:键名含敏感子串 → 值整段替换为 `[REDACTED:key]`;否则递归;
/// - 数组:逐元素递归;
/// - 字符串叶子:过 `zhuye-sanitize::redact_string`(10 类密钥 pattern);
/// - 其余原样。
pub fn redact_value(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, val) in map {
                let key_lower = k.to_lowercase();
                if SENSITIVE_KEY_SUBSTRINGS
                    .iter()
                    .any(|s| key_lower.contains(s))
                {
                    out.insert(
                        k.clone(),
                        serde_json::Value::String("[REDACTED:key]".into()),
                    );
                } else {
                    out.insert(k.clone(), redact_value(val));
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(redact_value).collect())
        }
        serde_json::Value::String(s) => serde_json::Value::String(zhuye_sanitize::redact_string(s)),
        other => other.clone(),
    }
}

/// SHA-256 小写 hex(与 prompt_guard::fingerprint 同算法)。
pub fn fingerprint(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// 审计日志。通过 [`AuditLog::open`] 打开的文件在进程间同步序号;
/// 自定义 sink 的序号在当前实例内递增。
pub struct AuditLog {
    sink: std::sync::Arc<dyn AuditSink>,
    file_sink: Option<std::sync::Arc<FileSink>>,
    seq: Mutex<u64>,
}

impl AuditLog {
    /// 内存 sink 构造(序号从 1 起)。测试 / 嵌入方自定义存储用。
    pub fn new(sink: std::sync::Arc<dyn AuditSink>) -> AuditLog {
        AuditLog {
            sink,
            file_sink: None,
            seq: Mutex::new(0),
        }
    }

    /// 打开 `<dir>/audit.jsonl`,检查历史并跨进程同步序号。
    /// 目录不存在则创建;损坏或不完整的历史返回错误。
    pub fn open(dir: impl AsRef<Path>) -> std::io::Result<AuditLog> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;
        let path = dir.join("audit.jsonl");
        let sink = std::sync::Arc::new(FileSink::open_inner(&path, true)?);
        let baseline = sink.baseline().map_err(|error| match error {
            AuditError::Io(error) => error,
            error => std::io::Error::new(std::io::ErrorKind::InvalidData, error),
        })?;
        Ok(AuditLog {
            sink: sink.clone(),
            file_sink: Some(sink),
            seq: Mutex::new(baseline),
        })
    }

    /// 记录一条审计(独立提交,不经状态变更绑定)。
    pub fn record(&self, entry: AuditEntry) -> Result<AuditRecord, AuditError> {
        let raw = serde_json::to_string(&entry.detail)?;
        let fingerprint = fingerprint(raw.as_bytes());
        let detail = redact_value(&entry.detail);
        let mut seq = self.seq.lock();
        let mut rec = AuditRecord {
            seq: seq
                .checked_add(1)
                .ok_or_else(|| std::io::Error::other("审计序号溢出"))?,
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            kind: entry.kind,
            subject: entry.subject,
            outcome: entry.outcome,
            detail,
            fingerprint,
        };
        if let Some(sink) = &self.file_sink {
            sink.append_sequenced(&mut rec, *seq)?;
        } else {
            let line = serde_json::to_string(&rec)?;
            self.sink.append(line.as_str())?;
        }
        *seq = rec.seq;
        Ok(rec)
    }

    /// **同事务提交**:先执行状态变更 `mutate`,按其成败写审计
    /// (Executed / Failed);审计写失败时返回
    /// [`AuditError::WriteFailed`] —— 即使状态变更已成功
    /// (fail-closed:调用方必须把该错误当作致命,回滚或终止)。
    ///
    /// 审计记录在 `mutate` **之后**写:审计要记的是"这次变更的最终
    /// 结果",而"变更发生了但审计失败"的窗口由返回错误兜住。
    pub fn audit_and_commit<T, E>(
        &self,
        entry: AuditEntry,
        mutate: impl FnOnce() -> Result<T, E>,
    ) -> Result<(T, AuditRecord), CommitError<E>> {
        match mutate() {
            Ok(v) => {
                let mut committed = entry;
                committed.outcome = AuditOutcome::Executed;
                match self.record(committed) {
                    Ok(rec) => Ok((v, rec)),
                    // 状态已变更但审计没写成 —— fail-closed 上抛。
                    Err(e) => Err(CommitError::AuditFailedAfterMutation(e)),
                }
            }
            Err(e) => {
                let mut committed = entry;
                committed.outcome = AuditOutcome::Failed;
                // 审计失败状态变更也要尽力留痕;留痕再失败只能返回
                // AuditFailed(状态本身没变,无审计缺口)。
                match self.record(committed) {
                    Ok(rec) => Err(CommitError::Mutation(Box::new(MutationFailure {
                        error: e,
                        audit_record: Some(rec),
                    }))),
                    Err(ae) => Err(CommitError::AuditFailedAfterMutation(ae)),
                }
            }
        }
    }
}

/// [`CommitError::Mutation`] 携带的失败详情。
#[derive(Debug, thiserror::Error)]
#[error("state mutation failed: {error:?}")]
pub struct MutationFailure<E> {
    pub error: E,
    /// 失败留痕记录(Some = 已写入审计)。
    pub audit_record: Option<AuditRecord>,
}

/// [`AuditLog::audit_and_commit`] 的错误。
#[derive(Debug, thiserror::Error)]
pub enum CommitError<E> {
    /// 状态变更失败(携带原错误 + 失败留痕记录)。整体盒装:错误路径
    /// 非热路径,避免 Result<_, CommitError<E>> 整体膨胀
    /// (clippy result_large_err)。
    #[error("state mutation failed: {0:?}")]
    Mutation(Box<MutationFailure<E>>),
    /// 状态变更成功但审计写失败(fail-closed:调用方必须回滚/终止)。
    #[error(transparent)]
    AuditFailedAfterMutation(AuditError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mem_log() -> (AuditLog, std::sync::Arc<VecSink>) {
        let sink = std::sync::Arc::new(VecSink::default());
        let log = AuditLog::new(sink.clone());
        (log, sink)
    }

    fn sample_entry(detail: serde_json::Value) -> AuditEntry {
        AuditEntry {
            kind: AuditKind::ToolCall,
            subject: "bash".into(),
            outcome: AuditOutcome::Requested,
            detail,
        }
    }

    #[test]
    fn record_writes_jsonl_with_increasing_seq() {
        let (log, sink) = mem_log();
        let r1 = log.record(sample_entry(json!({"cmd": "ls"}))).unwrap();
        let r2 = log.record(sample_entry(json!({"cmd": "pwd"}))).unwrap();
        assert_eq!(r1.seq, 1);
        assert_eq!(r2.seq, 2);
        assert_eq!(sink.lines().len(), 2);
        // 每行是合法 JSON 且能反序列化回 AuditRecord。
        let back: AuditRecord = serde_json::from_str(&sink.lines()[0]).unwrap();
        assert_eq!(back.seq, 1);
        assert_eq!(back.subject, "bash");
        assert_eq!(back.kind, AuditKind::ToolCall);
    }

    #[test]
    fn ts_is_rfc3339_utc() {
        let (log, _sink) = mem_log();
        let r = log.record(sample_entry(json!({}))).unwrap();
        // chrono 解析回 UTC 时间即证明格式合法(偏移为零 + Z 后缀)。
        let t = chrono::DateTime::parse_from_rfc3339(&r.ts).unwrap();
        assert_eq!(t.offset().local_minus_utc(), 0, "必须是 UTC(偏移 0)");
        assert!(r.ts.ends_with('Z'));
    }

    #[test]
    fn recursive_redaction_nested() {
        let (log, sink) = mem_log();
        let detail = json!({
            "cmd": "export OPENAI_KEY=sk-abcdefghijklmnopqrstuv && curl -H 'Authorization: Bearer abc.def.ghi' https://x",
            "env": {
                "NESTED_TOKEN": "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
                "safe": "plain value",
                "deeper": [ "AKIAIOSFODNN7EXAMPLE", {"password": "hunter2"} ]
            }
        });
        let rec = log.record(sample_entry(detail)).unwrap();
        let line = &sink.lines()[0];
        // 密钥字面量不得出现在落盘记录。
        for secret in [
            "sk-abcdefghijklmnopqrstuv",
            "ghp_abcdefghijklmnopqrstuvwxyz",
            "AKIAIOSFODNN7EXAMPLE",
            "hunter2",
        ] {
            assert!(!line.contains(secret), "泄漏 {secret}: {line}");
        }
        // 敏感键的值整段替换。
        assert_eq!(rec.detail["env"]["NESTED_TOKEN"], "[REDACTED:key]");
        assert_eq!(rec.detail["env"]["deeper"][1]["password"], "[REDACTED:key]");
        // 安全值保留。
        assert_eq!(rec.detail["env"]["safe"], "plain value");
    }

    #[test]
    fn fingerprint_is_of_raw_detail_and_stable() {
        let (log, _sink) = mem_log();
        let detail = json!({"cmd": "echo sk-abcdefghijklmnopqrstuv"});
        let rec = log.record(sample_entry(detail.clone())).unwrap();
        let raw = serde_json::to_string(&detail).unwrap();
        assert_eq!(rec.fingerprint, fingerprint(raw.as_bytes()));
        // 脱敏后 detail 与指纹不同源(指纹不随脱敏变化)。
        let rec2 = log.record(sample_entry(detail)).unwrap();
        assert_eq!(rec.fingerprint, rec2.fingerprint, "同内容同指纹");
        let rec3 = log.record(sample_entry(json!({"cmd": "other"}))).unwrap();
        assert_ne!(rec.fingerprint, rec3.fingerprint);
    }

    #[test]
    fn fingerprint_algorithm_locked() {
        // 锁算法:SHA-256("audit") 已知值(shell 校验过),防止被悄悄换。
        assert_eq!(
            fingerprint(b"audit"),
            "b81f37a043a6f767e7c94d105f4bd31282f3ecc20680bb9d09bd93461cf4c863"
        );
    }

    #[test]
    fn audit_and_commit_success_records_executed() {
        let (log, sink) = mem_log();
        let mut state = 0_i32;
        let (v, rec) = log
            .audit_and_commit(sample_entry(json!({"op": "inc"})), || {
                state += 1;
                Ok::<_, std::convert::Infallible>(state)
            })
            .unwrap();
        assert_eq!(v, 1);
        assert_eq!(state, 1);
        assert_eq!(rec.outcome, AuditOutcome::Executed);
        let line = &sink.lines()[0];
        assert!(line.contains("\"executed\""));
    }

    #[test]
    fn audit_and_commit_failure_records_failed_and_propagates() {
        let (log, sink) = mem_log();
        let mut state = 0_i32;
        let res = log.audit_and_commit(sample_entry(json!({"op": "boom"})), || {
            // 变更先做一半,再失败 —— 审计必须记 Failed。
            state += 5;
            Err::<i32, _>("boom")
        });
        match res {
            Err(CommitError::Mutation(failure)) => {
                assert_eq!(failure.error, "boom");
                let rec = failure.audit_record.expect("失败留痕应已写入");
                assert_eq!(rec.outcome, AuditOutcome::Failed);
            }
            other => panic!("expected Mutation failure with record, got {other:?}"),
        }
        let line = &sink.lines()[0];
        assert!(line.contains("\"failed\""));
        let _ = state;
    }

    #[test]
    fn audit_write_failure_after_mutation_is_fatal() {
        // fail-closed 核心:状态变更成功但审计写失败 → 返回
        // AuditFailedAfterMutation,调用方必须回滚。
        let log = AuditLog::new(std::sync::Arc::new(FailingSink));
        let mut state = 0_i32;
        let res = log.audit_and_commit(sample_entry(json!({})), || {
            state += 1;
            Ok::<_, std::convert::Infallible>(state)
        });
        assert!(matches!(
            res,
            Err(CommitError::AuditFailedAfterMutation(
                AuditError::WriteFailed(_)
            ))
        ));
        // 状态确实变了 —— 错误语义就是"你必须回滚我"。
        assert_eq!(state, 1);
    }

    #[test]
    fn audit_write_failure_on_failed_mutation_still_errors() {
        let log = AuditLog::new(std::sync::Arc::new(FailingSink));
        let res = log.audit_and_commit(sample_entry(json!({})), || Err::<(), _>("x"));
        assert!(matches!(res, Err(CommitError::AuditFailedAfterMutation(_))));
    }

    #[test]
    fn concurrent_records_have_unique_seq() {
        let sink = std::sync::Arc::new(VecSink::default());
        let log = std::sync::Arc::new(AuditLog::new(sink.clone() as std::sync::Arc<dyn AuditSink>));
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let log = log.clone();
                std::thread::spawn(move || log.record(sample_entry(json!({"i": i}))).unwrap().seq)
            })
            .collect();
        let mut seqs: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        seqs.sort();
        let unique: std::collections::HashSet<u64> = seqs.iter().copied().collect();
        assert_eq!(unique.len(), 16, "seq 必须全唯一");
        assert_eq!(seqs[0], 1);
        assert_eq!(seqs[15], 16);
        assert_eq!(sink.lines().len(), 16);
    }

    #[test]
    fn file_sink_persists_and_resumes_seq() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        {
            let log = AuditLog::new(std::sync::Arc::new(FileSink::open(&path).unwrap())
                as std::sync::Arc<dyn AuditSink>);
            log.record(sample_entry(json!({"a": 1}))).unwrap();
            log.record(sample_entry(json!({"a": 2}))).unwrap();
        }
        // 重新 open:序号续号。
        let log =
            AuditLog::new(std::sync::Arc::new(FileSink::open(&path).unwrap())
                as std::sync::Arc<dyn AuditSink>);
        // 直接 new(sink 注入)的序号基线是 0 —— 续号语义由 open 承担
        // (见 audit_log_open_resumes_seq_from_line_count)。
        let rec = log.record(sample_entry(json!({"a": 3}))).unwrap();
        assert_eq!(rec.seq, 1);
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 3);
    }

    #[test]
    fn audit_log_open_resumes_seq_from_line_count() {
        let dir = tempfile::tempdir().unwrap();
        {
            let log = AuditLog::open(dir.path()).unwrap();
            log.record(sample_entry(json!({"n": 1}))).unwrap();
            log.record(sample_entry(json!({"n": 2}))).unwrap();
            log.record(sample_entry(json!({"n": 3}))).unwrap();
        }
        let log = AuditLog::open(dir.path()).unwrap();
        let rec = log.record(sample_entry(json!({"n": 4}))).unwrap();
        assert_eq!(rec.seq, 4, "序号必须跨进程续(行数基线)");
        let content = std::fs::read_to_string(dir.path().join("audit.jsonl")).unwrap();
        assert_eq!(content.lines().count(), 4);
    }

    #[test]
    fn audit_writer_process() {
        let Some(dir) = std::env::var_os("ZHUYE_AUDIT_TEST_DIR") else {
            return;
        };
        let log = AuditLog::open(dir).unwrap();
        for index in 0..40 {
            log.record(sample_entry(
                json!({"index": index, "padding": "x".repeat(12000)}),
            ))
            .unwrap();
        }
    }

    #[test]
    fn concurrent_processes_append_complete_records_in_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut children: Vec<_> = (0..4)
            .map(|_| {
                std::process::Command::new(&executable)
                    .args(["--exact", "tests::audit_writer_process", "--nocapture"])
                    .env("ZHUYE_AUDIT_TEST_DIR", dir.path())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        let text = std::fs::read_to_string(dir.path().join("audit.jsonl")).unwrap();
        let records: Vec<AuditRecord> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 160);
        assert!(
            records
                .iter()
                .enumerate()
                .all(|(i, record)| record.seq == i as u64 + 1)
        );
        assert!(
            records
                .iter()
                .all(|record| record.detail["padding"].as_str().unwrap().len() == 12000)
        );
    }

    #[test]
    fn incomplete_audit_file_is_not_overwritten_or_extended() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let original = b"{\"seq\":1";
        std::fs::write(&path, original).unwrap();
        assert!(AuditLog::open(dir.path()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    fn old_nonmonotonic_history_uses_highest_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::new(std::sync::Arc::new(VecSink::default()));
        let mut record = log.record(sample_entry(json!({}))).unwrap();
        let path = dir.path().join("audit.jsonl");
        record.seq = 15;
        let first = serde_json::to_string(&record).unwrap();
        record.seq = 2;
        let second = serde_json::to_string(&record).unwrap();
        std::fs::write(&path, format!("{first}\n{second}\n")).unwrap();
        let opened = AuditLog::open(dir.path()).unwrap();
        assert_eq!(opened.record(sample_entry(json!({}))).unwrap().seq, 16);
    }

    #[test]
    fn legacy_blank_lines_do_not_prevent_appending() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(dir.path()).unwrap();
        assert_eq!(log.record(sample_entry(json!({}))).unwrap().seq, 1);
        let path = dir.path().join("audit.jsonl");
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(b"  \n\n").unwrap();
        drop(file);
        assert_eq!(log.record(sample_entry(json!({}))).unwrap().seq, 2);
    }

    #[test]
    fn concurrent_memory_sink_records_follow_sequence_order() {
        let (log, sink) = mem_log();
        let log = std::sync::Arc::new(log);
        std::thread::scope(|scope| {
            for index in 0..100 {
                let log = log.clone();
                scope.spawn(move || {
                    log.record(sample_entry(json!({"index": index}))).unwrap();
                });
            }
        });
        let records: Vec<AuditRecord> = sink
            .lines()
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            records
                .iter()
                .enumerate()
                .all(|(i, record)| record.seq == i as u64 + 1)
        );
    }

    #[test]
    fn kind_and_outcome_survive_serde_roundtrip() {
        let (log, sink) = mem_log();
        let entry = AuditEntry {
            kind: AuditKind::SandboxDecision,
            subject: "untrusted-gate".into(),
            outcome: AuditOutcome::Denied,
            detail: json!({"origin": "model"}),
        };
        let _ = log.record(entry).unwrap();
        let back: AuditRecord = serde_json::from_str(&sink.lines()[0]).unwrap();
        assert_eq!(back.kind, AuditKind::SandboxDecision);
        assert_eq!(back.outcome, AuditOutcome::Denied);
    }
}
