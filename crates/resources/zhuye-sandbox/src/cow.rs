//! # CoW 隔离后端(F25)
//!
//! 用文件系统「写时复制」秒级克隆工作区给子 agent,互不踩踏;改完能
//! diff 出差异、能合回。
//!
//! ## 平台后端
//!
//! | 平台 | 机制 | 成本 |
//! |---|---|---|
//! | macOS | `clonefile`(APFS) | 近乎零(元数据) |
//! | Linux | `overlayfs`(upper/lower) | 零(目录项) |
//! | Windows | `projfs` | 零(虚拟化) |
//! | 其它 | 硬链接 / 复制兜底 | 硬链接零,复制 O(n) |
//!
//! 本模块**只做策略与差异契约**,不直接调平台 API(那需要 unsafe FFI 与
//! 各平台条件编译,属于后续单独 PR)。它提供:
//!
//! - [`CowBackend`] —— 选择后端 + 说明能力(是否真 CoW / 是否支持 diff);
//! - [`ChangeSet`] —— 克隆与源之间的差异契约(`git diff --no-index` 的
//!   字节一致语义);
//! - [`MergeStrategy`] —— 合回策略(全部覆盖 / 只合并新增 / 逐文件确认)。
//!
//! ## 为什么「diff 字节一致」是契约
//!
//! 合回前用户要看 diff。若我们的 diff 与 `git diff` 不一致,用户就无法
//! 用熟悉的工具核对。所以 [`ChangeSet`] 只记「哪些文件变了 + 怎么变」,
//! 渲染交给既有 diff 视图 —— 保证语义单一来源。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// CoW 后端类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CowBackend {
    /// macOS APFS `clonefile`。
    ApfsClonefile,
    /// Linux overlayfs。
    Overlayfs,
    /// Windows Projected File System。
    Projfs,
    /// 硬链接(同文件系统)。
    Hardlink,
    /// 逐文件复制(兜底,最慢)。
    Copy,
}

impl CowBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            CowBackend::ApfsClonefile => "apfs_clonefile",
            CowBackend::Overlayfs => "overlayfs",
            CowBackend::Projfs => "projfs",
            CowBackend::Hardlink => "hardlink",
            CowBackend::Copy => "copy",
        }
    }

    /// 是否真正的写时复制(零拷贝)。
    ///
    /// 硬链接不是真 CoW —— 子 agent 原地改写会**穿透**到源文件
    /// (除非写时先 unlink)。这一点必须在 UI 上说明,不能让用户误以为
    /// 硬链接隔离等于 CoW 隔离。
    pub fn is_true_cow(self) -> bool {
        matches!(
            self,
            CowBackend::ApfsClonefile | CowBackend::Overlayfs | CowBackend::Projfs
        )
    }

    /// 该后端能否高效支持大仓库(零拷贝)。
    pub fn is_efficient(self) -> bool {
        !matches!(self, CowBackend::Copy)
    }

    /// 按平台名选择首选后端。
    pub fn preferred_for(os: &str) -> CowBackend {
        match os.trim().to_ascii_lowercase().as_str() {
            "macos" | "darwin" => CowBackend::ApfsClonefile,
            "linux" => CowBackend::Overlayfs,
            "windows" => CowBackend::Projfs,
            _ => CowBackend::Copy,
        }
    }

    /// 回退链:首选不可用时依次尝试。
    pub fn fallback_chain(self) -> Vec<CowBackend> {
        match self {
            // Copy 已是最后手段,无更弱的回退。
            CowBackend::Copy => vec![CowBackend::Copy],
            // 硬链接之后只剩复制。
            CowBackend::Hardlink => vec![CowBackend::Hardlink, CowBackend::Copy],
            // 真 CoW → 硬链接 → 复制。
            other => vec![other, CowBackend::Hardlink, CowBackend::Copy],
        }
    }
}

/// 文件变更类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Added => "added",
            ChangeKind::Modified => "modified",
            ChangeKind::Deleted => "deleted",
        }
    }
}

/// 单个文件的变更。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// 相对工作区根的路径。
    pub path: PathBuf,
    pub kind: ChangeKind,
    /// 源侧内容指纹(`None` = 新增)。
    #[serde(default)]
    pub before_hash: Option<String>,
    /// 克隆侧内容指纹(`None` = 删除)。
    #[serde(default)]
    pub after_hash: Option<String>,
}

