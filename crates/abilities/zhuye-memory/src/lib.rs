//! zhuye-memory —— 三 scope(project / user / session)持久 memory。
//!
//! Zhuye 路线图的 M4。移植自 Zhuye `agent_memory.py`。
//!
//! 三种 scope:
//!
//! - [`MemoryScope::Project`] —— `{workspace}/.zhuye/agent-memory/{agent_type}/MEMORY.md`。
//!   可入 VCS 共享;跨会话保留;用户择机提交。
//! - [`MemoryScope::User`] —— `~/.zhuye/agent-memory/{agent_type}/MEMORY.md`。
//!   跨项目、本机范围。
//! - [`MemoryScope::Session`] —— 仅内存。M5 将经 JSONL rollout 持久化;
//!   目前存放在 [`InMemoryStore`] 内的 `HashMap` 中。
//!
//! 注入 memory 的字符上限为
//! [`MAX_MEMORY_INJECT_CHARS`](8000,与 Zhuye 一致)。

pub mod edit;
pub mod model;
pub mod retrieval;
pub mod scope;
pub mod store;

pub use edit::{EntryWriteOptions, MemoryEntry, WHOLE_DOCUMENT_KEY, migrate_legacy_agent_dir};
pub use model::{MemoryError, MemoryScope};
pub use retrieval::{MemorySection, retrieve_relevant, retrieve_relevant_arc, split_sections};
pub use scope::{MAX_MEMORY_INJECT_CHARS, resolve_path};
pub use store::{FileMemoryStore, InMemoryStore, MemoryStore, truncate_for_injection};
