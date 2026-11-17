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
pub mod consolidate;
pub mod edit;
pub mod editor;
/// F18:技能自进化(候选队列 + review 门 + 错误-解法缓存 + 备份)。
pub mod evolution;
/// F16:BM25 + 向量 RRF 混合检索(无 embedding 时自动降级)。
pub mod hybrid;
/// F17:记忆自动抽取 + 分层生命周期(衰减/驱逐/注入预算)。
pub mod lifecycle;
/// F59:记忆长尾(时态边 + 证据接地 + 命名槽 + MemFS 导出)。
pub mod longtail;
pub mod model;
/// F19:用户画像(偏好学习 + 审批门)+ Persona/SOUL 注入。
pub mod persona;
pub mod retrieval;
pub mod scope;
pub mod sqlite;
pub mod store;
pub mod topics;

pub use backend::{MemoryBackend, MemoryStack, backend_from_env, bootstrap_stack};
pub use consolidate::{ConsolidateStats, Consolidator, consolidate_document};
pub use edit::{EntryWriteOptions, MemoryEntry, WHOLE_DOCUMENT_KEY, migrate_legacy_agent_dir};
pub use editor::MemoryEditor;
pub use evolution::{
    CachedSolution, CandidateQueue, CandidateStatus, ErrorSolutionCache, EvolutionError,
    SkillCandidate, SkillWriteBackup, prepare_write, restore_from_backup,
};
pub use hybrid::{
    HybridHit, HybridRetriever, InMemoryVectorIndex, RetrievalSource, cosine_similarity, fuse_rrf,
};
pub use lifecycle::{
    CandidateFact, ConsolidateOutcome, LayerPolicy, MemoryFact, MemoryLayer, consolidate,
    extract_candidates, injection_budget,
};
pub use longtail::{
    EdgeKind, Evidence, EvidenceSource, GroundedFact, LongTailStore, NamedSlot, SlotField,
    TemporalEdge,
};
pub use model::{MemoryError, MemoryScope};
pub use persona::{
    Persona, Preference, PreferenceStatus, PreferenceStore, compose_prompt_sections,
};
pub use retrieval::{MemorySection, retrieve_relevant, retrieve_relevant_arc, split_sections};
pub use scope::{MAX_MEMORY_INJECT_CHARS, resolve_path};
pub use sqlite::{ChangeAction, ChangeReport, EntryKind, MemoryChange, SqliteMemoryStore};
pub use store::{FileMemoryStore, InMemoryStore, MemoryStore, truncate_for_injection};
pub use topics::validate_topic_name;