/// 变更集(克隆 → 源)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSet {
    pub changes: Vec<FileChange>,
}

impl ChangeSet {
    pub fn new() -> ChangeSet {
        ChangeSet::default()
    }

    /// 记录一个变更(同路径覆盖 —— 一个文件只记最终状态)。
    pub fn record(&mut self, change: FileChange) {
        if let Some(slot) = self.changes.iter_mut().find(|c| c.path == change.path) {
            *slot = change;
        } else {
            self.changes.push(change);
        }
    }

    /// 按路径排序的变更列表(确定性输出)。
    pub fn sorted(&self) -> Vec<&FileChange> {
        let mut out: Vec<&FileChange> = self.changes.iter().collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    /// 是否有变更。
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// 按类型计数。
    pub fn count(&self, kind: ChangeKind) -> usize {
        self.changes.iter().filter(|c| c.kind == kind).count()
    }

    /// 人读摘要(`3 added · 2 modified · 1 deleted`)。
    pub fn summary(&self) -> String {
        format!(
            "{} added · {} modified · {} deleted",
            self.count(ChangeKind::Added),
            self.count(ChangeKind::Modified),
            self.count(ChangeKind::Deleted)
        )
    }

    /// 是否有冲突风险:同一路径既被克隆改、又在源侧被改。
    ///
    /// `source_changed` 是源侧自克隆以来变更的路径集合。
    pub fn conflicts_with(&self, source_changed: &[PathBuf]) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self
            .changes
            .iter()
            .filter(|c| c.kind != ChangeKind::Added) // 新增文件不会冲突
            .filter(|c| source_changed.contains(&c.path))
            .map(|c| c.path.clone())
            .collect();
        out.sort();
        out
    }
}

/// 合回策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStrategy {
    /// 全部覆盖源(克隆侧胜出)。
    #[default]
    OursWins,
    /// 只合并新增文件(不动既有文件 —— 最保守)。
    AddedOnly,
    /// 逐文件确认(调用方拿到 ChangeSet 后自己决定)。
    Manual,
}

impl MergeStrategy {
    /// 该策略是否允许合并这个变更。
    pub fn allows(&self, kind: ChangeKind) -> bool {
        match self {
            MergeStrategy::OursWins => true,
            MergeStrategy::AddedOnly => kind == ChangeKind::Added,
            MergeStrategy::Manual => false, // 需人工
        }
    }

    /// 从策略 + 变更集算出实际会合并的文件。
    pub fn select<'a>(&self, changes: &'a ChangeSet) -> Vec<&'a FileChange> {
        changes
            .changes
            .iter()
            .filter(|c| self.allows(c.kind))
            .collect()
    }
}

/// 一个 CoW 工作区句柄。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CowWorkspace {
    /// 源工作区根。
    pub source: PathBuf,
    /// 克隆出的工作区根。
    pub clone_root: PathBuf,
    /// 使用的后端。
    pub backend: CowBackend,
    /// 后端是否真 CoW(UI 据此提示"硬链接隔离不等价于 CoW")。
    pub true_cow: bool,
}

impl CowWorkspace {
    pub fn new(
        source: impl Into<PathBuf>,
        clone_root: impl Into<PathBuf>,
        backend: CowBackend,
    ) -> CowWorkspace {
        CowWorkspace {
            source: source.into(),
            clone_root: clone_root.into(),
            backend,
            true_cow: backend.is_true_cow(),
        }
    }

    /// 克隆侧路径是否落在克隆根内(防止子 agent 逃逸到源)。
    pub fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.clone_root)
    }

    /// 隔离说明(UI 徽标文案)。
    pub fn isolation_label(&self) -> String {
        if self.true_cow {
            format!("{} · true CoW", self.backend.as_str())
        } else {
            format!(
                "{} · 非真 CoW(原地改写可能穿透到源文件)",
                self.backend.as_str()
            )
        }
    }
}

