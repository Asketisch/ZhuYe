//! 协议层错误类型。通过 `zhuye-llm` 中的 `From` 实现桥接到 `LlmError`
//! (Rust 孤儿规则要求 `impl` 必须写在本地侧)。

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 协议层错误。`Display` 文案由 `#[error]` 固定(稳定,见测试),
/// 可经 serde 序列化跨进程传递。
#[derive(Debug, Error, Serialize, Deserialize)]
pub enum ProtocolError {
    /// 非法的 `Op`(无法识别或参数不合法)。
    #[error("invalid op: {reason}")]
    InvalidOp {
        /// 判定非法的原因。
        reason: String,
    },

    /// 不支持的 LLM provider。
    #[error("unsupported provider: {name}")]
    UnsupportedProvider {
        /// provider 名(如 `"openai"`)。
        name: String,
    },

    /// provider 不支持所请求的模型。
    #[error("unsupported model: {provider}/{model}")]
    UnsupportedModel {
        /// provider 名(如 `"openai"`)。
        provider: String,
        /// 模型名(不含 provider 前缀,如 `"gpt-4o"`)。
        model: String,
    },

    /// 工具执行超时。
    #[error("tool timeout: {tool_name} after {elapsed_ms}ms")]
    ToolTimeout {
        /// 超时的工具名。
        tool_name: String,
        /// 判定超时时已耗时(毫秒)。
        elapsed_ms: u64,
    },

    /// 找不到指定名称的工具(未注册)。
    #[error("tool not found: {tool_name}")]
    ToolNotFound {
        /// 请求的工具名。
        tool_name: String,
    },

    /// hook 拒绝了本次操作。
    #[error("hook rejected: {hook_name} ({reason})")]
    HookRejected {
        /// 作出拒绝的 hook 名。
        hook_name: String,
        /// 拒绝原因。
        reason: String,
    },

    /// LLM 调用失败,携带底层错误的文本描述。
    #[error("llm error: {0}")]
    LlmError(String),

    /// 操作被中断。
    #[error("interrupted")]
    Interrupted,

    /// 线程正在关闭,不再受理新操作。
    #[error("shutdown in progress")]
    ShutdownInProgress,

    /// 兜底的内部错误。
    #[error("internal: {message}")]
    Internal {
        /// 错误描述。
        message: String,
    },

    /// JSON 序列化 / 反序列化失败(由 `serde_json::Error` 转换,携带其文本)。
    #[error("serialization: {0}")]
    Serialization(String),

    /// I/O 错误(由 `std::io::Error` 转换,携带其文本)。
    #[error("io: {0}")]
    Io(String),
}

impl From<serde_json::Error> for ProtocolError {
    fn from(e: serde_json::Error) -> Self {
        ProtocolError::Serialization(e.to_string())
    }
}

impl From<std::io::Error> for ProtocolError {
    fn from(e: std::io::Error) -> Self {
        ProtocolError::Io(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages_are_stable() {
        let e = ProtocolError::InvalidOp {
            reason: "bad".into(),
        };
        assert_eq!(e.to_string(), "invalid op: bad");

        let e = ProtocolError::UnsupportedModel {
            provider: "openai".into(),
            model: "gpt-9".into(),
        };
        assert_eq!(e.to_string(), "unsupported model: openai/gpt-9");
    }
}
