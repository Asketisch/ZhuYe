//! # 技能自进化与经验沉淀(F18)
//!
//! 干完活自动复盘,把反复出现的做法沉淀成**可复用技能**;失败的坑记进
//! 「错误-解法」缓存。
//!
//! ## 为什么需要「review 门」
//!
//! 自动生成的技能**不能直接生效**:模型复盘出的「做法」可能是错的、
//! 过时的、或者只适用于那一次的具体情境。所以流程是:
//!
//! ```text
//! 轨迹 → 蒸馏候选 → review 门(人工/规则) → 写入 .zhuye/skills
//! ```
//!
//! [`SkillCandidate`] 带 `status`(pending / accepted / rejected)与
//! 来源轨迹引用。只有 `accepted` 才进技能库 —— 这条门是防「模型自我
//! 强化错误做法」的关键。
//!
//! ## 错误-解法缓存
//!
//! [`ErrorSolutionCache`] 记「这个错误 → 这样解决」的配对。与技能的区别:
//! 技能是**主动**的方法论,缓存是**被动**的查表(遇到同样错误直接给解法)。
//! 缓存按错误指纹匹配,不需要语义搜索。
//!
//! ## 备份与 undo
//!
//! 写入技能目录前先备份旧版本([`SkillWriteBackup`])—— 自动沉淀改坏了
//! 用户可以一键回滚。没有 undo 的自动写盘是不可接受的。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// 候选技能状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    /// 待审(默认)。
    #[default]
    Pending,
    /// 已接受 → 写入技能库。
    Accepted,
    /// 已拒绝(不再提示)。
    Rejected,
}

impl CandidateStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CandidateStatus::Pending => "pending",
            CandidateStatus::Accepted => "accepted",
            CandidateStatus::Rejected => "rejected",
        }
    }
}

/// 一条候选技能。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillCandidate {
    /// 技能名(文件名 stem,`[a-z0-9-]`)。
    pub name: String,
    /// 一句话描述(供模型判断何时使用)。
    pub description: String,
    /// 正文(方法论)。
    pub body: String,
    /// 来源轨迹引用(会话 id / 回合 id),便于回溯"这条技能从哪来"。
    pub source_trace: String,
    /// 出现次数(反复出现的做法优先沉淀)。
    pub occurrences: usize,
    pub status: CandidateStatus,
}

impl SkillCandidate {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        body: impl Into<String>,
        source_trace: impl Into<String>,
    ) -> SkillCandidate {
        SkillCandidate {
            name: name.into(),
            description: description.into(),
            body: body.into(),
            source_trace: source_trace.into(),
            occurrences: 1,
            status: CandidateStatus::Pending,
        }
    }

    /// 技能名是否合法(小写字母/数字/连字符,非空,不以连字符开头结尾)。
    pub fn name_is_valid(&self) -> bool {
        let n = &self.name;
        !n.is_empty()
            && n.len() <= 64
            && !n.starts_with('-')
            && !n.ends_with('-')
            && n.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    }

    /// 构造期校验(名称 + 非空正文)。
    pub fn validate(&self) -> Result<(), EvolutionError> {
        if !self.name_is_valid() {
            return Err(EvolutionError::InvalidName(self.name.clone()));
        }
        if self.body.trim().is_empty() {
            return Err(EvolutionError::EmptyBody(self.name.clone()));
        }
        if self.description.trim().is_empty() {
            return Err(EvolutionError::EmptyDescription(self.name.clone()));
        }
        Ok(())
    }

    /// 是否达到沉淀门槛(出现次数够 + 状态为 pending)。
    pub fn is_ready(&self, min_occurrences: usize) -> bool {
        self.status == CandidateStatus::Pending && self.occurrences >= min_occurrences
    }

    /// 渲染成 SKILL.md(含 frontmatter)。
    pub fn render_skill_md(&self) -> String {
        format!(
            "---\nname: {}\ndescription: {}\n---\n\n{}\n",
            self.name,
            self.description.replace('\n', " "),
            self.body.trim_end()
        )
    }
}

/// 进化流程错误。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EvolutionError {
    #[error("invalid skill name: {0:?} (expected [a-z0-9-], <=64 chars)")]
    InvalidName(String),
    #[error("skill {0:?} has empty body")]
    EmptyBody(String),
    #[error("skill {0:?} has empty description")]
    EmptyDescription(String),
    #[error("skill {0:?} is not accepted; cannot write")]
    NotAccepted(String),
}

