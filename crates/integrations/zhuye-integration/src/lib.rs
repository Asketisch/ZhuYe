//! `zhuye-integration` — 外部集成层安全扫描与通知。
//!
//! 本 crate 收敛为两个有真实调用方的能力(CLI `security` 子命令与
//! `doctor` 探测):
//! - `security_scan`: cargo audit CLI 包装
//! - `notifications`: Webhook 通知
//!
//! 历史上的 stub 模块(F45 gateway / F36 issues / F41 voice / F66-68
//! ide_and_local / F60 security_scan_v2)自落地起零调用方,按「形态
//! 判断铁律」(外部接入、可选启停应走插件形态,零消费 stub 不保留
//! 在核心 workspace)已删除;需要时以插件形态重新承载。

pub mod notifications;
pub mod security_scan;

pub use notifications::{NotificationChannel, NotificationPayload, WebhookNotifier};
pub use security_scan::{SecurityScanReport, run_cargo_audit};
