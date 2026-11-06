//! # 中毒会话判定与退役(v1.9 C3)
//!
//! 与 multica `daemon/poisoned.go` 对齐:会话历史被"坏经验"污染后,
//! 压缩/恢复只会继承毒性 —— 唯一有效处置是**判定中毒并退役**,
//! 让下一轮从全新会话开始(笔记/记忆可迁移,对话历史不迁移)。
//!
//! ## 四类中毒(与 multica 一致)
//!
//! 1. **Output**(输出毒化):连续 N 轮模型输出指纹一致 —— 模型在
//!    空转复读,历史里堆满重复文本。
//! 2. **Error**(错误毒化):滑动窗口内工具错误率 ≥ 阈值(有最小
//!    样本量)—— 会话陷入"调用 → 失败"泥潭。
//! 3. **Timeout**(超时毒化):连续 N 轮出现工具超时。
//! 4. **Transport**(传输毒化):连续 N 轮异常终止(模型调用反复
//!    失败放弃;用户主动取消**不算**)。
//!
//! ## 退役语义
//!
//! 判定即退役([`PoisonDetector::record_turn`] 返回 [`PoisonVerdict`]
//! 后 detector 置 `retired`):调用方(submission_loop)收到裁决后
//! 应向客户端发出带 `session_poisoned` code 的 Error 事件,由宿主
//! 开新会话继续(带 notes/memory,不带对话历史)。已退役的 detector
//! 不再重复触发(`Some` 只返回一次)。

use std::collections::VecDeque;

use sha2::{Digest, Sha256};

/// 输出文本指纹(SHA-256 小写 hex;与 prompt_guard / audit 同算法)。
pub fn output_fingerprint(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// 对最终回复的所有文本块按顺序计算指纹，空回复不参与复读检测。
pub(crate) fn output_blocks_fingerprint(blocks: &[zhuye_protocol::ContentBlock]) -> Option<String> {
    let mut digest = Sha256::new();
    let mut has_text = false;
    for block in blocks {
        if let zhuye_protocol::ContentBlock::Text { text } = block {
            if !text.is_empty() {
                digest.update(text.as_bytes());
                has_text = true;
            }
        }
    }
    has_text.then(|| {
        let mut out = String::with_capacity(64);
        for byte in digest.finalize() {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    })
}

/// 中毒类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoisonClass {
    /// 输出毒化(连续同指纹输出)。
    Output,
    /// 错误毒化(错误率超阈值)。
    Error,
    /// 超时毒化(连续超时轮)。
    Timeout,
    /// 传输毒化(连续异常终止轮)。
    Transport,
}

impl PoisonClass {
    /// 稳定小写码(协议 / 审计用)。
    pub fn code(self) -> &'static str {
        match self {
            PoisonClass::Output => "output",
            PoisonClass::Error => "error",
            PoisonClass::Timeout => "timeout",
            PoisonClass::Transport => "transport",
        }
    }
}

/// 判定配置。
#[derive(Debug, Clone)]
pub struct PoisonConfig {
    /// Output:连续同指纹输出轮数阈值。
    pub output_identical_threshold: usize,
    /// Error:滑动窗口大小(轮)。
    pub error_window: usize,
    /// Error:错误率阈值(窗口内 tool_errors/tool_calls)。
    pub error_rate_threshold: f64,
    /// Error:最小样本量(窗口内总调用数低于此不判)。
    pub error_min_samples: u32,
    /// Timeout:连续超时轮数阈值。
    pub timeout_consecutive: usize,
    /// Transport:连续异常终止轮数阈值。
    pub transport_consecutive: usize,
}

impl Default for PoisonConfig {
    fn default() -> Self {
        PoisonConfig {
            output_identical_threshold: 6,
            error_window: 10,
            error_rate_threshold: 0.8,
            error_min_samples: 6,
            timeout_consecutive: 3,
            transport_consecutive: 5,
        }
    }
}

