//! 浏览器控制错误类型(与 computer-use 的 `MediaError` 同形:降级文案
//! 给 LLM 看,含能力名与原因;错误一律可解释,不 panic)。

use thiserror::Error;

/// 浏览器控制错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BrowserError {
    /// 能力不可用(feature 未开 / 浏览器缺失 / 权限不足)。文案给 LLM。
    #[error("browser {capability} unavailable: {reason}")]
    Unavailable {
        /// 能力名(navigate / snapshot / screenshot …)。
        capability: String,
        /// 不可用原因( actionable,给 LLM / 用户看)。
        reason: String,
    },
    /// 其他错误(CDP 协议错误 / 启动失败等)。
    #[error("browser error: {message}")]
    Other {
        /// 错误描述。
        message: String,
    },
}

impl BrowserError {
    /// 构造不可用错误。
    pub fn unavailable(capability: impl Into<String>, reason: impl Into<String>) -> Self {
        BrowserError::Unavailable {
            capability: capability.into(),
            reason: reason.into(),
        }
    }

    /// 构造其他错误。
    pub fn other(message: impl Into<String>) -> Self {
        BrowserError::Other {
            message: message.into(),
        }
    }
}
