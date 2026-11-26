//! 记忆检索 —— 分节切分与混合召回。
//!
//! - [`sections`]：把平面 MEMORY.md 按 `## 标题` 拆成条目，按当前查询
//!   选相关条目 + 最近使用条目。
//! - [`hybrid`]：BM25 + 向量 RRF 融合的语义检索（F16）。

pub mod hybrid;
pub mod sections;
