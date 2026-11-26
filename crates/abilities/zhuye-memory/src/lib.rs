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

pub mod backend;
pub mod edit;
/// 记忆的认知面（画像 / 长尾 / 多文件知识组织）。
pub mod insight;
/// 记忆生命周期（抽取 / 整理 / 自进化）。
pub mod lifecycle;
pub mod model;
/// 记忆检索（分节切分 / 混合召回）。
pub mod retrieval;
pub mod sqlite;
/// 记忆的写侧端口与落盘位置。
pub mod storage;
pub mod store;

pub use backend::{MemoryBackend, MemoryStack, backend_from_env, bootstrap_stack};
pub use edit::{EntryWriteOptions, MemoryEntry, WHOLE_DOCUMENT_KEY, migrate_legacy_agent_dir};
pub use insight::longtail::{
    EdgeKind, Evidence, EvidenceSource, GroundedFact, LongTailStore, NamedSlot, SlotField,
    TemporalEdge,
};
pub use insight::persona::{
    Persona, Preference, PreferenceStatus, PreferenceStore, compose_prompt_sections,
};
pub use insight::topics::validate_topic_name;
pub use lifecycle::consolidate::{ConsolidateStats, Consolidator, consolidate_document};
pub use lifecycle::evolution::{
    CachedSolution, CandidateQueue, CandidateStatus, ErrorSolutionCache, EvolutionError,
    SkillCandidate, SkillWriteBackup, prepare_write, restore_from_backup,
};
pub use lifecycle::extract::{
    CandidateFact, ConsolidateOutcome, LayerPolicy, MemoryFact, MemoryLayer, consolidate,
    extract_candidates, injection_budget,
};
pub use model::{MemoryError, MemoryScope};
pub use retrieval::hybrid::{
    HybridHit, HybridRetriever, InMemoryVectorIndex, RetrievalSource, cosine_similarity, fuse_rrf,
};
pub use retrieval::sections::{
    MemorySection, retrieve_relevant, retrieve_relevant_arc, split_sections,
};
pub use sqlite::{ChangeAction, ChangeReport, EntryKind, MemoryChange, SqliteMemoryStore};
pub use storage::editor::MemoryEditor;
pub use storage::scope::{MAX_MEMORY_INJECT_CHARS, resolve_path};
pub use store::{FileMemoryStore, InMemoryStore, MemoryStore, truncate_for_injection};
