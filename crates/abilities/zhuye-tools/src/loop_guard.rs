//! # 循环失控守卫(v1.9 C1+C2)
//!
//! 与 Orkas `loop-guards` / `progress-evidence`、deepseek-harness
//! `repeat-tool-reminder` 对齐:**重复调用**与**无进展**分开检测,
//! 双档处置(advisory 提醒 → 阻断)。
//!
//! ## 双档
//!
//! - **软档 Advise**([`LoopGuardConfig::warn_threshold`]):同一
//!   (tool, args) 指纹在滑动窗口内出现 ≥ N 次 → 放行执行,但在
//!   工具输出前注入 advisory 文本,提醒模型"你在重复同一调用"。
//!   (deepseek-harness 的 repeat-tool-reminder 语义:advisory,
//!   参数预览截断。)
//! - **硬档 Block**([`LoopGuardConfig::block_threshold`]):重复
//!   达到硬线 **且** 进度证据显示无进展(连续同参调用的输出指纹
//!   一致)→ 不执行,直接返回阻断错误文本 —— 模型必须换策略。
//!
//! ## 进度证据(Orkas progress-evidence 语义)
//!
//! 关键区分:同参数重复 + **输出一直在变** = 系统在被推动(例如
//! `sleep 1` 轮询构建进度)→ 只提醒不阻断;同参数重复 + **输出
//! 也一致** = 真失控 → 阻断。输出证据由 [`LoopGuard::record_result`]
//! 在每次执行后登记(输出文本 SHA-256)。
//!
//! ## 窗口
//!
//! 滑动窗口([`LoopGuardConfig::window`],默认 64 次调用)内统计
//! 重复;窗口外的历史不参与计数(长会话不会永远背着旧账)。

use std::collections::{HashMap, VecDeque};

use sha2::{Digest, Sha256};

/// 守卫配置。
#[derive(Debug, Clone)]
pub struct LoopGuardConfig {
    /// 软阈值:窗口内同指纹次数 ≥ 该值 → advisory。
    pub warn_threshold: usize,
    /// 硬阈值:窗口内同指纹次数 ≥ 该值 且 无进展证据 → 阻断。
    pub block_threshold: usize,
    /// advisory 里参数预览的最大字符数。
    pub preview_chars: usize,
    /// 滑动窗口大小(最近 N 次工具调用);0 表示不记录历史。
    pub window: usize,
}

impl Default for LoopGuardConfig {
    fn default() -> Self {
        LoopGuardConfig {
            warn_threshold: 3,
            block_threshold: 6,
            preview_chars: 200,
            window: 64,
        }
    }
}

/// 裁决。
#[derive(Debug, Clone, PartialEq)]
pub enum GuardDecision {
    /// 正常放行。
    Proceed,
    /// 放行 + 注入 advisory(工具输出前插入提醒文本)。
    Advise { message: String },
    /// 阻断:不执行,返回错误文本。
    Block { message: String },
}

#[derive(Debug)]
struct FpState {
    /// 窗口内出现次数。
    count: usize,
    /// 参数预览(截断)。
    preview: String,
    /// 上一次执行的输出指纹(进度证据)。
    last_result_hash: Option<String>,
    /// 连续同参调用且输出一致的次数(无进展连击)。
    same_result_streak: u32,
}

/// 按字符数截断(不切多字节字符),超长加省略号。
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}

/// 循环失控守卫。会话级共享(跨 turn 记账),`Mutex` 外置。
#[derive(Debug)]
pub struct LoopGuard {
    /// 配置(crate 内可读,queue 取 preview_chars 截断参数预览)。
    pub(crate) config: LoopGuardConfig,
    ring: VecDeque<String>,
    states: HashMap<String, FpState>,
}

impl LoopGuard {
    pub fn new(config: LoopGuardConfig) -> LoopGuard {
        LoopGuard {
            config,
            ring: VecDeque::new(),
            states: HashMap::new(),
        }
    }

    /// 默认配置(warn=3 / block=6 / preview=200 / window=64)。
    pub fn with_defaults() -> LoopGuard {
        LoopGuard::new(LoopGuardConfig::default())
    }

