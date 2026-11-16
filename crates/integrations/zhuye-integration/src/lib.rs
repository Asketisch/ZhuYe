//! `zhuye-integration` — 外部集成层 stub 与安全扫描。
//!
//! 本 crate 聚合 P2 集成特性:
//! - `security_scan`: cargo audit CLI 包装
//! - `notifications`: Webhook 通知 stub

/// F45:本地 LLM 网关(三协议入站 + 虚拟密钥 + 边缘守护)。
pub mod gateway;
/// F36:工单驱动(归一 + 认领策略 + 依赖排序 + 站会报告)。
pub mod issues;
pub mod notifications;
pub mod security_scan;
/// F41:本地语音回路(模型目录 + 流式分帧 + barge-in + 状态机)。
pub mod voice;

pub use gateway::{
    EdgeGuardResult, EdgeVerdict, GatewayConfig, GatewayError, InboundProtocol, KeyTable,
    ProtocolMapping, VirtualKey, guard_inbound,
};
pub use issues::{
    ClaimDecision, ClaimPolicy, Issue, IssueSource, IssueStatus, StandupReport,
    order_by_dependencies, standup_report,
};
pub use notifications::{NotificationChannel, NotificationPayload, WebhookNotifier};
pub use security_scan::{SecurityScanReport, run_cargo_audit};
pub use voice::{
    AudioFrame, FrameAssembler, VoiceError, VoiceModelCatalog, VoiceModelKind, VoiceModelSpec,
    VoiceSession, VoiceState,
};