/// 一轮的观测证据(submission_loop 在 turn 结束时构造)。
#[derive(Debug, Clone, Default)]
pub struct TurnEvidence {
    /// 本轮最终 assistant 文本指纹(`None` = 无文本输出,不计入
    /// Output 检测 —— 纯工具轮不该让"复读"误判)。
    pub output_hash: Option<String>,
    /// 本轮工具调用总数。
    pub tool_calls: u32,
    /// 本轮工具错误数(is_error)。
    pub tool_errors: u32,
    /// 本轮工具超时数(metadata 带 timeout_ms)。
    pub tool_timeouts: u32,
    /// 本轮是否正常完成(自然结束 / 预算耗尽 / MaxIterations 都算
    /// "有结论";异常终止 = give-up 路径)。
    pub completed_with_outcome: bool,
}

/// 判定结果。
#[derive(Debug, Clone)]
pub struct PoisonVerdict {
    pub class: PoisonClass,
    /// 人可读原因(直接进 Error 事件 message)。
    pub reason: String,
}

/// 会话级中毒检测器。一个会话(submission_loop)一个实例。
pub struct PoisonDetector {
    config: PoisonConfig,
    /// 最近 N 轮输出指纹(Output 检测)。
    output_hashes: VecDeque<String>,
    /// 错误率滑动窗口:(calls, errors) 按轮。
    error_window: VecDeque<(u32, u32)>,
    /// 连续超时轮。
    timeout_streak: usize,
    /// 连续异常终止轮。
    transport_streak: usize,
    /// 已退役:判定只触发一次。
    retired: bool,
}

impl PoisonDetector {
    pub fn new(config: PoisonConfig) -> PoisonDetector {
        PoisonDetector {
            config,
            output_hashes: VecDeque::new(),
            error_window: VecDeque::new(),
            timeout_streak: 0,
            transport_streak: 0,
            retired: false,
        }
    }

    /// 默认配置。
    pub fn with_defaults() -> PoisonDetector {
        PoisonDetector::new(PoisonConfig::default())
    }

    /// 登记一轮证据并判定。返回 `Some(verdict)` 当且仅当**本次**首次
    /// 判定中毒(已退役后恒 `None`)。
    pub fn record_turn(&mut self, e: &TurnEvidence) -> Option<PoisonVerdict> {
        if self.retired {
            return None;
        }

        // ── Output:连续同指纹输出 ──
        match &e.output_hash {
            Some(h) => {
                // 与上一轮相同才延续;不同则窗口只留本轮。
                if self.output_hashes.back().is_some_and(|p| p == h) {
                    self.output_hashes.push_back(h.clone());
                } else {
                    self.output_hashes.clear();
                    self.output_hashes.push_back(h.clone());
                }
            }
            None => {
                // 无文本输出的轮打断"连续复读"链。
                self.output_hashes.clear();
            }
        }
        if self.output_hashes.len() >= self.config.output_identical_threshold {
            return Some(self.retire(PoisonClass::Output, format!(
                "session poisoned (output): 模型已连续 {} 轮输出完全相同的内容 —— 会话在空转复读,继续对话只会继承坏历史。建议退役本会话:新开会话,迁移 notes/memory,不迁移对话历史。",
                self.output_hashes.len()
            )));
        }

        // ── Error:错误率滑动窗口 ──
        self.error_window.push_back((e.tool_calls, e.tool_errors));
        while self.error_window.len() > self.config.error_window {
            self.error_window.pop_front();
        }
        if e.tool_calls > 0 {
            let total_calls: u64 = self.error_window.iter().map(|(c, _)| u64::from(*c)).sum();
            let total_errors: u64 = self.error_window.iter().map(|(_, er)| u64::from(*er)).sum();
            if total_calls > 0 && total_calls >= u64::from(self.config.error_min_samples) {
                let rate = total_errors as f64 / total_calls as f64;
                if rate >= self.config.error_rate_threshold {
                    return Some(self.retire(PoisonClass::Error, format!(
                        "session poisoned (error): 最近 {} 轮工具错误率 {:.0}%({}/{})超过阈值 {:.0}% —— 会话陷入调用-失败泥潭。建议退役本会话并从全新会话开始。",
                        self.error_window.len(),
                        rate * 100.0,
                        total_errors,
                        total_calls,
                        self.config.error_rate_threshold * 100.0
                    )));
                }
            }
        }

        // ── Timeout:连续超时轮 ──
        if e.tool_timeouts > 0 {
            self.timeout_streak += 1;
        } else {
            self.timeout_streak = 0;
        }
        if self.timeout_streak >= self.config.timeout_consecutive {
            return Some(self.retire(PoisonClass::Timeout, format!(
                "session poisoned (timeout): 连续 {} 轮出现工具超时 —— 执行环境已不可用或任务超纲。建议退役本会话。",
                self.timeout_streak
            )));
        }

        // ── Transport:连续异常终止轮 ──
        if e.completed_with_outcome {
            self.transport_streak = 0;
        } else {
            self.transport_streak += 1;
            if self.transport_streak >= self.config.transport_consecutive {
                return Some(self.retire(PoisonClass::Transport, format!(
                    "session poisoned (transport): 连续 {} 轮异常终止(模型调用反复失败放弃)—— 传输链路持续不可用。建议退役本会话。",
                    self.transport_streak
                )));
            }
        }

        None
    }