    /// 规范化参数指纹(SHA-256(canonical json))。serde_json 默认
    /// BTreeMap 序列化即键有序,同一逻辑参数得到同一串。
    pub fn canonical_args_hash(args: &serde_json::Value) -> String {
        let s = serde_json::to_string(args).unwrap_or_default();
        let digest = Sha256::digest(s.as_bytes());
        let mut out = String::with_capacity(digest.len() * 2);
        for b in digest {
            out.push_str(&format!("{b:02x}"));
        }
        out
    }

    fn fingerprint(tool: &str, args_hash: &str) -> String {
        format!("{tool}\u{1}{args_hash}")
    }

    /// 哈希版 observe(调用方可预先算 hash,避免 args 克隆)。
    /// `preview` 为参数预览文本(调用方按 `config.preview_chars` 截断),
    /// 仅在首次登记时保存;advisory / 阻断消息里会带上,便于模型和
    /// 用户定位"是哪一个重复调用"。
    pub fn observe_hashed(
        &mut self,
        tool: &str,
        args_hash: &str,
        preview: Option<&str>,
    ) -> GuardDecision {
        if self.config.window == 0 {
            return GuardDecision::Proceed;
        }
        let fp = Self::fingerprint(tool, args_hash);

        // 窗口推进 + 驱逐。
        self.ring.push_back(fp.clone());
        if let Some(state) = self.states.get_mut(&fp) {
            state.count += 1;
        } else {
            // preview 只在首次登记时留(后续相同)。
            self.states.insert(
                fp.clone(),
                FpState {
                    count: 1,
                    preview: preview.unwrap_or_default().to_string(),
                    last_result_hash: None,
                    same_result_streak: 0,
                },
            );
        }
        while self.ring.len() > self.config.window {
            if let Some(old) = self.ring.pop_front() {
                if old != fp {
                    // 驱逐的非本次指纹:计数减一,归零删除。
                    if let Some(s) = self.states.get_mut(&old) {
                        s.count = s.count.saturating_sub(1);
                        if s.count == 0 {
                            self.states.remove(&old);
                        }
                    }
                } else {
                    // 极小窗口下同指纹被挤:计数减一(本次仍在 map)。
                    if let Some(s) = self.states.get_mut(&fp) {
                        s.count = s.count.saturating_sub(1);
                    }
                }
            }
        }

        let count = self.states.get(&fp).map(|s| s.count).unwrap_or(1);
        let (same_streak, preview) = self
            .states
            .get(&fp)
            .map(|s| (s.same_result_streak, s.preview.clone()))
            .unwrap_or((0, String::new()));
        let preview_line = if preview.is_empty() {
            String::new()
        } else {
            format!("\n参数预览: {preview}")
        };

        // 决策发生在本次执行前；硬线为 N 时，前 N-1 次执行的输出需一致。
        let required_streak = self.config.block_threshold.saturating_sub(2).max(1);
        if count >= self.config.block_threshold && (same_streak as usize) >= required_streak {
            return GuardDecision::Block {
                message: format!(
                    "[loop-guard] blocked: {tool} 已用完全相同的参数被调用 {count} 次\
                     (窗口 {w}),且最近输出反复一致 —— 没有新的进展证据。{preview_line}\
                     继续重复只会得到相同结果。请改变参数、换一个工具、\
                     或先向用户说明卡点。",
                    w = self.config.window
                ),
            };
        }
        // 软档:重复达软线 → advisory(有进展证据的重复只是提醒)。
        if count >= self.config.warn_threshold {
            return GuardDecision::Advise {
                message: format!(
                    "[loop-guard] advisory: {tool} 已用完全相同的参数被调用 {count} 次。\
                     {preview_line}若是轮询/等待类操作可继续,否则请考虑改变参数或换策略。"
                ),
            };
        }
        GuardDecision::Proceed
    }

    /// 便捷入口:有 args 引用时自动算 hash + 预览并调用
    /// [`LoopGuard::observe_hashed`]。
    pub fn observe(&mut self, tool: &str, args: &serde_json::Value) -> GuardDecision {
        let hash = Self::canonical_args_hash(args);
        let preview = truncate_chars(
            &serde_json::to_string(args).unwrap_or_default(),
            self.config.preview_chars,
        );
        self.observe_hashed(tool, &hash, Some(&preview))
    }

    /// 执行后调用:登记输出指纹(进度证据)。同参调用输出一致 →
    /// 无进展连击 +1;不一致 → 清零，之后可重新累计。
    pub fn record_result(&mut self, tool: &str, args: &serde_json::Value, result_hash: &str) {
        let h = Self::canonical_args_hash(args);
        self.record_result_hashed(tool, &h, result_hash)
    }

