//! `zhuye-browser` — 浏览器控制插件 shim(plugins/browser 的能力本体)。
//!
//! ## 架构(v0.2 起:CDP 页面能力外包给上游 agent-browser)
//!
//! 本 crate 不再自己说 CDP。`zhuye-browser-server` 二进制做三件事:
//!
//! 1. **确保本机浏览器**:先读 `$ZHUYE_HOME/browser-profile/DevToolsActivePort`
//!    attach 已运行实例(与桌面 browser_backend 的跨仓共享契约:谁先启动
//!    谁持有),无实例则探测本机 Chrome/Edge 并以隔离 profile 启动 headless
//!    新实例 —— 「只用本机浏览器、绝不下载浏览器发行版」的原则不变;
//! 2. **定位上游二进制**:`ZHUYE_AGENT_BROWSER` env → 本二进制同目录
//!    (`plugins/browser/bin/agent-browser`,由构建脚本从 submodule 构建)
//!    → PATH(brew/npm 安装兜底);
//! 3. **spawn 上游 MCP 并代理 stdio**:上游经 `AGENT_BROWSER_CDP` env
//!    attach 我们确保好的浏览器实例(上游 daemon 对 cdp 连接失败只报错、
//!    不会回退自行下载/启动浏览器 —— 该行为已对照上游源码确认,这是
//!    「不下载」原则成立的根基)。上游缺失/浏览器不可用时,shim 自答
//!    MCP:工具调用统一返回 isError + 可操作安装提示(**优雅降级,不 panic**)。
//!
//! ## 元素寻址
//!
//! 页面元素寻址(`@eN` 引用 + 快照绑定语义)由上游实现,语义与原 F39
//! 设计同构;`zhuye-computer-use` 的 F39 模型此后仅服务 OS 通道
//! (屏幕/键鼠),browser 通道不再依赖它。
//!
//! ## 权限
//!
//! MCP 工具天然 `Prompt` 权限下限(`FloorEnforcingTool`),浏览器操作
//! 走宿主审批,不会被静默放行。

pub mod chrome;
pub mod error;
pub mod shim;

pub use error::BrowserError;