    fn retire(&mut self, class: PoisonClass, reason: String) -> PoisonVerdict {
        self.retired = true;
        PoisonVerdict { class, reason }
    }

    /// 是否已退役。
    pub fn is_retired(&self) -> bool {
        self.retired
    }

    /// 当前观测快照(可观测性:状态面板 / TUI)。
    pub fn snapshot(&self) -> PoisonSnapshot {
        PoisonSnapshot {
            retired: self.retired,
            identical_output_streak: self.output_hashes.len(),
            error_window_turns: self.error_window.len(),
            timeout_streak: self.timeout_streak,
            transport_streak: self.transport_streak,
        }
    }
}

/// 检测器状态快照。
#[derive(Debug, Clone, Copy)]
pub struct PoisonSnapshot {
    pub retired: bool,
    pub identical_output_streak: usize,
    pub error_window_turns: usize,
    pub timeout_streak: usize,
    pub transport_streak: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean_turn(hash_seed: usize) -> TurnEvidence {
        TurnEvidence {
            output_hash: Some(output_fingerprint(&format!("answer-{hash_seed}"))),
            tool_calls: 3,
            tool_errors: 0,
            tool_timeouts: 0,
            completed_with_outcome: true,
        }
    }

    #[test]
    fn no_tool_turns_expire_old_error_evidence() {
        let mut detector = PoisonDetector::with_defaults();
        let failed = TurnEvidence {
            tool_calls: 5,
            tool_errors: 5,
            completed_with_outcome: true,
            ..Default::default()
        };
        assert!(detector.record_turn(&failed).is_none());
        for _ in 0..10 {
            assert!(
                detector
                    .record_turn(&TurnEvidence {
                        completed_with_outcome: true,
                        ..Default::default()
                    })
                    .is_none()
            );
        }
        let failed_once = TurnEvidence {
            tool_calls: 1,
            tool_errors: 1,
            completed_with_outcome: true,
            ..Default::default()
        };
        assert!(detector.record_turn(&failed_once).is_none());
        assert!(!detector.is_retired());
        assert_eq!(detector.snapshot().error_window_turns, 10);
    }

    #[test]
    fn complete_output_fingerprint_includes_each_text_block() {
        use zhuye_protocol::ContentBlock;
        let first = [
            ContentBlock::text("共同开头"),
            ContentBlock::text("第一种结论"),
        ];
        let second = [
            ContentBlock::text("共同开头"),
            ContentBlock::text("第二种结论"),
        ];
        assert_ne!(
            output_blocks_fingerprint(&first),
            output_blocks_fingerprint(&second)
        );
        assert_eq!(
            output_blocks_fingerprint(&first),
            Some(output_fingerprint("共同开头第一种结论"))
        );
        assert_eq!(output_blocks_fingerprint(&[ContentBlock::text("")]), None);
    }

