//! PostgreSQL 会话持久化 —— `RolloutRecorder` 的 DB 后端 trait。
//!
//! 具体 `sqlx` / `tokio-postgres` 实现留 v2.x;此处定义 contract + stub,
//! 让 exec / CLI 可以编译期注入后端。

use async_trait::async_trait;
use zhuye_protocol::{RolloutError, RolloutRecord, RolloutRecorder, SessionInfo, ThreadId};

/// PostgreSQL 会话存储操作错误(当前仅 stub 实现,真实 DB 后端留 v2.x)。
#[derive(Debug, thiserror::Error)]
pub enum PostgresSessionError {
    /// 未配置 `database_url`(stub 实现一律返回本错误)。
    #[error("postgres_session stub: 未配置 database_url")]
    NotConfigured,
}

/// PostgreSQL 连接配置(对应 TOML `[postgres_session]` 段)。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PostgresSessionConfig {
    /// 连接 URL,如 `postgres://user:pass@localhost/zhuye`。
    pub database_url: String,
    /// 表名前缀,默认 `zhuye_`。
    #[serde(default = "default_table_prefix")]
    pub table_prefix: String,
    /// 连接池大小上限。
    #[serde(default = "default_pool_size")]
    pub pool_size: u32,
}

fn default_table_prefix() -> String {
    "zhuye_".into()
}

fn default_pool_size() -> u32 {
    4
}

impl PostgresSessionConfig {
    pub fn sessions_table(&self) -> String {
        format!("{}sessions", self.table_prefix)
    }

    pub fn records_table(&self) -> String {
        format!("{}rollout_records", self.table_prefix)
    }
}

/// PostgreSQL 会话存储 trait —— 扩展 `RolloutRecorder` 加 checkpoint 语义。
#[async_trait]
pub trait PostgresSessionStore: RolloutRecorder {
    /// 健康检查( ping / SELECT 1 )。
    async fn health_check(&self) -> Result<(), PostgresSessionError>;

    /// 按 session 删除所有记录(checkpoint purge)。
    async fn delete_session(&self, session_id: ThreadId) -> Result<(), PostgresSessionError>;
}

/// 未配置 DB 时的 stub —— 所有写操作 no-op,读返回空。
#[derive(Debug, Default, Clone, Copy)]
pub struct StubPostgresSessionStore;

#[async_trait]
impl RolloutRecorder for StubPostgresSessionStore {
    async fn record(&self, _r: RolloutRecord) -> Result<(), RolloutError> {
        Ok(())
    }

    async fn replay(&self, _session_id: ThreadId) -> Result<Vec<RolloutRecord>, RolloutError> {
        Ok(Vec::new())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>, RolloutError> {
        Ok(Vec::new())
    }

    async fn truncate_after(
        &self,
        _to_turn_id: Option<&zhuye_protocol::TurnId>,
    ) -> Result<usize, RolloutError> {
        Ok(0)
    }
}

#[async_trait]
impl PostgresSessionStore for StubPostgresSessionStore {
    async fn health_check(&self) -> Result<(), PostgresSessionError> {
        Err(PostgresSessionError::NotConfigured)
    }

    async fn delete_session(&self, _session_id: ThreadId) -> Result<(), PostgresSessionError> {
        Ok(())
    }
}

/// TUI / CLI 状态行。
pub fn status_line(config: Option<&PostgresSessionConfig>) -> String {
    match config {
        Some(c) if !c.database_url.is_empty() => {
            "postgres-session: trait 就绪 — URL 已配置(真实 sqlx 实现留 v2.x)".into()
        }
        _ => "postgres-session: stub — 配置 [postgres_session].database_url 启用".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stub_recorder_is_noop() {
        let s = StubPostgresSessionStore;
        let sid = ThreadId::new();
        s.record(RolloutRecord::session_meta(sid, "m"))
            .await
            .unwrap();
        assert!(s.replay(sid).await.unwrap().is_empty());
    }

    #[test]
    fn table_names() {
        let c = PostgresSessionConfig {
            database_url: "postgres://localhost/db".into(),
            table_prefix: "r_".into(),
            pool_size: 2,
        };
        assert_eq!(c.sessions_table(), "r_sessions");
        assert_eq!(c.records_table(), "r_rollout_records");
    }
}