/// 技能写盘备份记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillWriteBackup {
    pub name: String,
    /// 被覆盖的旧内容(`None` = 新建,原本不存在)。
    pub previous: Option<String>,
}

/// 候选队列(蒸馏 → 审阅 → 接受)。
#[derive(Debug, Default)]
pub struct CandidateQueue {
    items: HashMap<String, SkillCandidate>,
}

impl CandidateQueue {
    pub fn new() -> CandidateQueue {
        CandidateQueue::default()
    }

    /// 提交一个候选。同名候选累加 `occurrences`(反复出现)。
    ///
    /// 已被拒绝的候选**不复活**(用户说过不要了,不该反复打扰)。
    pub fn submit(&mut self, candidate: SkillCandidate) {
        match self.items.get_mut(&candidate.name) {
            Some(existing) => {
                if existing.status == CandidateStatus::Rejected {
                    return; // 已拒绝的不复活
                }
                existing.occurrences += candidate.occurrences;
                // 保留最新正文与描述(更完整的版本胜出)。
                if candidate.body.len() > existing.body.len() {
                    existing.body = candidate.body;
                }
                if candidate.description.len() > existing.description.len() {
                    existing.description = candidate.description;
                }
            }
            None => {
                self.items.insert(candidate.name.clone(), candidate);
            }
        }
    }

    /// 待审列表(按出现次数降序)。
    pub fn pending(&self) -> Vec<&SkillCandidate> {
        let mut out: Vec<&SkillCandidate> = self
            .items
            .values()
            .filter(|c| c.status == CandidateStatus::Pending)
            .collect();
        out.sort_by(|a, b| {
            b.occurrences
                .cmp(&a.occurrences)
                .then_with(|| a.name.cmp(&b.name))
        });
        out
    }

    /// 达到门槛的候选(可直接沉淀)。
    pub fn ready(&self, min_occurrences: usize) -> Vec<&SkillCandidate> {
        let mut out: Vec<&SkillCandidate> = self
            .items
            .values()
            .filter(|c| c.is_ready(min_occurrences))
            .collect();
        out.sort_by(|a, b| {
            b.occurrences
                .cmp(&a.occurrences)
                .then_with(|| a.name.cmp(&b.name))
        });
        out
    }

    /// 接受一个候选(进写盘流程)。
    pub fn accept(&mut self, name: &str) -> Result<&SkillCandidate, EvolutionError> {
        let item = self
            .items
            .get_mut(name)
            .ok_or_else(|| EvolutionError::NotAccepted(name.to_string()))?;
        item.status = CandidateStatus::Accepted;
        Ok(item)
    }

    /// 拒绝一个候选(不再提示)。
    pub fn reject(&mut self, name: &str) -> Result<(), EvolutionError> {
        let item = self
            .items
            .get_mut(name)
            .ok_or_else(|| EvolutionError::NotAccepted(name.to_string()))?;
        item.status = CandidateStatus::Rejected;
        Ok(())
    }

    /// 当前候选数。
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 取一个候选。
    pub fn get(&self, name: &str) -> Option<&SkillCandidate> {
        self.items.get(name)
    }
}

/// 错误-解法缓存。
///
/// 按错误**指纹**(归一化后的错误文本)查表,命中直接给解法。与技能不同,
/// 它不参与模型判断,只在错误再次出现时被检索。
#[derive(Debug, Default)]
pub struct ErrorSolutionCache {
    entries: HashMap<String, CachedSolution>,
}

/// 一条缓存解法。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedSolution {
    /// 原始错误文本(截断保留,供人读)。
    pub error_excerpt: String,
    /// 解法。
    pub solution: String,
    /// 命中次数。
    pub hits: usize,
}

impl ErrorSolutionCache {
    pub fn new() -> ErrorSolutionCache {
        ErrorSolutionCache::default()
    }

    /// 错误指纹:小写 + 去空白 + 截断(同错误不同缩进/路径前缀能匹配)。
    pub fn fingerprint(error: &str) -> String {
        let normalized: String = error
            .chars()
            .filter(|c| !c.is_whitespace())
            .take(160)
            .collect::<String>()
            .to_lowercase();
        normalized
    }

