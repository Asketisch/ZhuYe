//! # 时间旅行与恢复(F47)
//!
//! agent 发现走错了能「后悔」回退到更早状态。
//!
//! ## 三粒度 checkpoint
//!
//! - **Turn 级** —— 每个回合边界(粗粒度,恢复快);
//! - **Tool 级** —— 每次工具调用后(中粒度,能回到"改坏文件之前");
//! - **Message 级** —— 每条消息后(细粒度,存储开销大)。
//!
//! [`CheckpointKind`] 让调用方按需选择 —— 默认 Tool 级(性价比最高:
//! 能精确回到"那一步之前",开销可接受)。
//!
//! ## 回退的两种语义
//!
//! 1. **仅回退对话**(`rewind` 现有行为)—— 消息历史截断,文件不变;
//! 2. **联动文件还原**([`RewindPlan::restore_files`])—— 同时把被改的
//!    文件恢复到 checkpoint 时刻。
//!
//! 第二种是 F47 的增量:agent 说"我改错了"时,只回退对话是不够的 ——
//! 磁盘上还是坏代码。
//!
//! ## 为什么回退前要确认
//!
//! 文件还原会**丢弃** checkpoint 之后的所有改动。若用户在同一时间段
//! 手改过文件,那些改动也会丢。所以 [`RewindPlan`] 要求显式列出会被
//! 覆盖的文件,由调用方向用户确认。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// checkpoint 粒度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointKind {
    /// 每回合。
    Turn,
    /// 每次工具调用后(默认)。
    #[default]
    Tool,
    /// 每条消息。
    Message,
}

impl CheckpointKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckpointKind::Turn => "turn",
            CheckpointKind::Tool => "tool",
            CheckpointKind::Message => "message",
        }
    }
}

/// 一个 checkpoint。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointMeta {
    /// checkpoint id(单调递增)。
    pub id: u64,
    pub kind: CheckpointKind,
    /// 对应的回合 id。
    pub turn_id: String,
    /// 触发点描述(`"after tool: edit"` / `"turn boundary"`)。
    pub trigger: String,
    /// 时间戳。
    pub at: u64,
    /// 消息数(回退时截断到这个数)。
    pub message_count: usize,
    /// 文件快照指纹(`path -> hash`),用于判断哪些文件需要还原。
    #[serde(default)]
    pub file_hashes: BTreeMap<String, String>,
}

impl CheckpointMeta {
    /// 是否某文件在此时刻的状态与给定哈希不同(需要还原)。
    pub fn needs_restore(&self, path: &str, current_hash: &str) -> bool {
        self.file_hashes
            .get(path)
            .is_some_and(|h| h != current_hash)
    }
}

/// checkpoint 存储(内存 + 上限)。
#[derive(Debug)]
pub struct CheckpointStore {
    checkpoints: Vec<CheckpointMeta>,
    next_id: u64,
    /// 最多保留多少个(超出丢最旧 —— 细粒度 checkpoint 会很多)。
    max_entries: usize,
}

impl Default for CheckpointStore {
    fn default() -> Self {
        CheckpointStore {
            checkpoints: Vec::new(),
            next_id: 1,
            max_entries: 200,
        }
    }
}

impl CheckpointStore {
    pub fn new(max_entries: usize) -> CheckpointStore {
        CheckpointStore {
            max_entries: if max_entries == 0 { 200 } else { max_entries },
            ..Default::default()
        }
    }

    /// 记录一个 checkpoint,返回分配到的 id。
    pub fn record(
        &mut self,
        kind: CheckpointKind,
        turn_id: impl Into<String>,
        trigger: impl Into<String>,
        at: u64,
        message_count: usize,
        file_hashes: BTreeMap<String, String>,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.checkpoints.push(CheckpointMeta {
            id,
            kind,
            turn_id: turn_id.into(),
            trigger: trigger.into(),
            at,
            message_count,
            file_hashes,
        });
        // 超出上限丢最旧。
        while self.checkpoints.len() > self.max_entries {
            self.checkpoints.remove(0);
        }
        id
    }

    /// 全部 checkpoint(按 id 升序)。
    pub fn all(&self) -> &[CheckpointMeta] {
        &self.checkpoints
    }

    pub fn len(&self) -> usize {
        self.checkpoints.len()
    }

    pub fn is_empty(&self) -> bool {
        self.checkpoints.is_empty()
    }

    /// 按 id 查。
    pub fn get(&self, id: u64) -> Option<&CheckpointMeta> {
        self.checkpoints.iter().find(|c| c.id == id)
    }

    /// 最近一个 checkpoint。
    pub fn latest(&self) -> Option<&CheckpointMeta> {
        self.checkpoints.last()
    }

    /// 某回合的 checkpoint。
    pub fn by_turn(&self, turn_id: &str) -> Vec<&CheckpointMeta> {
        self.checkpoints
            .iter()
            .filter(|c| c.turn_id == turn_id)
            .collect()
    }

    /// 指定粒度最近的一个。
    pub fn latest_of_kind(&self, kind: CheckpointKind) -> Option<&CheckpointMeta> {
        self.checkpoints.iter().rev().find(|c| c.kind == kind)
    }