    #[test]
    fn healthy_session_never_verdicts() {
        let mut d = PoisonDetector::with_defaults();
        for i in 0..30 {
            assert!(
                d.record_turn(&clean_turn(i)).is_none(),
                "第 {i} 轮不同输出不该判定"
            );
        }
        assert!(!d.is_retired());
    }

    #[test]
    fn identical_output_streak_verdicts() {
        let mut d = PoisonDetector::with_defaults();
        let h = output_fingerprint("same answer");
        let mut verdict = None;
        for _ in 0..6 {
            verdict = d.record_turn(&TurnEvidence {
                output_hash: Some(h.clone()),
                completed_with_outcome: true,
                ..Default::default()
            });
        }
        let v = verdict.expect("第 6 轮同输出应判定");
        assert_eq!(v.class, PoisonClass::Output);
        assert!(v.reason.contains("output"));
        assert!(d.is_retired());
        // 退役后不再触发。
        assert!(
            d.record_turn(&TurnEvidence {
                output_hash: Some(h),
                completed_with_outcome: true,
                ..Default::default()
            })
            .is_none()
        );
    }

    #[test]
    fn no_output_breaks_output_chain() {
        let mut d = PoisonDetector::with_defaults();
        let h = output_fingerprint("same");
        for _ in 0..4 {
            d.record_turn(&TurnEvidence {
                output_hash: Some(h.clone()),
                completed_with_outcome: true,
                ..Default::default()
            });
        }
        // 无文本输出轮打断链。
        d.record_turn(&TurnEvidence {
            completed_with_outcome: true,
            ..Default::default()
        });
        // 之后再 4 轮同输出:链最长 4+1 <6 → 不判定。
        for _ in 0..4 {
            assert!(
                d.record_turn(&TurnEvidence {
                    output_hash: Some(h.clone()),
                    completed_with_outcome: true,
                    ..Default::default()
                })
                .is_none()
            );
        }
    }

    #[test]
    fn differing_output_resets_chain() {
        let mut d = PoisonDetector::with_defaults();
        let h = output_fingerprint("same");
        for _ in 0..5 {
            d.record_turn(&TurnEvidence {
                output_hash: Some(h.clone()),
                completed_with_outcome: true,
                ..Default::default()
            });
        }
        // 不同输出 → 链清空重来。
        assert!(
            d.record_turn(&TurnEvidence {
                output_hash: Some(output_fingerprint("different")),
                completed_with_outcome: true,
                ..Default::default()
            })
            .is_none()
        );
        assert_eq!(d.snapshot().identical_output_streak, 1);
    }

    #[test]
    fn high_error_rate_verdicts_after_min_samples() {
        let mut d = PoisonDetector::with_defaults();
        let poisoned = TurnEvidence {
            output_hash: Some(output_fingerprint("trying")),
            tool_calls: 3,
            tool_errors: 3,
            tool_timeouts: 0,
            completed_with_outcome: true,
        };
        // 窗口 10 / 最小样本 6:第 2 轮累计 6 次调用全错 → rate 1.0。
        let mut verdict = None;
        for _ in 0..2 {
            verdict = d.record_turn(&poisoned);
        }
        let v = verdict.expect("错误率 100% 应判定");
        assert_eq!(v.class, PoisonClass::Error);
    }

    #[test]
    fn low_sample_error_rate_does_not_verdict() {
        let mut d = PoisonDetector::with_defaults();
        // 每轮输出不同(隔离 Output 检测 —— 本测试只考察 Error 通道)。
        let poisoned = |i: usize| TurnEvidence {
            output_hash: Some(output_fingerprint(&format!("attempt-{i}"))),
            tool_calls: 1,
            tool_errors: 1,
            tool_timeouts: 0,
            completed_with_outcome: true,
        };
        // 每轮 1 次调用:前 5 轮共 5 样本 < min 6 → 不判。
        for i in 0..5 {
            assert!(
                d.record_turn(&poisoned(i)).is_none(),
                "5 样本不足不判(第 {i} 轮)"
            );
        }
        // 第 6 轮:样本 6,rate 1.0 → 判 Error(而非 Output)。
        let v = d.record_turn(&poisoned(5)).expect("样本达最小值后应判定");
        assert_eq!(v.class, PoisonClass::Error);
    }

