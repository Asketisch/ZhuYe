//! Harness adapter 契约与注册表。
//!
//! 每家外部 Harness 一个实现(与 `zhuye-llm` 的 per-provider 组织方式
//! 同构):`probe` 做格式嗅探(`--from auto` 用),`parse` 把一个
//! [`ImportSource`] 解析为 IR,`discover` 扫描该 Harness 的默认安装
//! 目录(`--all` 用)。新增一家(如 DeepSeek)= 新增一个模块 + 在
//! [`all_kinds`] / [`adapter_for`] 各加一行,不动架构。

use std::path::Path;

use crate::error::ImportError;
use crate::model::{HarnessKind, ImportOptions, ImportSource, ImportedSession};

/// 单家 Harness 的解析器。
pub trait HarnessAdapter {
    /// 所属来源种类。
    fn kind(&self) -> HarnessKind;

    /// 嗅探给定 JSONL 文件是否为本 Harness 格式(读首行判别,不整读)。
    /// SQLite 来源不参与嗅探(必须显式 `--from zcode`)。
    fn probe(&self, path: &Path) -> bool;

    /// 解析一个来源为 IR。实现约定:
    /// - 逐行 / 逐表容错:畸形行记 warning 跳过,不整体失败;
    /// - 完全没有可导入内容时返回 [`ImportError::EmptySession`]。
    fn parse(
        &self,
        source: &ImportSource,
        opts: &ImportOptions,
    ) -> Result<ImportedSession, ImportError>;

    /// 扫描该 Harness 默认安装位置,列出全部可导入来源。
    /// 找不到安装目录返回空列表(不报错 —— 未安装是常态)。
    fn discover(&self) -> Result<Vec<ImportSource>, ImportError>;
}

/// 全部已注册的 adapter(按种类)。
pub fn all_kinds() -> &'static [HarnessKind] {
    &[
        HarnessKind::ClaudeCode,
        HarnessKind::Codex,
        HarnessKind::Zcode,
    ]
}

/// 按种类取 adapter。DeepSeek 等未来来源在此追加。
pub fn adapter_for(kind: HarnessKind) -> Box<dyn HarnessAdapter> {
    match kind {
        HarnessKind::ClaudeCode => Box::new(crate::adapters::claude_code::ClaudeCodeAdapter),
        HarnessKind::Codex => Box::new(crate::adapters::codex::CodexAdapter),
        HarnessKind::Zcode => Box::new(crate::adapters::zcode::ZcodeAdapter),
    }
}

/// 对单个 JSONL 文件做格式嗅探,返回命中的种类。
///
/// 嗅探顺序即注册顺序;都不命中 → `None`(调用方报
/// [`ImportError::UnknownFormat`])。SQLite 来源(`.sqlite` 后缀)不嗅探。
pub fn sniff(path: &Path) -> Option<HarnessKind> {
    if path.extension().and_then(|e| e.to_str()) == Some("sqlite") {
        return None;
    }
    [
        HarnessKind::ClaudeCode,
        HarnessKind::Codex,
        HarnessKind::Zcode,
    ]
    .into_iter()
    .find(|k| adapter_for(*k).probe(path))
}