/// 按文件名集合计算差异(简化版:调用方提供两侧指纹)。
///
/// 这是 [`ChangeSet`] 的构造辅助 —— 真实的文件系统遍历由调用方做
/// (需要处理符号链接 / 权限 / 二进制),本函数只负责**归类**。
pub fn diff_hashes(
    before: &BTreeMap<PathBuf, String>,
    after: &BTreeMap<PathBuf, String>,
) -> ChangeSet {
    let mut set = ChangeSet::new();
    for (path, after_hash) in after {
        match before.get(path) {
            Some(before_hash) if before_hash == after_hash => {} // 未变
            Some(before_hash) => set.record(FileChange {
                path: path.clone(),
                kind: ChangeKind::Modified,
                before_hash: Some(before_hash.clone()),
                after_hash: Some(after_hash.clone()),
            }),
            None => set.record(FileChange {
                path: path.clone(),
                kind: ChangeKind::Added,
                before_hash: None,
                after_hash: Some(after_hash.clone()),
            }),
        }
    }
    for (path, before_hash) in before {
        if !after.contains_key(path) {
            set.record(FileChange {
                path: path.clone(),
                kind: ChangeKind::Deleted,
                before_hash: Some(before_hash.clone()),
                after_hash: None,
            });
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashes(entries: &[(&str, &str)]) -> BTreeMap<PathBuf, String> {
        entries
            .iter()
            .map(|(p, h)| (PathBuf::from(*p), h.to_string()))
            .collect()
    }

    #[test]
    fn backend_preference_by_os() {
        assert_eq!(
            CowBackend::preferred_for("macos"),
            CowBackend::ApfsClonefile
        );
        assert_eq!(
            CowBackend::preferred_for("darwin"),
            CowBackend::ApfsClonefile
        );
        assert_eq!(CowBackend::preferred_for("linux"), CowBackend::Overlayfs);
        assert_eq!(CowBackend::preferred_for("windows"), CowBackend::Projfs);
        assert_eq!(CowBackend::preferred_for("freebsd"), CowBackend::Copy);
    }

    #[test]
    fn true_cow_classification() {
        assert!(CowBackend::ApfsClonefile.is_true_cow());
        assert!(CowBackend::Overlayfs.is_true_cow());
        assert!(CowBackend::Projfs.is_true_cow());
        assert!(!CowBackend::Hardlink.is_true_cow(), "硬链接不是真 CoW");
        assert!(!CowBackend::Copy.is_true_cow());
    }

    #[test]
    fn efficiency() {
        assert!(CowBackend::ApfsClonefile.is_efficient());
        assert!(CowBackend::Hardlink.is_efficient());
        assert!(!CowBackend::Copy.is_efficient(), "复制是 O(n)");
    }

    #[test]
    fn fallback_chain_ends_with_copy() {
        let chain = CowBackend::ApfsClonefile.fallback_chain();
        assert_eq!(chain[0], CowBackend::ApfsClonefile);
        assert_eq!(*chain.last().unwrap(), CowBackend::Copy);
        assert!(chain.contains(&CowBackend::Hardlink));
    }

    #[test]
    fn fallback_from_copy_is_just_copy() {
        assert_eq!(CowBackend::Copy.fallback_chain(), vec![CowBackend::Copy]);
    }

    #[test]
    fn diff_detects_all_three_kinds() {
        let before = hashes(&[("a.txt", "h1"), ("b.txt", "h2"), ("gone.txt", "h3")]);
        let after = hashes(&[("a.txt", "h1"), ("b.txt", "CHANGED"), ("new.txt", "h4")]);
        let set = diff_hashes(&before, &after);
        assert_eq!(set.len(), 3);
        assert_eq!(set.count(ChangeKind::Modified), 1);
        assert_eq!(set.count(ChangeKind::Added), 1);
        assert_eq!(set.count(ChangeKind::Deleted), 1);
        let modified = set
            .changes
            .iter()
            .find(|c| c.kind == ChangeKind::Modified)
            .unwrap();
        assert_eq!(modified.path, PathBuf::from("b.txt"));
        assert_eq!(modified.before_hash.as_deref(), Some("h2"));
        assert_eq!(modified.after_hash.as_deref(), Some("CHANGED"));
    }

    #[test]
    fn diff_no_change_is_empty() {
        let same = hashes(&[("a", "h")]);
        assert!(diff_hashes(&same, &same).is_empty());
    }

    #[test]
    fn change_set_sorted_deterministic() {
        let before = hashes(&[]);
        let after = hashes(&[("z.txt", "1"), ("a.txt", "2"), ("m.txt", "3")]);
        let set = diff_hashes(&before, &after);
        let paths: Vec<String> = set
            .sorted()
            .iter()
            .map(|c| c.path.display().to_string())
            .collect();
        assert_eq!(paths, vec!["a.txt", "m.txt", "z.txt"]);
    }

    #[test]
    fn change_set_record_dedups_same_path() {
        let mut set = ChangeSet::new();
        set.record(FileChange {
            path: "a".into(),
            kind: ChangeKind::Modified,
            before_hash: Some("1".into()),
            after_hash: Some("2".into()),
        });
        set.record(FileChange {
            path: "a".into(),
            kind: ChangeKind::Deleted,
            before_hash: Some("2".into()),
            after_hash: None,
        });
        assert_eq!(set.len(), 1, "同路径只记最终状态");
        assert_eq!(set.changes[0].kind, ChangeKind::Deleted);
    }

    #[test]
    fn summary_counts() {
        let before = hashes(&[("m", "1"), ("d", "1")]);
        let after = hashes(&[("m", "2"), ("a", "1")]);
        let set = diff_hashes(&before, &after);
        let s = set.summary();
        assert!(s.contains("1 added"), "{s}");
        assert!(s.contains("1 modified"), "{s}");
        assert!(s.contains("1 deleted"), "{s}");
    }

    #[test]
    fn conflicts_detected_for_modified_not_added() {
        let mut set = ChangeSet::new();
        set.record(FileChange {
            path: "both-changed.txt".into(),
            kind: ChangeKind::Modified,
            before_hash: Some("1".into()),
            after_hash: Some("2".into()),
        });
        set.record(FileChange {
            path: "only-new.txt".into(),
            kind: ChangeKind::Added,
            before_hash: None,
            after_hash: Some("3".into()),
        });
        let source_changed = vec![
            PathBuf::from("both-changed.txt"),
            PathBuf::from("only-new.txt"),
        ];
        let conflicts = set.conflicts_with(&source_changed);
        assert_eq!(
            conflicts,
            vec![PathBuf::from("both-changed.txt")],
            "新增不冲突"
        );
    }

    #[test]
    fn merge_strategies() {
        assert!(MergeStrategy::OursWins.allows(ChangeKind::Deleted));
        assert!(MergeStrategy::AddedOnly.allows(ChangeKind::Added));
        assert!(!MergeStrategy::AddedOnly.allows(ChangeKind::Modified));
        assert!(!MergeStrategy::Manual.allows(ChangeKind::Added));
    }

    #[test]
    fn merge_select_filters() {
        let before = hashes(&[("m", "1")]);
        let after = hashes(&[("m", "2"), ("a", "1")]);
        let set = diff_hashes(&before, &after);
        let ours = MergeStrategy::OursWins.select(&set);
        assert_eq!(ours.len(), 2);
        let added_only = MergeStrategy::AddedOnly.select(&set);
        assert_eq!(added_only.len(), 1);
        assert_eq!(added_only[0].kind, ChangeKind::Added);
        assert!(MergeStrategy::Manual.select(&set).is_empty());
    }

    #[test]
    fn workspace_contains_guard() {
        let ws = CowWorkspace::new("/src/repo", "/tmp/clone-1", CowBackend::ApfsClonefile);
        assert!(ws.contains(Path::new("/tmp/clone-1/src/lib.rs")));
        assert!(
            !ws.contains(Path::new("/src/repo/src/lib.rs")),
            "不得逃逸到源"
        );
        assert!(!ws.contains(Path::new("/tmp/clone-10/x")), "前缀不能误匹配");
    }

    #[test]
    fn isolation_label_explains_hardlink() {
        let cow = CowWorkspace::new("/s", "/c", CowBackend::ApfsClonefile);
        assert!(cow.isolation_label().contains("true CoW"));
        let hl = CowWorkspace::new("/s", "/c", CowBackend::Hardlink);
        assert!(
            hl.isolation_label().contains("非真 CoW"),
            "{}",
            hl.isolation_label()
        );
    }

    #[test]
    fn change_set_serializes() {
        let mut set = ChangeSet::new();
        set.record(FileChange {
            path: "a".into(),
            kind: ChangeKind::Added,
            before_hash: None,
            after_hash: Some("h".into()),
        });
        let json = serde_json::to_string(&set).expect("serialize");
        let back: ChangeSet = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, set);
    }
}
