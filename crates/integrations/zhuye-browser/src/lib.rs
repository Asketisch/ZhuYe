//! `zhuye-browser` — 浏览器控制抽象层(F39;plugins/browser 的能力本体)。
//!
//! 工具面:`browser_navigate` / `browser_snapshot` / `browser_click` /
//! `browser_type` / `browser_press_key` / `browser_scroll` /
//! `browser_screenshot` / `browser_close`(MCP server 二进制暴露)。
//!
//! ## 控制通道
//!
//! Chrome DevTools Protocol(CDP)over **loopback** WebSocket:
//! - 被控端是本机 Chrome/Edge/Chromium(支持 headless 新模式),以隔离
//!   profile(`$ZHUYE_HOME/browser-profile`)启动,绝不触碰用户主 profile;
//! - 连接发现:`DevToolsActivePort` 文件(profile 目录,先到先得 —— 桌面
//!   browser_backend 或本插件谁先启动浏览器谁持有,后到方 attach);
//! - 安全约束复用 [`zhuye_computer_use::BrowserSecurity`]:仅回环 +
//!   单 target 强制启用;`require_token` **v1 不启用**(原生 CDP 无鉴权
//!   机制,隔离 profile + loopback 是当前边界;token 代理为后续项)。
//!
//! ## 元素寻址(F39)
//!
//! 复用 `zhuye-computer-use::a11y` 的寻址模型:快照 → `@eN` 引用 →
//! 快照绑定(旧 ref 显式失效)→ 动作后验。CDP `Accessibility.getFullAXTree`
//! 的 AX 节点映射为 [`zhuye_computer_use::A11ySnapshot`]。
//!
//! ## 优雅降级
//!
//! CDP 客户端(ws 栈)做成 **`real` feature,默认关** —— workspace
//! check / CI 不拉网络栈。feature 关闭时所有工具统一返回
//! [`BrowserError::Unavailable`](isError 文案),**不 panic**。
//!
//! ## 权限
//!
//! MCP 工具天然 `Prompt` 权限下限(`FloorEnforcingTool`),浏览器操作
//! 走宿主审批,不会被静默放行。

pub mod backend;
pub mod error;

pub use backend::{BrowserBackend, BrowserCapability, BrowserStatus, browser_backend};
pub use error::BrowserError;

/// CDP 客户端与浏览器启动器(`real` feature;关闭时不编译)。
#[cfg(feature = "real")]
pub mod cdp;
/// 浏览器发现 / 启动 / attach(`real` feature;关闭时不编译)。
#[cfg(feature = "real")]
pub mod chrome;

#[cfg(feature = "real")]
pub use backend::CdpBrowserBackend;