    /// 清理指定 id 之后的 checkpoint(回退后丢弃未来)。
    pub fn truncate_after(&mut self, id: u64) -> usize {
        let before = self.checkpoints.len();
        self.checkpoints.retain(|c| c.id <= id);
        before - self.checkpoints.len()
    }
}

/// 回退计划。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindPlan {
    /// 目标 checkpoint。
    pub checkpoint_id: u64,
    /// 对话回退到多少条消息。
    pub truncate_messages_to: usize,
    /// 需要还原的文件(路径 → 应恢复到的哈希)。
    pub restore_files: BTreeMap<String, String>,
    /// 是否已被用户确认(有文件还原时必须确认)。
    pub confirmed: bool,
}

impl RewindPlan {
    /// 是否需要文件还原。
    pub fn needs_file_restore(&self) -> bool {
        !self.restore_files.is_empty()
    }

    /// 是否可执行(有文件还原时需 confirmed)。
    pub fn is_executable(&self) -> bool {
        !self.needs_file_restore() || self.confirmed
    }

    /// 人读摘要。
    pub fn summary(&self) -> String {
        let files = if self.restore_files.is_empty() {
            "无文件还原".to_string()
        } else {
            format!("还原 {} 个文件", self.restore_files.len())
        };
        format!(
            "回退到 checkpoint #{}:消息截断至 {} 条,{files}",
            self.checkpoint_id, self.truncate_messages_to
        )
    }
}

/// 生成回退计划。
///
/// `current_hashes` 是当前磁盘上文件的哈希;只有与 checkpoint 时刻不同的
/// 文件才需要还原。
pub fn plan_rewind(
    checkpoint: &CheckpointMeta,
    current_hashes: &BTreeMap<String, String>,
) -> RewindPlan {
    let mut restore = BTreeMap::new();
    for (path, hash) in &checkpoint.file_hashes {
        match current_hashes.get(path) {
            Some(current) if current == hash => {} // 未变,不用还原
            _ => {
                restore.insert(path.clone(), hash.clone());
            }
        }
    }
    RewindPlan {
        checkpoint_id: checkpoint.id,
        truncate_messages_to: checkpoint.message_count,
        restore_files: restore,
        confirmed: false,
    }
}

/// 恢复配方(Recovery recipes):常见故障的标准处置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryRecipe {
    /// 症状描述。
    pub symptom: String,
    /// 建议动作。
    pub actions: Vec<String>,
    /// 是否需要人工介入。
    pub needs_human: bool,
}

impl RecoveryRecipe {
    /// 内置配方表。
    pub fn builtin() -> Vec<RecoveryRecipe> {
        vec![
            RecoveryRecipe {
                symptom: "工具反复失败且输出相同".into(),
                actions: vec![
                    "回退到上一次成功的 checkpoint".into(),
                    "检查该工具的参数是否被模型误解".into(),
                ],
                needs_human: false,
            },
            RecoveryRecipe {
                symptom: "上下文被压缩后模型忘记关键信息".into(),
                actions: vec![
                    "回退到压缩前的 checkpoint".into(),
                    "把关键信息写入 note 后再继续".into(),
                ],
                needs_human: false,
            },
            RecoveryRecipe {
                symptom: "文件被改坏".into(),
                actions: vec!["回退到工具调用前的 checkpoint 并还原文件".into()],
                needs_human: true,
            },
            RecoveryRecipe {
                symptom: "会话被判中毒".into(),
                actions: vec![
                    "退役当前会话".into(),
                    "把 notes / memory 迁移到新会话".into(),
                ],
                needs_human: false,
            },
        ]
    }

