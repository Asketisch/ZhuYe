//! 技能的外部来源与引入安全 —— 市场发现、安装与安装前扫描。
//!
//! 三件模块构成一条完整链路：发现（[`directory`]）→ 安装
//! （[`marketplace`]）→ 安全校验（[`supply_chain`]）。
//!
//! - [`directory`]：skills.sh 目录（sitemap 索引 + 本地搜索）。
//! - [`marketplace`]：从 GitHub 仓库一键安装技能。
//! - [`supply_chain`]：锁文件 / 完整性校验 / 安装前安全扫描（F27）。

pub mod directory;
pub mod marketplace;
pub mod supply_chain;