    #[test]
    fn timeout_streak_verdicts() {
        let mut d = PoisonDetector::with_defaults();
        let timing_out = TurnEvidence {
            output_hash: Some(output_fingerprint("waiting")),
            tool_calls: 2,
            tool_errors: 0,
            tool_timeouts: 2,
            completed_with_outcome: true,
        };
        let mut verdict = None;
        for _ in 0..3 {
            verdict = d.record_turn(&timing_out);
        }
        let v = verdict.expect("连续 3 轮超时应判定");
        assert_eq!(v.class, PoisonClass::Timeout);
    }

    #[test]
    fn clean_turn_resets_timeout_streak() {
        let mut d = PoisonDetector::with_defaults();
        // 超时轮已完成(completed=true)→ 不混入 Transport 通道。
        let timing_out = |i: usize| TurnEvidence {
            output_hash: Some(output_fingerprint(&format!("w-{i}"))),
            tool_timeouts: 1,
            tool_calls: 1,
            tool_errors: 1,
            completed_with_outcome: true,
        };
        d.record_turn(&timing_out(1));
        d.record_turn(&timing_out(2));
        assert_eq!(d.snapshot().timeout_streak, 2);
        // 干净轮打断连击。
        d.record_turn(&clean_turn(1));
        assert_eq!(d.snapshot().timeout_streak, 0, "干净轮应清零超时连击");
        d.record_turn(&timing_out(3));
        d.record_turn(&timing_out(4));
        // 打断后只有 2 连(< threshold 3)→ 不判。
        assert!(
            d.record_turn(&timing_out(5)).is_some(),
            "第 3 个连续超时轮应判 Timeout"
        );
    }

    #[test]
    fn abnormal_turns_verdict_transport() {
        let mut d = PoisonDetector::with_defaults();
        let abnormal = TurnEvidence {
            output_hash: None,
            completed_with_outcome: false,
            ..Default::default()
        };
        let mut verdict = None;
        for _ in 0..5 {
            verdict = d.record_turn(&abnormal);
        }
        let v = verdict.expect("连续 5 轮异常终止应判定");
        assert_eq!(v.class, PoisonClass::Transport);
    }

    #[test]
    fn normal_turn_resets_transport_streak() {
        let mut d = PoisonDetector::with_defaults();
        let abnormal = TurnEvidence {
            completed_with_outcome: false,
            ..Default::default()
        };
        for _ in 0..4 {
            d.record_turn(&abnormal);
        }
        d.record_turn(&clean_turn(9));
        for _ in 0..4 {
            assert!(d.record_turn(&abnormal).is_none());
        }
    }

    #[test]
    fn priority_first_hit_wins() {
        // 输出与超时同时达阈值:先检查的 Output 胜(检查顺序即优先级:
        // Output → Error → Timeout → Transport)。
        let mut d = PoisonDetector::new(PoisonConfig {
            output_identical_threshold: 2,
            timeout_consecutive: 2,
            ..Default::default()
        });
        let both = TurnEvidence {
            output_hash: Some(output_fingerprint("x")),
            tool_calls: 2,
            tool_errors: 0,
            tool_timeouts: 2,
            completed_with_outcome: true,
        };
        d.record_turn(&both);
        let v = d.record_turn(&both).expect("应判定");
        assert_eq!(v.class, PoisonClass::Output, "Output 优先级最高");
    }

    #[test]
    fn output_fingerprint_is_stable_sha256() {
        assert_eq!(
            output_fingerprint("hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }
}
