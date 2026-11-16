//! `zhuye-integration` — 外部集成层 stub 与安全扫描。
//!
//! 本 crate 聚合 P2 集成特性:
//! - `security_scan`: cargo audit CLI 包装
//! - `notifications`: Webhook 通知 stub

/// F45:本地 LLM 网关(三协议入站 + 虚拟密钥 + 边缘守护)。
pub mod gateway;
pub mod notifications;
pub mod security_scan;

pub use gateway::{
    EdgeGuardResult, EdgeVerdict, GatewayConfig, GatewayError, InboundProtocol, KeyTable,
    ProtocolMapping, VirtualKey, guard_inbound,
};
pub use notifications::{NotificationChannel, NotificationPayload, WebhookNotifier};
pub use security_scan::{SecurityScanReport, run_cargo_audit};
