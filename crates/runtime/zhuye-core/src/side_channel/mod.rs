//! Side-channel agent(详见 [`registry`] / [`driver`] 模块文档)。

pub mod driver;
pub mod registry;

pub use driver::{SideChannelDriver, SideChannelStartError, SideThreadFactory};
pub use registry::{
    MAX_CONCURRENT, SideChannelEvent, SideChannelId, SideChannelInfo, SideChannelRegistry,
    SideChannelStatus,
};
