//! `zhuye-computer-use` — Computer Use 抽象层(WP15 收编 ReflectDesktop
//! `app-core/src/media.rs` 的 Computer Use 半部 + `src-tauri/src/media_backend.rs`
//! 的真实后端)。
//!
//! 工具面:`computer_screenshot` / `computer_click` / `computer_type` /
//! `computer_key` / `computer_scroll`(MCP server 二进制暴露)。
//!
//! ## 优雅降级
//!
//! 平台重依赖(xcap 截屏 + enigo 输入)做成 **`real` feature,默认关**
//! —— workspace check / CI 不拉 GUI 栈。feature 关闭时所有工具统一返回
//! [`MediaError::Unavailable`](isError 文案),**不 panic**;开启后走
//! 真实后端(macOS 需屏幕录制 + 辅助功能权限)。
//!
//! ## 权限
//!
//! MCP 工具天然 `Prompt` 权限下限(`FloorEnforcingTool`),截屏 / 键鼠
//! 输入都走宿主审批,不会被静默放行。

pub mod backend;
pub mod types;

pub use backend::{RealComputerBackend, computer_backend};
pub use types::{BackendCapability, ComputerUseAction, MediaError};
