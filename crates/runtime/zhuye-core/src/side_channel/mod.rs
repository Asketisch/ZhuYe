//! Side-channel agent(详见 [`registry`] / [`driver`] 模块文档)。

pub mod driver;
mod inherited_plan;
pub mod registry;

pub use driver::{
    SideChannelContext, SideChannelDriver, SideChannelStartError, SideContextThreadFactory,
    SideThreadFactory,
};
pub use inherited_plan::InheritedPlanGate;
pub use registry::{
    MAX_CONCURRENT, SideChannelEvent, SideChannelId, SideChannelInfo, SideChannelRegistry,
    SideChannelStatus,
};
