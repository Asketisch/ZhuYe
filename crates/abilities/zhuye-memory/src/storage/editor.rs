//! 条目编辑端口(v1.11 M5a):`MemoryTool` 等写侧消费方经此 trait
//! 操作条目,**不感知**底层是文件(`FileMemoryStore` 旧路径)还是
//! SQLite 条目版本库(`SqliteMemoryStore` 新路径)—— 所有既有写入
//! 口收敛到同一接口(plan §2:唯一权威写入链)。

use crate::consolidate::ConsolidateStats;
use crate::edit::{EntryWriteOptions, MemoryEntry};
use crate::model::{MemoryError, MemoryScope};
use crate::store::FileMemoryStore;

/// 条目编辑抽象:列 / 写 / 删 / 整理。
pub trait MemoryEditor: Send + Sync {
    /// 列出当前全部(活跃)条目。
    fn list_entries(
        &self,
        scope: MemoryScope,
        agent_type: &str,
    ) -> Result<Vec<MemoryEntry>, MemoryError>;

    /// 新增 / 更新条目(语义见各实现的乐观锁与 create_only 约定)。
    fn write_entry(
        &self,
        scope: MemoryScope,
        agent_type: &str,
        key: &str,
        value: &str,
        opts: &EntryWriteOptions,
    ) -> Result<(), MemoryError>;

    /// 删除条目(旧路径物理移除标题块;新路径软失效 + 墓碑)。
    fn delete_entry(
        &self,
        scope: MemoryScope,
        agent_type: &str,
        key: &str,
        opts: &EntryWriteOptions,
    ) -> Result<(), MemoryError>;

    /// 确定性整理(去重 / 合并);无文件可整理时返回 `None`。
    fn consolidate_entries(
        &self,
        scope: MemoryScope,
        agent_type: &str,
    ) -> Result<Option<ConsolidateStats>, MemoryError>;
}

impl MemoryEditor for FileMemoryStore {
    fn list_entries(
        &self,
        scope: MemoryScope,
        agent_type: &str,
    ) -> Result<Vec<MemoryEntry>, MemoryError> {
        FileMemoryStore::list_entries(self, scope, agent_type)
    }

    fn write_entry(
        &self,
        scope: MemoryScope,
        agent_type: &str,
        key: &str,
        value: &str,
        opts: &EntryWriteOptions,
    ) -> Result<(), MemoryError> {
        FileMemoryStore::add_entry(self, scope, agent_type, key, value, opts)
    }

    fn delete_entry(
        &self,
        scope: MemoryScope,
        agent_type: &str,
        key: &str,
        opts: &EntryWriteOptions,
    ) -> Result<(), MemoryError> {
        FileMemoryStore::remove_entry(self, scope, agent_type, key, opts)
    }

    fn consolidate_entries(
        &self,
        scope: MemoryScope,
        agent_type: &str,
    ) -> Result<Option<ConsolidateStats>, MemoryError> {
        FileMemoryStore::consolidate(self, scope, agent_type)
    }
}