    /// 记录「错误 → 解法」。
    pub fn record(&mut self, error: &str, solution: impl Into<String>) {
        let key = Self::fingerprint(error);
        if key.is_empty() {
            return;
        }
        let new_solution: String = solution.into();
        let excerpt: String = error.chars().take(200).collect();
        let entry = self.entries.entry(key).or_insert_with(|| CachedSolution {
            error_excerpt: excerpt,
            solution: new_solution.clone(),
            hits: 0,
        });
        // 已有条目:更新解法(更长的解法通常更完整)。
        if new_solution.len() > entry.solution.len() {
            entry.solution = new_solution;
        }
    }

    /// 查解法(命中则 `hits += 1`)。
    pub fn lookup(&mut self, error: &str) -> Option<&CachedSolution> {
        let key = Self::fingerprint(error);
        let entry = self.entries.get_mut(&key)?;
        entry.hits += 1;
        Some(entry)
    }

    /// 只读查询(不计数)。
    pub fn peek(&self, error: &str) -> Option<&CachedSolution> {
        self.entries.get(&Self::fingerprint(error))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 按命中次数排序的 top 条目(用于「哪些坑最常踩」)。
    pub fn most_hit(&self, k: usize) -> Vec<(&str, &CachedSolution)> {
        let mut all: Vec<(&str, &CachedSolution)> =
            self.entries.iter().map(|(k, v)| (k.as_str(), v)).collect();
        all.sort_by(|a, b| b.1.hits.cmp(&a.1.hits).then_with(|| a.0.cmp(b.0)));
        all.truncate(k);
        all
    }
}

/// 准备写盘:返回 `(skill.md 内容, 备份记录)`。
///
/// **只对 accepted 的候选生成** —— pending / rejected 一律拒绝写盘,
/// 这是 review 门的落地点。
pub fn prepare_write(
    candidate: &SkillCandidate,
    existing: Option<String>,
) -> Result<(String, SkillWriteBackup), EvolutionError> {
    if candidate.status != CandidateStatus::Accepted {
        return Err(EvolutionError::NotAccepted(candidate.name.clone()));
    }
    candidate.validate()?;
    let content = candidate.render_skill_md();
    let backup = SkillWriteBackup {
        name: candidate.name.clone(),
        previous: existing,
    };
    Ok((content, backup))
}

/// 从备份恢复(undo)。
pub fn restore_from_backup(backup: &SkillWriteBackup) -> Option<String> {
    backup.previous.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(name: &str, occurrences: usize) -> SkillCandidate {
        let mut c = SkillCandidate::new(name, "when to use", "do the thing", "trace-1");
        c.occurrences = occurrences;
        c
    }

    #[test]
    fn name_validation() {
        assert!(cand("good-name", 1).name_is_valid());
        assert!(cand("a1", 1).name_is_valid());
        assert!(!cand("", 1).name_is_valid());
        assert!(!cand("Bad-Name", 1).name_is_valid(), "大写不合法");
        assert!(!cand("-lead", 1).name_is_valid());
        assert!(!cand("trail-", 1).name_is_valid());
        assert!(!cand("under_score", 1).name_is_valid());
    }

    #[test]
    fn validate_rejects_empty_fields() {
        let mut c = cand("ok-name", 1);
        c.body = "   ".into();
        assert!(matches!(c.validate(), Err(EvolutionError::EmptyBody(_))));

        let mut c = cand("ok-name", 1);
        c.description = String::new();
        assert!(matches!(
            c.validate(),
            Err(EvolutionError::EmptyDescription(_))
        ));
    }

    #[test]
    fn render_includes_frontmatter() {
        let md = cand("my-skill", 1).render_skill_md();
        assert!(md.starts_with("---\n"), "{md}");
        assert!(md.contains("name: my-skill"), "{md}");
        assert!(md.contains("description: when to use"), "{md}");
        assert!(md.contains("do the thing"), "{md}");
    }

    #[test]
    fn queue_accumulates_occurrences() {
        let mut q = CandidateQueue::new();
        q.submit(cand("s", 1));
        q.submit(cand("s", 2));
        assert_eq!(q.len(), 1);
        assert_eq!(q.get("s").unwrap().occurrences, 3);
    }

    #[test]
    fn queue_keeps_longer_body() {
        let mut q = CandidateQueue::new();
        let mut a = cand("s", 1);
        a.body = "short".into();
        q.submit(a);
        let mut b = cand("s", 1);
        b.body = "a much longer body".into();
        q.submit(b);
        assert_eq!(q.get("s").unwrap().body, "a much longer body");
    }

    #[test]
    fn rejected_does_not_revive() {
        let mut q = CandidateQueue::new();
        q.submit(cand("s", 1));
        q.reject("s").unwrap();
        q.submit(cand("s", 5));
        assert_eq!(q.get("s").unwrap().occurrences, 1, "拒绝后不再累加");
        assert!(q.pending().is_empty(), "拒绝项不在待审列表");
    }

    #[test]
    fn ready_threshold() {
        let mut q = CandidateQueue::new();
        q.submit(cand("rare", 1));
        q.submit(cand("frequent", 5));
        let ready = q.ready(3);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].name, "frequent");
    }

    #[test]
    fn pending_sorted_by_occurrences() {
        let mut q = CandidateQueue::new();
        q.submit(cand("low", 1));
        q.submit(cand("high", 9));
        q.submit(cand("mid", 4));
        let names: Vec<&str> = q.pending().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["high", "mid", "low"]);
    }

    #[test]
    fn accept_and_reject_unknown_errors() {
        let mut q = CandidateQueue::new();
        assert!(q.accept("ghost").is_err());
        assert!(q.reject("ghost").is_err());
    }

    #[test]
    fn prepare_write_requires_accepted() {
        let c = cand("s", 1); // pending
        assert!(matches!(
            prepare_write(&c, None),
            Err(EvolutionError::NotAccepted(_))
        ));
    }

    #[test]
    fn prepare_write_accepted_with_backup() {
        let mut c = cand("s", 1);
        c.status = CandidateStatus::Accepted;
        let (content, backup) = prepare_write(&c, Some("old content".into())).expect("write");
        assert!(content.contains("name: s"));
        assert_eq!(backup.previous.as_deref(), Some("old content"));
        assert_eq!(restore_from_backup(&backup).as_deref(), Some("old content"));
    }

    #[test]
    fn prepare_write_new_file_has_no_previous() {
        let mut c = cand("s", 1);
        c.status = CandidateStatus::Accepted;
        let (_, backup) = prepare_write(&c, None).expect("write");
        assert!(backup.previous.is_none());
        assert!(restore_from_backup(&backup).is_none());
    }

    #[test]
    fn error_cache_fingerprint_normalizes() {
        let a = ErrorSolutionCache::fingerprint("Error:   Foo  Bar");
        let b = ErrorSolutionCache::fingerprint("error:foobar");
        assert_eq!(a, b, "大小写与空白应归一");
    }

    #[test]
    fn error_cache_record_and_lookup() {
        let mut c = ErrorSolutionCache::new();
        c.record("E0432 unresolved import", "add the dependency");
        let hit = c.lookup("e0432   unresolved import").expect("hit");
        assert_eq!(hit.solution, "add the dependency");
        assert_eq!(hit.hits, 1);
        // 再次命中累加。
        c.lookup("E0432 unresolved import");
        assert_eq!(c.peek("E0432 unresolved import").unwrap().hits, 2);
    }

    #[test]
    fn error_cache_miss() {
        let mut c = ErrorSolutionCache::new();
        assert!(c.lookup("never seen").is_none());
    }

    #[test]
    fn error_cache_ignores_empty_error() {
        let mut c = ErrorSolutionCache::new();
        c.record("   ", "solution");
        assert!(c.is_empty());
    }

    #[test]
    fn error_cache_most_hit() {
        let mut c = ErrorSolutionCache::new();
        c.record("err-a", "sol-a");
        c.record("err-b", "sol-b");
        c.lookup("err-b");
        c.lookup("err-b");
        let top = c.most_hit(1);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].1.hits, 2);
    }

    #[test]
    fn error_cache_upgrades_to_longer_solution() {
        let mut c = ErrorSolutionCache::new();
        c.record("err", "short");
        c.record("err", "a much longer and better solution");
        assert_eq!(
            c.peek("err").unwrap().solution,
            "a much longer and better solution"
        );
    }
}