    /// 按症状关键词匹配配方。
    pub fn match_symptom(symptom: &str) -> Option<RecoveryRecipe> {
        let lower = symptom.to_lowercase();
        Self::builtin().into_iter().find(|r| {
            // 关键词命中(任一 3 字以上片段)。
            r.symptom
                .chars()
                .collect::<Vec<_>>()
                .windows(3)
                .any(|w| lower.contains(&w.iter().collect::<String>()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashes(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(p, h)| (p.to_string(), h.to_string()))
            .collect()
    }

    fn store_with_checkpoints() -> CheckpointStore {
        let mut s = CheckpointStore::new(10);
        s.record(
            CheckpointKind::Turn,
            "t1",
            "turn boundary",
            100,
            10,
            hashes(&[("a.rs", "h1")]),
        );
        s.record(
            CheckpointKind::Tool,
            "t1",
            "after tool: edit",
            101,
            12,
            hashes(&[("a.rs", "h2")]),
        );
        s
    }

    #[test]
    fn record_assigns_increasing_ids() {
        let mut s = CheckpointStore::new(10);
        let id1 = s.record(CheckpointKind::Turn, "t", "x", 1, 0, BTreeMap::new());
        let id2 = s.record(CheckpointKind::Turn, "t", "y", 2, 0, BTreeMap::new());
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[test]
    fn store_caps_entries() {
        let mut s = CheckpointStore::new(3);
        for i in 0..5 {
            s.record(
                CheckpointKind::Tool,
                "t",
                format!("x{i}"),
                i,
                0,
                BTreeMap::new(),
            );
        }
        assert_eq!(s.len(), 3, "超上限丢最旧");
        assert_eq!(s.all()[0].id, 3, "保留的是较新的");
    }

    #[test]
    fn latest_and_by_turn() {
        let s = store_with_checkpoints();
        assert_eq!(s.latest().unwrap().id, 2);
        assert_eq!(s.by_turn("t1").len(), 2);
        assert!(s.by_turn("t2").is_empty());
    }

    #[test]
    fn latest_of_kind() {
        let mut s = store_with_checkpoints();
        s.record(
            CheckpointKind::Message,
            "t1",
            "msg",
            102,
            13,
            BTreeMap::new(),
        );
        assert_eq!(s.latest_of_kind(CheckpointKind::Tool).unwrap().id, 2);
        assert_eq!(s.latest_of_kind(CheckpointKind::Message).unwrap().id, 3);
        assert_eq!(s.latest_of_kind(CheckpointKind::Turn).unwrap().id, 1);
    }

    #[test]
    fn truncate_after_drops_future() {
        let mut s = store_with_checkpoints();
        let removed = s.truncate_after(1);
        assert_eq!(removed, 1);
        assert_eq!(s.len(), 1);
        assert!(s.get(2).is_none());
    }

    #[test]
    fn get_unknown_returns_none() {
        let s = store_with_checkpoints();
        assert!(s.get(999).is_none());
    }

    #[test]
    fn plan_rewind_identifies_changed_files() {
        let s = store_with_checkpoints();
        let cp = s.get(1).unwrap(); // a.rs = h1
        // 当前磁盘:a.rs 变成 h9(被改坏)
        let current = hashes(&[("a.rs", "h9")]);
        let plan = plan_rewind(cp, &current);
        assert_eq!(plan.checkpoint_id, 1);
        assert_eq!(plan.truncate_messages_to, 10);
        assert_eq!(
            plan.restore_files.get("a.rs").map(String::as_str),
            Some("h1")
        );
        assert!(plan.needs_file_restore());
    }

    #[test]
    fn plan_rewind_skips_unchanged_files() {
        let s = store_with_checkpoints();
        let cp = s.get(1).unwrap();
        // 当前磁盘与 checkpoint 一致 → 不需还原
        let current = hashes(&[("a.rs", "h1")]);
        let plan = plan_rewind(cp, &current);
        assert!(!plan.needs_file_restore());
        assert!(plan.is_executable(), "无文件还原不需确认");
    }

    #[test]
    fn plan_requires_confirmation_for_file_restore() {
        let s = store_with_checkpoints();
        let cp = s.get(1).unwrap();
        let current = hashes(&[("a.rs", "changed")]);
        let mut plan = plan_rewind(cp, &current);
        assert!(!plan.is_executable(), "有文件还原需确认");
        plan.confirmed = true;
        assert!(plan.is_executable());
    }

    #[test]
    fn plan_summary() {
        let s = store_with_checkpoints();
        let cp = s.get(1).unwrap();
        let plan = plan_rewind(cp, &hashes(&[("a.rs", "x")]));
        let summary = plan.summary();
        assert!(summary.contains("checkpoint #1"), "{summary}");
        assert!(summary.contains("还原 1 个文件"), "{summary}");
    }

    #[test]
    fn needs_restore_helper() {
        let s = store_with_checkpoints();
        let cp = s.get(1).unwrap();
        assert!(cp.needs_restore("a.rs", "different"));
        assert!(!cp.needs_restore("a.rs", "h1"));
        assert!(!cp.needs_restore("unknown.rs", "any"));
    }

    #[test]
    fn recovery_recipes_builtin() {
        let recipes = RecoveryRecipe::builtin();
        assert_eq!(recipes.len(), 4);
        assert!(recipes.iter().any(|r| r.needs_human), "文件改坏需人工");
    }

    #[test]
    fn recovery_recipe_matching() {
        let m = RecoveryRecipe::match_symptom("工具反复失败").expect("match");
        assert!(m.symptom.contains("反复失败"), "{}", m.symptom);

        let m = RecoveryRecipe::match_symptom("会话被判中毒了").expect("match");
        assert!(m.symptom.contains("中毒"));

        assert!(RecoveryRecipe::match_symptom("完全无关的症状").is_none());
    }

    #[test]
    fn checkpoint_serializes() {
        let s = store_with_checkpoints();
        let cp = s.get(1).unwrap();
        let json = serde_json::to_string(cp).expect("serialize");
        let back: CheckpointMeta = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, *cp);
    }

    #[test]
    fn rewind_plan_serializes() {
        let plan = RewindPlan {
            checkpoint_id: 1,
            truncate_messages_to: 5,
            restore_files: hashes(&[("a", "h")]),
            confirmed: true,
        };
        let json = serde_json::to_string(&plan).expect("serialize");
        let back: RewindPlan = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, plan);
    }
}
