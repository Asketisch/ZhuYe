//! 导入错误类型(库 crate 约定:`thiserror`)。
//!
//! 设计原则:错误信息带上下文(源路径 / 会话 id),让 CLI 与 MCP 工具
//! 两侧都能直接把 `to_string()` 展示给用户;解析类失败尽量不走这里
//! (adapter 内部以 `ImportWarning` 记录并跳过,保证单文件可容忍部分
//! 畸形行),只有整体无法进行时才返回错误。

use thiserror::Error;

/// 导入流程的全部错误面。
#[derive(Debug, Error)]
pub enum ImportError {
    /// 文件 / 目录 / SQLite 拷贝等 IO 失败。
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    /// SQLite 侧错误(打开、查询、schema 不符)。
    #[error("SQLite 错误: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// JSONL 单文件整体无法解析(逐行畸形以 warning 跳过,不走到这里)。
    #[error("JSON 解析失败({path}): {detail}")]
    Json {
        /// 出错的源文件路径(展示用)。
        path: String,
        /// 具体解析错误说明。
        detail: String,
    },

    /// 无法识别为任何已知 Harness 的会话文件(`--from auto` 嗅探失败)。
    #[error("无法识别的会话格式: {0}")]
    UnknownFormat(String),

    /// 目标 thread_id 已存在于本地 session 存储(未加 `--force`)。
    #[error("目标会话已存在: {0}(加 --force 覆盖)")]
    AlreadyExists(String),

    /// 解析成功但没有任何可导入的对话内容(空会话 / 全是注入与审计行)。
    #[error("空会话({0}):没有可导入的对话内容")]
    EmptySession(String),

    /// 源会话 id 无法映射为合法 UUID 且未提供替代(ZCode 场景为主)。
    #[error("无效的会话标识: {0}")]
    InvalidSessionId(String),

    /// 其余带上下文的错误(MCP 侧参数缺失等)。
    #[error("{0}")]
    Other(String),
}