    /// 哈希版 record_result。
    pub fn record_result_hashed(&mut self, tool: &str, args_hash: &str, result_hash: &str) {
        let fp = Self::fingerprint(tool, args_hash);
        if let Some(state) = self.states.get_mut(&fp) {
            match &state.last_result_hash {
                Some(prev) if prev == result_hash => {
                    state.same_result_streak += 1;
                }
                Some(_) => {
                    // 输出变了:有进展证据。
                    state.same_result_streak = 0;
                }
                None => {}
            }
            state.last_result_hash = Some(result_hash.to_string());
        }
    }

    /// 输出文本指纹(SHA-256),供调用方把工具输出折进进度证据。
    pub fn result_hash(text: &str) -> String {
        let digest = Sha256::digest(text.as_bytes());
        let mut out = String::with_capacity(digest.len() * 2);
        for b in digest {
            out.push_str(&format!("{b:02x}"));
        }
        out
    }

    /// 清空全部状态(用户显式介入/steering 后重置)。
    pub fn reset(&mut self) {
        self.ring.clear();
        self.states.clear();
    }

    /// 当前窗口内最大重复次数(可观测/测试用)。
    pub fn max_repeat_in_window(&self) -> usize {
        self.states.values().map(|s| s.count).max().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn guard() -> LoopGuard {
        LoopGuard::with_defaults()
    }

    #[test]
    fn first_call_proceeds() {
        let mut g = guard();
        assert_eq!(
            g.observe("bash", &json!({"cmd": "ls"})),
            GuardDecision::Proceed
        );
    }

    #[test]
    fn identical_args_count_toward_warn() {
        let mut g = guard();
        let args = json!({"cmd": "cargo build"});
        assert_eq!(g.observe("bash", &args), GuardDecision::Proceed);
        assert_eq!(g.observe("bash", &args), GuardDecision::Proceed);
        // 第 3 次(窗口内第 3 次出现)→ advisory。
        assert!(matches!(
            g.observe("bash", &args),
            GuardDecision::Advise { .. }
        ));
    }

    #[test]
    fn different_args_do_not_count() {
        let mut g = guard();
        for i in 0..5 {
            let args = json!({"cmd": format!("echo {i}")});
            let d = g.observe("bash", &args);
            assert_eq!(d, GuardDecision::Proceed, "第 {i} 次不同参数不应触发");
        }
    }

    #[test]
    fn no_progress_reaches_block() {
        let mut g = guard();
        let args = json!({"cmd": "cargo build"});
        let result = LoopGuard::result_hash("error: compile failed");
        // 前 5 次:proceed/proceed/advise/advise/advise(block=6,同输出)。
        let mut decisions = Vec::new();
        for _ in 0..5 {
            decisions.push(g.observe("bash", &args));
            g.record_result("bash", &args, &result);
        }
        assert!(decisions[..2].iter().all(|d| *d == GuardDecision::Proceed));
        assert!(
            decisions[2..]
                .iter()
                .all(|d| matches!(d, GuardDecision::Advise { .. }))
        );
        // 第 6 次:达硬线 + 无进展 → Block。
        assert!(matches!(
            g.observe("bash", &args),
            GuardDecision::Block { .. }
        ));
    }

    #[test]
    fn progressing_output_softens_to_advise() {
        // 进度证据:同参数重复但输出一直在变(轮询场景)→ 永不 Block。
        let mut g = guard();
        let args = json!({"cmd": "sleep 1 && tail build.log"});
        for i in 0..20 {
            let d = g.observe("bash", &args);
            assert!(
                matches!(d, GuardDecision::Proceed | GuardDecision::Advise { .. }),
                "有进展的重复第 {i} 次不得 Block,实际 {d:?}"
            );
            g.record_result(
                "bash",
                &args,
                &LoopGuard::result_hash(&format!("progress {i}")),
            );
        }
    }

    #[test]
    fn window_eviction_resets_counts() {
        let mut g = LoopGuard::new(LoopGuardConfig {
            warn_threshold: 3,
            block_threshold: 6,
            preview_chars: 200,
            window: 4,
        });
        let args = json!({"cmd": "loop"});
        for _ in 0..4 {
            g.observe("bash", &args);
        }
        assert_eq!(g.max_repeat_in_window(), 4);
        // 4 个不同调用把同指纹挤出窗口 → 计数清零删除。
        for i in 0..4 {
            g.observe("bash", &json!({"cmd": format!("other-{i}")}));
        }
        assert_eq!(g.max_repeat_in_window(), 1, "旧指纹应被驱逐");
        assert!(
            matches!(g.observe("bash", &args), GuardDecision::Proceed),
            "驱逐后重新计数,不背旧账"
        );
    }

    #[test]
    fn zero_window_never_retains_or_warns_about_calls() {
        let mut guard = LoopGuard::new(LoopGuardConfig {
            warn_threshold: 0,
            block_threshold: 0,
            preview_chars: 200,
            window: 0,
        });
        for i in 0..1000 {
            let args = json!({"value": i});
            assert_eq!(guard.observe("echo", &args), GuardDecision::Proceed);
            guard.record_result("echo", &args, "same");
        }
        assert!(guard.ring.is_empty());
        assert!(guard.states.is_empty());
        assert_eq!(guard.max_repeat_in_window(), 0);
    }

    #[test]
    fn reset_clears_everything() {
        let mut g = guard();
        let args = json!({"cmd": "x"});
        g.observe("bash", &args);
        g.observe("bash", &args);
        g.observe("bash", &args);
        assert!(g.max_repeat_in_window() >= 3);
        g.reset();
        assert_eq!(g.max_repeat_in_window(), 0);
        assert_eq!(g.observe("bash", &args), GuardDecision::Proceed);
    }

    #[test]
    fn args_hash_is_canonical_and_stable() {
        // serde_json 默认 BTreeMap:键序无关。
        let a = json!({"cmd": "ls", "x": 1});
        let b = json!({"x": 1, "cmd": "ls"});
        assert_eq!(
            LoopGuard::canonical_args_hash(&a),
            LoopGuard::canonical_args_hash(&b)
        );
        let c = json!({"cmd": "ls -l"});
        assert_ne!(
            LoopGuard::canonical_args_hash(&a),
            LoopGuard::canonical_args_hash(&c)
        );
    }

    #[test]
    fn block_message_carries_tool_and_count() {
        let mut g = LoopGuard::new(LoopGuardConfig {
            warn_threshold: 1,
            block_threshold: 2,
            preview_chars: 200,
            window: 64,
        });
        let args = json!({"cmd": "stuck"});
        // 第 1 次:count=1 ≥ warn(1) → Advise;record(同输出) → 首条无连击。
        assert!(matches!(
            g.observe("bash", &args),
            GuardDecision::Advise { .. }
        ));
        g.record_result("bash", &args, &LoopGuard::result_hash("same"));
        // 第 2 次:count=2 ≥ block(2),same_streak=0 → 仍 Advise。
        assert!(matches!(
            g.observe("bash", &args),
            GuardDecision::Advise { .. }
        ));
        // 第二次同输出 → 连击 1。
        g.record_result("bash", &args, &LoopGuard::result_hash("same"));
        match g.observe("bash", &args) {
            GuardDecision::Block { message } => {
                assert!(message.contains("bash"));
                assert!(message.contains("3 次"));
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn different_tools_same_args_are_independent() {
        let mut g = guard();
        let args = json!({"path": "a.rs"});
        g.observe("read", &args);
        g.observe("read", &args);
        // read 已 2 次,但 grep 是第一次。
        assert_eq!(g.observe("grep", &args), GuardDecision::Proceed);
    }

    #[test]
    fn ever_progressed_then_stuck_needs_streak_again() {
        // 先有进展，后续稳定输出应重新累计无进展连击。
        let mut g = guard();
        let args = json!({"cmd": "watch"});
        g.observe("bash", &args);
        g.record_result("bash", &args, "out-v1");
        g.observe("bash", &args);
        g.record_result("bash", &args, "out-v2"); // 有进展
        for _ in 0..4 {
            assert!(!matches!(
                g.observe("bash", &args),
                GuardDecision::Block { .. }
            ));
            g.record_result("bash", &args, "out-v2");
        }
        let d = g.observe("bash", &args);
        assert!(
            matches!(d, GuardDecision::Block { .. }),
            "输出再次停滞后应阻断,实际 {d:?}"
        );
    }
}
