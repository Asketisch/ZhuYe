//! `Compactor` —— 串联 microcompact → smart_prune → LLM-summarize 三级压缩。
//!
//! 对应 Zhuye 的递进式压缩策略:
//! 1. 估算总 token 数。
//! 2. 若低于 `trigger * microcompact_trigger_ratio` → `Noop`。
//! 3. 执行 microcompact。若当前低于 `trigger` → 返回 `Microcompact`。
//! 4. 执行 smart_prune。若当前低于 `target` → 返回 `SmartPrune`。
//! 5. 若 `summarize_after` 为 true,调用 LLM 摘要器。若摘要结果低于
//!    `target` → 返回 `LlMSummarize`。
//! 6. 回退到 smart_prune 的尽力而为结果;若两个本地阶段都没有改动,
//!    整次判定降级为
//!    `Noop` —— 空压谎报非空策略会让调用方误判「压缩已发生」,给
//!    rapid-refill 熔断器上膛后把正常 turn 连击误杀。
//!
//! 无环依赖:仅依赖 `zhuye-llm` 的类型。具体 `Summarizer` 由调用方注入
//! (通常是在 `zhuye-exec` 中构造的 `LlmSummarizer`)。

use std::sync::Arc;

use zhuye_llm::ChatMessage;
use zhuye_protocol::{ContextCompactedEvent, ContextCompactedStrategy};

use crate::microcompact::{MicrocompactConfig, microcompact};
use crate::smart_prune::{SmartPruneConfig, smart_prune};
use crate::summarizer::Summarizer;
use crate::tokens::{calibration_scale, estimate_messages, scaled_tokens};

/// 默认触发阈值。M5 v0 默认 `max_estimated_tokens = 10000`,以便在长对话
/// 中频繁触发 microcompact;此前的 M4 默认值为 160_000。运维人员可通过
/// `ZHUYE_AUTO_COMPACT_INPUT_TOKENS` 环境变量覆盖。
pub const DEFAULT_TRIGGER_TOKENS: u32 = 10_000;

/// 压缩器的可调参数。
#[derive(Debug, Clone)]
pub struct CompactorConfig {
    /// 触发压缩的总 token 阈值。
    pub trigger_tokens: u32,
    /// 低于 `trigger * ratio` 时视为无需压缩(no-op)。
    pub microcompact_ratio: f32,
    /// microcompact 阶段的 `keep_recent`。
    pub keep_recent_microcompact: usize,
    /// smart_prune 阶段的 `keep_recent`。
    pub keep_recent_smart_prune: usize,
    /// smart_prune 阶段的 `target_tokens`(默认 = trigger * 0.75)。
    pub target_ratio: f32,
    /// smart_prune 之后仍超出预算时,是否调用 LLM 摘要器。
    pub summarize_after: bool,
}

impl Default for CompactorConfig {
    fn default() -> Self {
        Self {
            trigger_tokens: DEFAULT_TRIGGER_TOKENS,
            microcompact_ratio: 0.7,
            keep_recent_microcompact: 30,
            keep_recent_smart_prune: 40,
            target_ratio: 0.75,
            summarize_after: true,
        }
    }
}

impl CompactorConfig {
    /// 由 `trigger * target_ratio` 计算 smart_prune 目标 token 数。
    pub fn target_tokens(&self) -> u32 {
        ((self.trigger_tokens as f32) * self.target_ratio) as u32
    }
}

/// 压缩编排器。持有配置与外部注入的摘要器。
pub struct Compactor {
    cfg: CompactorConfig,
    summarizer: Arc<dyn Summarizer>,
    trigger_policy: Arc<std::sync::RwLock<Option<TriggerPolicy>>>,
    /// v1.4 D2:记忆固化目标。`Some` 时 LLM 摘要成功后追加一步记忆
    /// 提取(extract_memories),把长期事实条目 append 进目标 scope。
    /// `None`(默认)= 关闭,零开销。
    consolidation: Option<ConsolidationTarget>,
}

/// 运行时按模型窗口重新计算，或使用明确指定的固定阈值。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TriggerPolicy {
    Fixed(u32),
    WindowRatio(f32),
}

/// 引擎内部压缩结果。只有 Applied 携带改动后的消息。
pub enum CompactionOutcome {
    Noop {
        event: ContextCompactedEvent,
        reason: &'static str,
    },
    Applied {
        messages: Vec<ChatMessage>,
        event: ContextCompactedEvent,
        memory_write: Option<MemoryWriteInfo>,
        micro_changed: usize,
        pruned: usize,
        truncated: usize,
        /// 校准后的水位是否低于触发阈值。只有这时才允许给快速回填守卫上膛。
        below_trigger: bool,
    },
}

fn log_compaction_decision(
    stage: &'static str,
    local: u32,
    scale: f64,
    reported: Option<u32>,
    threshold: u32,
    reason: &'static str,
) {
    tracing::info!(
        stage,
        local_tokens = local,
        scaled_tokens = scaled_tokens(local, scale),
        reported_tokens = reported,
        threshold,
        reason,
        "上下文压缩决策"
    );
}

fn below_trigger(local_after: u32, scale: f64, trigger: u32) -> bool {
    scaled_tokens(local_after, scale) < trigger
}

impl CompactionOutcome {
    fn into_legacy(
        self,
    ) -> (
        Option<Vec<ChatMessage>>,
        ContextCompactedEvent,
        Option<MemoryWriteInfo>,
    ) {
        match self {
            Self::Noop { event, .. } => (None, event, None),
            Self::Applied {
                messages,
                event,
                memory_write,
                ..
            } => (Some(messages), event, memory_write),
        }
    }
}

/// v1.4 D2:记忆固化目标(store + scope + agent 名)。
#[derive(Clone)]
pub struct ConsolidationTarget {
    pub store: Arc<dyn zhuye_memory::MemoryStore>,
    pub scope: zhuye_memory::MemoryScope,
    pub agent: String,
}

impl std::fmt::Debug for Compactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Compactor")
            .field("cfg", &self.cfg)
            .field("summarizer", &"<dyn Summarizer>")
            .finish()
    }
}

impl Compactor {
    /// 使用给定配置和摘要器创建新压缩器。
    pub fn new(cfg: CompactorConfig, summarizer: Arc<dyn Summarizer>) -> Self {
        Self {
            cfg,
            summarizer,
            trigger_policy: Arc::new(std::sync::RwLock::new(None)),
            consolidation: None,
        }
    }

    /// v1.x:压缩触发水位(只读)。调用方(rapid-refill 熔断器预检)据
    /// 此廉价判定「本次 pre_loop 是否会真的触发压缩」—— 只有真触发才
    /// 计入 refill 连击,避免把未超阈值的普通步也当 refill。
    pub fn trigger_tokens(&self) -> u32 {
        self.cfg.trigger_tokens
    }

    /// 更新共享策略；调用方在配置热重载时使用。
    pub fn set_trigger_policy(&self, policy: TriggerPolicy) {
        *self
            .trigger_policy
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(policy);
    }

    /// 返回阈值来源类别；没有动态策略的压缩器保留构造时配置。
    pub fn trigger_source(&self) -> &'static str {
        match *self
            .trigger_policy
            .read()
            .unwrap_or_else(|e| e.into_inner())
        {
            Some(TriggerPolicy::Fixed(_)) => "explicit",
            Some(TriggerPolicy::WindowRatio(_)) => "window_ratio",
            None => "configured",
        }
    }

    /// 为一次决策固定阈值，避免并发模型切换使守卫与压缩器口径不同。
    pub fn for_window(&self, window: Option<u32>) -> Self {
        let policy = *self
            .trigger_policy
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let mut cfg = self.cfg.clone();
        cfg.trigger_tokens = match policy {
            Some(TriggerPolicy::Fixed(value)) => value,
            Some(TriggerPolicy::WindowRatio(ratio)) => {
                let ratio = if ratio.is_finite() && ratio > 0.0 {
                    ratio.min(1.0)
                } else {
                    0.85
                };
                (((window.unwrap_or(128_000) as f64) * (ratio as f64)) as u32).max(1_024)
            }
            None => cfg.trigger_tokens,
        };
        Self {
            cfg,
            summarizer: Arc::clone(&self.summarizer),
            trigger_policy: Arc::clone(&self.trigger_policy),
            consolidation: self.consolidation.clone(),
        }
    }

    /// v1.4 D2:挂载记忆固化目标(bootstrap 在配置开启时调用)。
    pub fn with_memory_consolidation(
        mut self,
        store: Arc<dyn zhuye_memory::MemoryStore>,
        scope: zhuye_memory::MemoryScope,
        agent: impl Into<String>,
    ) -> Self {
        self.consolidation = Some(ConsolidationTarget {
            store,
            scope,
            agent: agent.into(),
        });
        self
    }

    /// 执行压缩。返回压缩后的消息列表与记录所选策略的
    /// `ContextCompactedEvent`。
    ///
    /// 事件包含 `removed_messages`、`before_tokens`、`after_tokens`。
    /// `strategy` 取值为 `Noop` / `Microcompact` / `SmartPrune` /
    /// `LlMSummarize` 之一。
    ///
    /// `prev_summary` 是上一次 LLM 生成的摘要(若有)—— 当为 `Some` 时,
    /// LLM-summarize 步骤调用 `summarize_recent` 而非 `summarize_full`,
    /// 产生增量更新而非从零重新摘要。
    pub async fn compact(
        &self,
        messages: &[ChatMessage],
    ) -> (Option<Vec<ChatMessage>>, ContextCompactedEvent) {
        let (out, evt, _memory_write) = self
            .compact_with_prior_and_tokens(messages, None, None)
            .await;
        (out, evt)
    }

    /// 完整入口。当 `llm_reported_input_tokens` 为 `Some(n)` 时,
    /// 其值取自 LLM 上一次 `Usage` 事件的输入 token 数(Anthropic 的
    /// `input_tokens` 已包含 `cache_creation` 子段,因此即为线上计费的
    /// 输入量)。当为 `None` 或小于本地估算时,使用本地
    /// `estimate_messages` 启发式作为保守回退。触发阈值取
    /// `max(estimate, llm_reported)`,因此**任一**信号越界都会触发——
    /// 这是安全的默认行为,因为:
    /// - 若 LLM 异常地报告 0,本地估算仍能捕获超大 prompt;
    /// - 若本地估算偏低(如尚未计入系统工具 + skills 目录),
    ///   LLM 报告的总量是权威值。
    ///
    /// v1.x:第三返回值是本次压缩期间发生的记忆固化写入摘要 —— `Some`
    /// 表示有事实被 append 进 MEMORY.md(供上层落盘审计);无固化 /
    /// 未配置 consolidation 时为 `None`。
    ///
    /// 消息入参为借用;返回的消息列表为 `Option` —— `None` 表示未触发
    /// 压缩(Noop),调用方保持原列表不回写。只有真正触发压缩时才做
    /// 一次 `to_vec()`:压缩判定每迭代执行,绝大多数迭代走 Noop,若
    /// 按值收 `Vec` 则每次迭代都要为「借一下」深拷贝整个会话历史。
    pub async fn compact_with_prior_and_tokens(
        &self,
        messages: &[ChatMessage],
        prev_summary: Option<&str>,
        llm_reported_input_tokens: Option<u32>,
    ) -> (
        Option<Vec<ChatMessage>>,
        ContextCompactedEvent,
        Option<MemoryWriteInfo>,
    ) {
        self.compact_outcome(messages, prev_summary, llm_reported_input_tokens, None)
            .await
            .into_legacy()
    }

    /// 以明确的动作结果供宿主驱动事件、回写和守卫。
    ///
    /// v1.x:`focus` 是 `/compact <指示>` 的用户附加指示 —— `Some` 时注入
    /// 摘要 prompt(「## 用户附加指示」段),把摘要引向用户关心的部分;
    /// `None` = 普通压缩,行为不变。
    pub async fn compact_outcome(
        &self,
        messages: &[ChatMessage],
        prev_summary: Option<&str>,
        llm_reported_input_tokens: Option<u32>,
        focus: Option<&str>,
    ) -> CompactionOutcome {
        let local_estimate = estimate_messages(messages);
        let reported = llm_reported_input_tokens.filter(|n| *n > 0 && *n < u32::MAX);
        let scale = calibration_scale(local_estimate, reported);
        let before_tokens = match llm_reported_input_tokens {
            Some(n) => n.max(local_estimate),
            None => local_estimate,
        };
        let micro_threshold =
            ((self.cfg.trigger_tokens as f32) * self.cfg.microcompact_ratio) as u32;

        // 第 1 步:低于 micro 阈值 → noop(不回写,调用方保持原列表)。
        if before_tokens < micro_threshold {
            log_compaction_decision(
                "noop",
                local_estimate,
                scale,
                reported,
                micro_threshold,
                "below_threshold",
            );
            return CompactionOutcome::Noop {
                event: ContextCompactedEvent {
                    strategy: ContextCompactedStrategy::Noop,
                    removed_messages: 0,
                    before_tokens,
                    after_tokens: before_tokens,
                },
                reason: "below_threshold",
            };
        }

        // 第 2 步:microcompact(仅在真正触发压缩时克隆一次)。
        let mc_cfg = MicrocompactConfig {
            trigger_tokens: self.cfg.trigger_tokens,
            keep_recent: self.cfg.keep_recent_microcompact,
            trigger_ratio: self.cfg.microcompact_ratio,
            calibration_scale: scale,
        };
        let (after_mc, mc_report) = microcompact(messages.to_vec(), &mc_cfg);
        if !mc_report.was_compacted {
            // Microcompact 提前退出(例如 n <= keep_recent)。
            // 直接尝试 smart_prune。
        } else if scaled_tokens(estimate_messages(&after_mc), scale) < self.cfg.trigger_tokens {
            let after_tokens = estimate_messages(&after_mc);
            log_compaction_decision(
                "microcompact",
                after_tokens,
                scale,
                reported,
                self.cfg.trigger_tokens,
                "below_trigger",
            );
            return CompactionOutcome::Applied {
                messages: after_mc,
                event: ContextCompactedEvent {
                    strategy: ContextCompactedStrategy::Microcompact,
                    removed_messages: mc_report.removed_count,
                    before_tokens,
                    after_tokens,
                },
                memory_write: None,
                micro_changed: mc_report.removed_count,
                pruned: 0,
                truncated: 0,
                below_trigger: true,
            };
        }

        // 第 3 步:smart_prune。
        let sp_cfg = SmartPruneConfig {
            trigger_tokens: self.cfg.trigger_tokens,
            keep_recent: self.cfg.keep_recent_smart_prune,
            target_tokens: self.cfg.target_tokens(),
            calibration_scale: scale,
            ..Default::default()
        };
        let (after_sp, sp_report) = smart_prune(after_mc, &sp_cfg);
        let after_sp_tokens = estimate_messages(&after_sp);
        if scaled_tokens(after_sp_tokens, scale) < self.cfg.target_tokens() {
            // 只在两个阶段均未修改消息时返回 Noop，避免丢弃前一阶段的成果。
            if !mc_report.was_compacted && !sp_report.was_compacted {
                log_compaction_decision(
                    "noop",
                    after_sp_tokens,
                    scale,
                    reported,
                    self.cfg.trigger_tokens,
                    "nothing_prunable",
                );
                return CompactionOutcome::Noop {
                    event: ContextCompactedEvent {
                        strategy: ContextCompactedStrategy::Noop,
                        removed_messages: 0,
                        before_tokens,
                        after_tokens: before_tokens,
                    },
                    reason: "nothing_prunable",
                };
            }
            log_compaction_decision(
                if sp_report.was_compacted {
                    "smart_prune"
                } else {
                    "microcompact"
                },
                after_sp_tokens,
                scale,
                reported,
                self.cfg.target_tokens(),
                "below_target",
            );
            return CompactionOutcome::Applied {
                messages: after_sp,
                event: ContextCompactedEvent {
                    strategy: if sp_report.was_compacted {
                        ContextCompactedStrategy::SmartPrune
                    } else {
                        ContextCompactedStrategy::Microcompact
                    },
                    removed_messages: mc_report.removed_count + sp_report.removed_count,
                    before_tokens,
                    after_tokens: after_sp_tokens,
                },
                memory_write: None,
                micro_changed: mc_report.removed_count,
                pruned: sp_report.removed_count,
                truncated: sp_report.truncated_count,
                below_trigger: below_trigger(after_sp_tokens, scale, self.cfg.trigger_tokens),
            };
        }

        // 第 4 步:LLM 摘要(若启用)。M5 v0:存在历史摘要时使用增量模式
        // (`summarize_recent`),否则使用全量模式。v1.x:`/compact <指示>`
        // 的附加指示经 `*_with_focus` 注入摘要 prompt。
        if self.cfg.summarize_after {
            let result = match prev_summary {
                Some(prev) => {
                    self.summarizer
                        .summarize_recent_with_focus(&after_sp, Some(prev), focus)
                        .await
                }
                None => {
                    self.summarizer
                        .summarize_full_with_focus(&after_sp, focus)
                        .await
                }
            };
            match result {
                Ok(summary) => {
                    // v1.4 D2:记忆固化 —— LLM 摘要成功后追加提取长期事实
                    // 并 append 进目标 scope。失败仅 warn(固化是增益,
                    // 绝不阻断压缩主路径)。
                    let mut memory_write: Option<MemoryWriteInfo> = None;
                    if let Some(cons) = &self.consolidation {
                        match self.summarizer.extract_memories(&after_sp).await {
                            Ok(facts) if !facts.is_empty() => {
                                memory_write = Some(append_memories(cons, &facts));
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!(?e, "memory consolidation skipped");
                            }
                        }
                    }
                    // summary 作为 System 消息存档历史;但**必须保留最近的
                    // 用户消息(及紧随其后的 assistant/tool 对)**,否则下一次
                    // `model_call` 的 `request.messages` 只剩一条 System 消息
                    // —— 它被 provider 放进 `system` 字段而非 `messages`,
                    // 导致 `messages` 为空,上游 API 返回
                    // `messages must not be empty`(GAIA 截断类失败的直接原因)。
                    // 修复:summary 之后追加 smart_prune 的尾部 `keep_recent`
                    // 条消息(含最新用户问题 + 最近工具结果),让对话能续作。
                    let mut summary_msg = vec![ChatMessage::System(format!(
                        "<summary>\n{summary}\n</summary>"
                    ))];
                    let tail = keep_recent_tail(&after_sp, self.cfg.keep_recent_smart_prune);
                    summary_msg.extend(tail);
                    let after_tokens = estimate_messages(&summary_msg);
                    log_compaction_decision(
                        "llm_summarize",
                        after_tokens,
                        scale,
                        reported,
                        self.cfg.trigger_tokens,
                        "summarized",
                    );
                    return CompactionOutcome::Applied {
                        messages: summary_msg,
                        event: ContextCompactedEvent {
                            strategy: ContextCompactedStrategy::LlMSummarize,
                            removed_messages: sp_report.removed_count + 1,
                            before_tokens,
                            after_tokens,
                        },
                        memory_write,
                        micro_changed: mc_report.removed_count,
                        pruned: sp_report.removed_count,
                        truncated: sp_report.truncated_count,
                        below_trigger: below_trigger(after_tokens, scale, self.cfg.trigger_tokens),
                    };
                }
                Err(e) => {
                    tracing::warn!(?e, "summarize failed; returning best-effort smart_prune");
                }
            }
        }

        // 回退:摘要不可用(无 provider / 摘要失败 / summarize_after 关)
        // 时返回 smart_prune 的尽力而为结果。但**必须核实确实裁剪过
        // 东西**:两个阶段都没有修改才意味着消息列表
        // 原样 —— 此时谎报 `SmartPrune` 就是「幽灵压缩」:调用方
        // (pre_loop)会登记 rapid-refill 熔断锚点并广播压缩事件,而
        // 上下文一个字节没少,下一迭代立即再次越阈 → Skip/Abort 连击,
        // 把正常推进的 turn 误杀(桌面 sess_77cd626f 实证:12K token
        // 会话被幽灵压缩引爆熔断)。空压一律降级为 Noop(不回写列表、
        // 不上膛守卫)。
        if !mc_report.was_compacted && !sp_report.was_compacted {
            log_compaction_decision(
                "noop",
                after_sp_tokens,
                scale,
                reported,
                self.cfg.trigger_tokens,
                "nothing_prunable",
            );
            return CompactionOutcome::Noop {
                event: ContextCompactedEvent {
                    strategy: ContextCompactedStrategy::Noop,
                    removed_messages: 0,
                    before_tokens,
                    after_tokens: before_tokens,
                },
                reason: "nothing_prunable",
            };
        }
        log_compaction_decision(
            if sp_report.was_compacted {
                "smart_prune"
            } else {
                "microcompact"
            },
            after_sp_tokens,
            scale,
            reported,
            self.cfg.trigger_tokens,
            "still_above_trigger",
        );
        CompactionOutcome::Applied {
            messages: after_sp,
            event: ContextCompactedEvent {
                strategy: if sp_report.was_compacted {
                    ContextCompactedStrategy::SmartPrune
                } else {
                    ContextCompactedStrategy::Microcompact
                },
                removed_messages: mc_report.removed_count + sp_report.removed_count,
                before_tokens,
                after_tokens: after_sp_tokens,
            },
            memory_write: None,
            micro_changed: mc_report.removed_count,
            pruned: sp_report.removed_count,
            truncated: sp_report.truncated_count,
            below_trigger: below_trigger(after_sp_tokens, scale, self.cfg.trigger_tokens),
        }
    }
}

/// v1.x:compact 期间记忆固化的写入摘要,由 `append_memories` 产生、
/// `compact_with_prior_and_tokens` 透出,供上层(pre_loop)落盘审计。
#[derive(Debug, Clone)]
pub struct MemoryWriteInfo {
    /// 实际写入的 scope。
    pub scope: zhuye_memory::MemoryScope,
    /// agent 类型名。
    pub agent: String,
    /// 固化的条目数。
    pub facts: usize,
    /// 追加进 MEMORY.md 的字符数(不含原有内容)。
    pub chars: usize,
}

/// v1.4 D2:把固化的记忆条目 append 进目标 scope —— 读取现有内容,
/// 追加带时间戳的 `##` 条目后整体保存(FileMemoryStore 的 save 是
/// 覆盖语义,append 在此层组装)。写入失败仅 warn。
///
/// v1.x:返回写入摘要(scope / agent / 条目数 / 追加字符数),供上层落盘
/// `RolloutRecord::MemoryAccess` 审计记录。
fn append_memories(target: &ConsolidationTarget, facts: &[String]) -> MemoryWriteInfo {
    let existing = target
        .store
        .load(target.scope, &target.agent)
        .unwrap_or_default();
    let existing_len = existing.len();
    let mut body = existing;
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    let ts = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC");
    body.push_str(&format!("\n## Consolidated {ts}\n"));
    for f in facts {
        body.push_str(&format!("- {f}\n"));
    }
    let appended_chars = body.len().saturating_sub(existing_len);
    if let Err(e) = target.store.save(target.scope, &target.agent, &body) {
        tracing::warn!(?e, "memory consolidation write failed");
    } else {
        tracing::info!(
            count = facts.len(),
            scope = ?target.scope,
            "memory consolidation written"
        );
    }
    MemoryWriteInfo {
        scope: target.scope,
        agent: target.agent.clone(),
        facts: facts.len(),
        chars: appended_chars,
    }
}

/// 返回 `messages` 的末尾 `n` 条,但保证至少包含最后一条 User 消息
/// (即原始用户问题)。LLM-summarize 后必须保留这一条,否则 provider 端
/// `messages` 为空会被 API 拒绝(`messages must not be empty`)。
///
/// 截断点对齐:若末尾 `n` 条的第一条是 Tool 消息(dangling tool result,
/// 缺少对应 assistant tool_use),向前回溯一条以保持成对完整。
fn keep_recent_tail(messages: &[ChatMessage], n: usize) -> Vec<ChatMessage> {
    if messages.is_empty() || n == 0 {
        return Vec::new();
    }
    // 找到最后一条 User 消息的位置 —— 必须保留它(原始问题)。
    let last_user = messages
        .iter()
        .rposition(|m| matches!(m, ChatMessage::User(_)))
        .unwrap_or(0);
    // 起点 = min(末尾 n 条的起点, 最后一条 User 的位置)。
    let mut start = messages.len().saturating_sub(n).min(last_user);
    // 若起点落在 dangling Tool 消息上(无对应 assistant tool_use),
    // 向前回溯一条让它与 assistant 成对。
    while start > 0 && matches!(messages[start], ChatMessage::Tool(_)) {
        start -= 1;
    }
    let tail = messages[start..].to_vec();
    // 摘要类内联 System 不参与 tail:新摘要已覆盖旧摘要,带进来会让同一份
    // 摘要在 system 前缀里重复(白烧 token)。若过滤后为空则回退原 tail ——
    // 保住「messages 非空」的既有守卫。
    let filtered: Vec<ChatMessage> = tail
        .iter()
        .filter(|m| !matches!(m, ChatMessage::System(_)))
        .cloned()
        .collect();
    if filtered.is_empty() { tail } else { filtered }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use zhuye_llm::{AssistantContent, ContentBlock, ToolCallRequest, ToolResult, UserContent};

    /// 返回预制输出的 mock summarizer。
    struct MockSummarizer {
        canned: String,
        fail: bool,
    }

    #[async_trait]
    impl Summarizer for MockSummarizer {
        async fn summarize_full(
            &self,
            _: &[ChatMessage],
        ) -> Result<String, crate::summarizer::SummarizerError> {
            if self.fail {
                Err(crate::summarizer::SummarizerError::Cancelled)
            } else {
                Ok(self.canned.clone())
            }
        }
        async fn summarize_recent(
            &self,
            _: &[ChatMessage],
            _: Option<&str>,
        ) -> Result<String, crate::summarizer::SummarizerError> {
            self.summarize_full(&[]).await
        }
    }

    fn huge_message_list() -> Vec<ChatMessage> {
        let mut msgs = vec![ChatMessage::System("sys".into())];
        for i in 0..200 {
            msgs.push(ChatMessage::Tool(ToolResult {
                call_id: format!("c{i}"),
                content: vec![ContentBlock::Text {
                    text: "x".repeat(1000),
                }],
                is_error: false,
            }));
        }
        msgs
    }

    #[test]
    fn trigger_policy_tracks_window_and_fixed_override() {
        let c = Compactor::new(
            CompactorConfig::default(),
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: false,
            }),
        );
        c.set_trigger_policy(TriggerPolicy::WindowRatio(0.5));
        assert_eq!(c.for_window(Some(1_000_000)).trigger_tokens(), 500_000);
        assert_eq!(c.for_window(Some(8_000)).trigger_tokens(), 4_000);
        assert_eq!(c.for_window(None).trigger_tokens(), 64_000);
        c.set_trigger_policy(TriggerPolicy::Fixed(7_000));
        assert_eq!(c.for_window(Some(8_000)).trigger_tokens(), 7_000);
        assert_eq!(c.for_window(Some(1_000_000)).trigger_tokens(), 7_000);
    }

    #[tokio::test]
    async fn noop_when_under_micro_threshold() {
        let c = Compactor::new(
            CompactorConfig::default(),
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: false,
            }),
        );
        let msgs = vec![ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::text("hi")],
        })];
        let (_, evt) = c.compact(&msgs).await;
        assert!(matches!(evt.strategy, ContextCompactedStrategy::Noop));
    }

    /// 幽灵压缩回归(桌面 sess_77cd626f 死循环熔断):before 落在
    /// [micro 阈值, trigger) 区间且消息数 ≤ keep_recent 时,microcompact
    /// 提前退出、smart_prune 内部 Noop,摘要器又失败 —— 三级全部空转。
    /// 兜底必须降级为 Noop:谎报 SmartPrune 会让 pre_loop 给
    /// rapid-refill 熔断器登记锚点,而上下文没变小,后续每步越阈都被判
    /// refill,3 连击直接误杀正常 turn。
    #[tokio::test]
    async fn phantom_compaction_degrades_to_noop() {
        let c = Compactor::new(
            CompactorConfig::default(), // trigger 10_000 / micro 7_000 / target 7_500
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: true,
            }),
        );
        // 30_000 字符 ≈ 8_572 token,落在 [7_000, 10_000) 区间;2 条消息
        // ≤ keep_recent(30) → micro 提前退出;8_572 < 10_000 →
        // smart_prune 内部 Noop;8_572 ≥ 7_500 → 落到摘要器(失败)→ 兜底。
        let msgs = vec![
            ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::text("hi")],
            }),
            ChatMessage::Tool(ToolResult {
                call_id: "c0".into(),
                content: vec![ContentBlock::Text {
                    text: "x".repeat(30_000),
                }],
                is_error: false,
            }),
        ];
        let (out, evt) = c.compact(&msgs).await;
        assert!(
            matches!(evt.strategy, ContextCompactedStrategy::Noop),
            "三级空转时必须降级 Noop,实际 {:?}",
            evt.strategy
        );
        assert!(out.is_none(), "空压不得回写消息列表");
    }

    /// 对照组:smart_prune 确实裁剪过(removed>0 或 truncated>0)时,
    /// 兜底仍返回 SmartPrune 尽力而为结果 —— 降级只针对「什么都没做」
    /// 的空压,不吞掉真实的部分压缩。
    #[tokio::test]
    async fn best_effort_smart_prune_kept_when_pruned() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 100,
                keep_recent_microcompact: 1_000,
                keep_recent_smart_prune: 1_000,
                summarize_after: true,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: true,
            }),
        );
        // 200 条 1_000 字符工具结果:n=200 ≤ keep_recent → micro 提前
        // 退出;before 远超 trigger(100)→ smart_prune 真跑,阶段一截断
        // 旧工具输出(truncated>0)。摘要失败 → 兜底:有截断在,不得降级。
        let msgs = huge_message_list();
        let (out, evt) = c.compact(&msgs).await;
        assert!(
            matches!(evt.strategy, ContextCompactedStrategy::SmartPrune),
            "有真实裁剪时兜底保持 SmartPrune,实际 {:?}",
            evt.strategy
        );
        assert!(out.is_some());
        assert!(
            evt.removed_messages > 0 || evt.after_tokens < evt.before_tokens,
            "必须发生真实变化(移除或截断)"
        );
    }

    #[tokio::test]
    async fn microcompact_when_over_threshold() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 100,
                keep_recent_microcompact: 5,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: false,
            }),
        );
        let msgs = huge_message_list();
        let (out, evt) = c.compact(&msgs).await;
        // tokens ≈ 200k,threshold = 100 → 明显超限。
        // 200 条工具结果、keep_recent=5 时,microcompact 会替换大多数
        // 旧工具结果。若能压到 trigger_tokens=100 以下,返回 Microcompact;
        // 否则升级策略。
        let strat = evt.strategy;
        assert!(matches!(
            strat,
            ContextCompactedStrategy::Microcompact
                | ContextCompactedStrategy::SmartPrune
                | ContextCompactedStrategy::LlMSummarize
        ));
        assert!(out.as_ref().is_some_and(|o| o.len() <= 205));
    }

    #[tokio::test]
    async fn llm_summarize_when_smart_prune_insufficient() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 100,
                keep_recent_microcompact: 5,
                keep_recent_smart_prune: 5,
                target_ratio: 0.01, // very tight target so smart_prune fails
                summarize_after: true,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: "fake summary".into(),
                fail: false,
            }),
        );
        let msgs = huge_message_list();
        let (_, evt) = c.compact(&msgs).await;
        // 目标紧 → 升级到 summarize。
        assert!(matches!(
            evt.strategy,
            ContextCompactedStrategy::LlMSummarize
        ));
    }

    /// v1.4 D2:固化 —— MockSummarizer 提供 extract_memories,压缩触发
    /// LLM 摘要后,事实条目被 append 进目标 store。
    #[tokio::test]
    async fn consolidation_writes_extracted_facts() {
        use zhuye_memory::{InMemoryStore, MemoryScope};

        // MockSummarizer 增配记忆提取(直接包装默认 Mock + 自定义行为:
        // 这里内联一个带提取的 stub)。
        struct WithMemories(Arc<MockSummarizer>);
        use crate::summarizer::SummarizerError;
        #[async_trait::async_trait]
        impl crate::summarizer::Summarizer for WithMemories {
            async fn summarize_full(&self, m: &[ChatMessage]) -> Result<String, SummarizerError> {
                self.0.summarize_full(m).await
            }
            async fn summarize_recent(
                &self,
                m: &[ChatMessage],
                p: Option<&str>,
            ) -> Result<String, SummarizerError> {
                self.0.summarize_recent(m, p).await
            }
            async fn extract_memories(
                &self,
                _m: &[ChatMessage],
            ) -> Result<Vec<String>, SummarizerError> {
                Ok(vec![
                    "User prefers Rust edition 2024".into(),
                    "Deploy target is aarch64 linux".into(),
                ])
            }
        }

        let store: std::sync::Arc<dyn zhuye_memory::MemoryStore> =
            Arc::new(InMemoryStore::default());
        let store_for_read = store.clone();
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 100,
                keep_recent_microcompact: 5,
                keep_recent_smart_prune: 5,
                target_ratio: 0.01,
                summarize_after: true,
                ..Default::default()
            },
            Arc::new(WithMemories(Arc::new(MockSummarizer {
                canned: "fake summary".into(),
                fail: false,
            }))),
        )
        .with_memory_consolidation(store, MemoryScope::Session, "tester");
        let msgs = huge_message_list();
        let (_, evt) = c.compact(&msgs).await;
        assert!(matches!(
            evt.strategy,
            ContextCompactedStrategy::LlMSummarize
        ));
        // 固化条目应写入 Session scope(store 走 InMemory)。
        let saved = store_for_read.load(MemoryScope::Session, "tester").unwrap();
        assert!(saved.contains("Consolidated"));
        assert!(saved.contains("Rust edition 2024"));
        assert!(saved.contains("aarch64 linux"));
    }

    /// v1.x:compact_with_prior_and_tokens 的第三返回值透出记忆固化写入
    /// 摘要(scope / agent / 条目数 / 追加字符数),供上层落盘审计。
    #[tokio::test]
    async fn consolidation_returns_memory_write_info() {
        use crate::summarizer::SummarizerError;
        use zhuye_memory::{InMemoryStore, MemoryScope};

        struct WithMemories(Arc<MockSummarizer>);
        #[async_trait::async_trait]
        impl crate::summarizer::Summarizer for WithMemories {
            async fn summarize_full(&self, m: &[ChatMessage]) -> Result<String, SummarizerError> {
                self.0.summarize_full(m).await
            }
            async fn summarize_recent(
                &self,
                m: &[ChatMessage],
                p: Option<&str>,
            ) -> Result<String, SummarizerError> {
                self.0.summarize_recent(m, p).await
            }
            async fn extract_memories(
                &self,
                _m: &[ChatMessage],
            ) -> Result<Vec<String>, SummarizerError> {
                Ok(vec!["fact one".into(), "fact two".into()])
            }
        }

        let store: std::sync::Arc<dyn zhuye_memory::MemoryStore> =
            Arc::new(InMemoryStore::default());
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 100,
                keep_recent_microcompact: 5,
                keep_recent_smart_prune: 5,
                target_ratio: 0.01,
                summarize_after: true,
                ..Default::default()
            },
            Arc::new(WithMemories(Arc::new(MockSummarizer {
                canned: "fake summary".into(),
                fail: false,
            }))),
        )
        .with_memory_consolidation(store, MemoryScope::Session, "tester");
        let msgs = huge_message_list();
        let (_, evt, mem) = c.compact_with_prior_and_tokens(&msgs, None, None).await;
        assert!(matches!(
            evt.strategy,
            ContextCompactedStrategy::LlMSummarize
        ));
        let info = mem.expect("固化发生时应透出写入摘要");
        assert_eq!(info.scope, MemoryScope::Session);
        assert_eq!(info.agent, "tester");
        assert_eq!(info.facts, 2);
        assert!(info.chars > 0, "追加字符数应大于 0");
    }

    /// v1.x:未配置 consolidation 时第三返回值为 None(不阻断主路径)。
    #[tokio::test]
    async fn no_consolidation_yields_none_memory_write() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 100,
                keep_recent_microcompact: 5,
                keep_recent_smart_prune: 5,
                target_ratio: 0.01,
                summarize_after: true,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: "fake summary".into(),
                fail: false,
            }),
        );
        let msgs = huge_message_list();
        let (_, _evt, mem) = c.compact_with_prior_and_tokens(&msgs, None, None).await;
        assert!(mem.is_none());
    }

    #[tokio::test]
    async fn fallback_to_smart_prune_when_summarize_fails() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 100,
                keep_recent_microcompact: 5,
                keep_recent_smart_prune: 5,
                target_ratio: 0.01,
                summarize_after: true,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: true,
            }),
        );
        let msgs = huge_message_list();
        let (_, evt) = c.compact(&msgs).await;
        // Summarizer 失败 → 返回 best-effort 的 smart_prune。
        assert!(matches!(evt.strategy, ContextCompactedStrategy::SmartPrune));
    }

    #[test]
    fn target_tokens_is_trigger_times_ratio() {
        let cfg = CompactorConfig {
            trigger_tokens: 200_000,
            target_ratio: 0.75,
            ..Default::default()
        };
        assert_eq!(cfg.target_tokens(), 150_000);
    }

    // ── GAIA-fix: LLM-summarize 保留尾部消息(含用户问题) ───────────────

    fn sample_conversation() -> Vec<ChatMessage> {
        // System + User(问题) + Assistant(tool_use) + Tool(result) + ...
        vec![
            ChatMessage::System("sys".into()),
            ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::text("What is 2+2?")],
            }),
            ChatMessage::Assistant(AssistantContent {
                text: Some("let me compute".into()),
                tool_calls: vec![zhuye_llm::ToolCallRequest {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({}),
                }],
                thinking: None,
            }),
            ChatMessage::Tool(ToolResult {
                call_id: "c1".into(),
                content: vec![ContentBlock::text("4")],
                is_error: false,
            }),
            ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::text("now what is 3+3?")],
            }),
            ChatMessage::Assistant(AssistantContent {
                text: Some("let me compute again".into()),
                tool_calls: vec![zhuye_llm::ToolCallRequest {
                    id: "c2".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({}),
                }],
                thinking: None,
            }),
            ChatMessage::Tool(ToolResult {
                call_id: "c2".into(),
                content: vec![ContentBlock::text("6")],
                is_error: false,
            }),
        ]
    }

    #[test]
    fn keep_recent_tail_preserves_last_user_message() {
        // n=2 但最后一条 User 在更早位置 → 必须至少保留到那条 User。
        let msgs = sample_conversation();
        let tail = keep_recent_tail(&msgs, 2);
        // tail 必须包含最后一条 User("now what is 3+3?")。
        assert!(
            tail.iter().any(|m| matches!(m, ChatMessage::User(u)
                if u.blocks.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("3+3"))))
            ),
            "tail must include the last user message, got {:?}",
            tail.iter().map(|m| match m {
                ChatMessage::User(_) => "User",
                ChatMessage::Assistant(_) => "Assistant",
                ChatMessage::Tool(_) => "Tool",
                ChatMessage::System(_) => "System",
            }).collect::<Vec<_>>()
        );
    }

    #[test]
    fn keep_recent_tail_backtracks_dangling_tool_result() {
        // n=3 让起点落在 Tool(c1) 上 → 必须向前回溯到 Assistant(c1)。
        let msgs = sample_conversation();
        let tail = keep_recent_tail(&msgs, 3);
        // tail 的第一条不应是 Tool(dangling)。
        assert!(
            !tail
                .first()
                .is_some_and(|m| matches!(m, ChatMessage::Tool(_))),
            "tail must not start with a dangling Tool result"
        );
    }

    #[test]
    fn keep_recent_tail_empty_input() {
        assert!(keep_recent_tail(&[], 5).is_empty());
        assert!(keep_recent_tail(&sample_conversation(), 0).is_empty());
    }

    /// 旧摘要(System)不能被 tail 带回压缩结果:新摘要已覆盖它,重复出现
    /// 只会把同一份摘要再塞进 system 前缀。
    #[test]
    fn keep_recent_tail_drops_inline_system_summary() {
        let mut msgs = vec![ChatMessage::System("<summary>旧摘要</summary>".into())];
        msgs.extend(sample_conversation());
        // n 大于消息总数 → 起点为 0,旧摘要本来会被整段带走。
        let tail = keep_recent_tail(&msgs, msgs.len() + 10);
        assert!(
            !tail.iter().any(|m| matches!(m, ChatMessage::System(_))),
            "tail 不应含 System: {:?}",
            tail.len()
        );
        // 最后一条 User 仍在,保证 messages 非空。
        assert!(tail.iter().any(|m| matches!(m, ChatMessage::User(_))));
    }

    // ── M8 P0b: LLM-reported input tokens drive trigger ─────────────────

    /// `huge_message_list` 本地估算约 200k token。trigger=10k 时,
    /// compactor 触发(无论 LLM 信号如何)。这是 M7 基线 —— 不得回归。
    #[tokio::test]
    async fn huge_list_fires_compact_without_llm_signal() {
        let c = Compactor::new(
            CompactorConfig::default(),
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: false,
            }),
        );
        let msgs = huge_message_list();
        let (_out, evt, _mem) = c.compact_with_prior_and_tokens(&msgs, None, None).await;
        assert!(!matches!(evt.strategy, ContextCompactedStrategy::Noop));
    }

    /// 本地估算 = 1000(仅 1 条 "hi"),LLM 上报 = 50_000,trigger =
    /// 10_000 → 管线**进入**(上报值是权威触发信号,micro / smart_prune
    /// 照跑),但消息列表没有可裁的东西 —— 超限的大头在 system prompt /
    /// 工具目录里,消息级压缩器鞭长莫及。此时必须诚实降级 `Noop`:
    /// 旧版在此谎报 SmartPrune(幽灵压缩);若改走「摘要可用就硬摘要」,
    /// 一条 "hi" 摘不出内容、上下文不变量,摘要存在又会给 rapid-refill
    /// 守卫上膛 —— 死循环换皮复发。Noop 不上膛、不烧 LLM,上下文真被
    /// 消息撑大时自然由后续迭代真实压缩。
    #[tokio::test]
    async fn llm_reported_triggers_pipeline_but_noop_when_nothing_prunable() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 10_000,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: false,
            }),
        );
        // 本地估算仅 1 token。
        let msgs = vec![ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::text("hi")],
        })];
        let (out, evt, _mem) = c
            .compact_with_prior_and_tokens(&msgs, None, Some(50_000))
            .await;
        assert!(
            matches!(evt.strategy, ContextCompactedStrategy::Noop),
            "无可裁内容时必须降级 Noop,实际 {:?}",
            evt.strategy
        );
        assert!(out.is_none(), "空压不得回写消息列表");
    }

    #[tokio::test]
    async fn microcompact_kept_when_smart_prune_has_no_changes() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 1_000,
                keep_recent_microcompact: 1,
                keep_recent_smart_prune: 100,
                summarize_after: false,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: false,
            }),
        );
        let msgs = vec![
            ChatMessage::System("s".repeat(1_000)),
            ChatMessage::Tool(ToolResult {
                call_id: "c1".into(),
                content: vec![ContentBlock::text("x".repeat(8_000))],
                is_error: false,
            }),
            ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::text("继续")],
            }),
        ];
        let (out, event, _) = c.compact_with_prior_and_tokens(&msgs, None, None).await;
        assert_eq!(event.strategy, ContextCompactedStrategy::Microcompact);
        assert_eq!(event.removed_messages, 1);
        assert!(matches!(&out.unwrap()[1], ChatMessage::Tool(result)
            if matches!(&result.content[0], ContentBlock::Text { text } if text == crate::microcompact::TRUNCATE_PLACEHOLDER)));
    }

    #[tokio::test]
    async fn repeated_microcompact_does_not_report_an_unchanged_placeholder() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 1_000,
                keep_recent_microcompact: 1,
                keep_recent_smart_prune: 100,
                summarize_after: false,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: false,
            }),
        );
        let msgs = vec![
            ChatMessage::System("s".repeat(8_000)),
            ChatMessage::Tool(ToolResult {
                call_id: "c1".into(),
                content: vec![ContentBlock::text(
                    crate::microcompact::TRUNCATE_PLACEHOLDER,
                )],
                is_error: false,
            }),
            ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::text("继续")],
            }),
        ];
        let (out, event, _) = c.compact_with_prior_and_tokens(&msgs, None, None).await;
        assert_eq!(event.strategy, ContextCompactedStrategy::Noop);
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn smart_prune_can_change_content_without_removing_messages() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 1_000,
                keep_recent_microcompact: 100,
                keep_recent_smart_prune: 100,
                summarize_after: false,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: false,
            }),
        );
        let msgs = vec![
            ChatMessage::System("s".repeat(8_000)),
            ChatMessage::Tool(ToolResult {
                call_id: "c1".into(),
                content: vec![ContentBlock::text("x".repeat(2_000))],
                is_error: false,
            }),
        ];
        let (out, event, _) = c.compact_with_prior_and_tokens(&msgs, None, None).await;
        assert_eq!(
            event.strategy,
            ContextCompactedStrategy::SmartPrune,
            "removed={}",
            event.removed_messages
        );
        assert_eq!(event.removed_messages, 0);
        let output = out.expect("截断必须回写");
        assert!(matches!(&output[1], ChatMessage::Tool(result)
            if matches!(&result.content[0], ContentBlock::Text { text } if text.len() < 2_000)));
    }

    #[tokio::test]
    async fn calibrated_report_continues_past_microcompact_into_smart_prune() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 5_800,
                keep_recent_microcompact: 1,
                keep_recent_smart_prune: 1,
                summarize_after: false,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: false,
            }),
        );
        let mut msgs = vec![ChatMessage::System("s".repeat(1_000))];
        for i in 0..12 {
            msgs.push(ChatMessage::Tool(ToolResult {
                call_id: format!("tool:bash:{i}"),
                content: vec![ContentBlock::text("x".repeat(1_400))],
                is_error: false,
            }));
        }
        msgs.push(ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::text("最新问题")],
        }));
        let local = estimate_messages(&msgs);
        let reported = ((local as f64) * 1.4).ceil() as u32;
        let outcome = c.compact_outcome(&msgs, None, Some(reported), None).await;
        let CompactionOutcome::Applied {
            messages: output,
            event,
            ..
        } = outcome
        else {
            panic!("校准后仍超阈时必须继续压缩");
        };
        let local_after = estimate_messages(&output);
        let scale = calibration_scale(local, Some(reported));
        assert!(
            scaled_tokens(local_after, scale) < 5_800,
            "microcompact 未达标时同一次压缩必须继续升级: strategy={:?} local_after={local_after} scale={scale}",
            event.strategy
        );
        assert!(
            matches!(
                event.strategy,
                ContextCompactedStrategy::Microcompact | ContextCompactedStrategy::SmartPrune
            ),
            "校准水位达标后才能停止: {event:?}"
        );
    }

    #[tokio::test]
    async fn calibrated_underestimate_continues_into_smart_prune() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 2_500,
                keep_recent_microcompact: 1,
                keep_recent_smart_prune: 1,
                summarize_after: false,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: false,
            }),
        );
        // 占位符本身仍有体积。条数足够多时，microcompact 全部替换后，
        // 2 倍校准水位仍高于阈值，同一次压缩必须继续 smart_prune。
        let mut msgs = vec![ChatMessage::System("s".repeat(500))];
        for i in 0..40 {
            msgs.push(ChatMessage::Assistant(AssistantContent {
                text: Some("调用工具做事情".repeat(5)),
                tool_calls: vec![ToolCallRequest {
                    id: format!("c{i}"),
                    name: "bash".into(),
                    arguments: serde_json::json!({"cmd": format!("{}{i}", "c".repeat(80))}),
                }],
                thinking: None,
            }));
            msgs.push(ChatMessage::Tool(ToolResult {
                call_id: format!("c{i}"),
                content: vec![ContentBlock::text("y".repeat(2_000))],
                is_error: false,
            }));
        }
        msgs.push(ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::text("最新问题")],
        }));
        let local = estimate_messages(&msgs);
        let reported = local.saturating_mul(2);
        let after_micro_floor = scaled_tokens(
            estimate_messages(&{
                let mut placeholders = msgs.clone();
                for message in &mut placeholders {
                    if let ChatMessage::Tool(result) = message {
                        result.content = vec![ContentBlock::text(
                            crate::microcompact::TRUNCATE_PLACEHOLDER,
                        )];
                    }
                }
                placeholders
            }),
            calibration_scale(local, Some(reported)),
        );
        assert!(
            after_micro_floor >= 2_500,
            "夹具失效：microcompact 单独就能压到阈值下 ({after_micro_floor})"
        );
        let outcome = c.compact_outcome(&msgs, None, Some(reported), None).await;
        let CompactionOutcome::Applied {
            messages: output,
            event,
            below_trigger,
            ..
        } = outcome
        else {
            panic!("校准水位仍超阈时不得在 microcompact 提前返回");
        };
        assert_eq!(
            event.strategy,
            ContextCompactedStrategy::SmartPrune,
            "microcompact 后校准水位仍超阈时必须继续 smart_prune"
        );
        assert!(below_trigger, "smart_prune 应把校准水位压到阈值以下");
        assert!(
            output
                .iter()
                .filter(|message| matches!(message, ChatMessage::Tool(_)))
                .count()
                < 40,
            "smart_prune 应删除旧工具结果"
        );
        assert!(
            scaled_tokens(
                estimate_messages(&output),
                calibration_scale(local, Some(reported))
            ) < 2_500
        );
    }

    #[tokio::test]
    async fn extreme_report_is_clamped_and_still_above_does_not_claim_success() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 500,
                keep_recent_microcompact: 20,
                keep_recent_smart_prune: 20,
                summarize_after: false,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: false,
            }),
        );
        let msgs = vec![
            ChatMessage::System("s".repeat(3_500)),
            ChatMessage::Tool(ToolResult {
                call_id: "c1".into(),
                content: vec![ContentBlock::text("x".repeat(4_000))],
                is_error: false,
            }),
            ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::text("继续")],
            }),
        ];
        let local = estimate_messages(&msgs);
        assert_eq!(
            calibration_scale(local, Some(local.saturating_mul(10))),
            3.0
        );
        let outcome = c
            .compact_outcome(&msgs, None, Some(local.saturating_mul(10)), None)
            .await;
        let CompactionOutcome::Applied {
            below_trigger,
            event,
            ..
        } = outcome
        else {
            panic!("有截断动作时不得降级成 Noop");
        };
        assert!(
            !below_trigger,
            "钳制后仍超阈的裁剪不得登记为成功压缩: {event:?}"
        );
        assert_eq!(event.strategy, ContextCompactedStrategy::SmartPrune);
    }

    #[tokio::test]
    async fn missing_report_does_not_scale_local_estimate() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 2_000,
                keep_recent_microcompact: 1,
                keep_recent_smart_prune: 100,
                summarize_after: false,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: false,
            }),
        );
        let msgs = vec![
            ChatMessage::System("s".repeat(1_000)),
            ChatMessage::Tool(ToolResult {
                call_id: "tool:bash:1".into(),
                content: vec![ContentBlock::text("x".repeat(8_000))],
                is_error: false,
            }),
            ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::text("继续")],
            }),
        ];
        let local = estimate_messages(&msgs);
        assert!(local > 2_000, "夹具应先越过阈值: {local}");
        assert!(
            calibration_scale(local, None) == 1.0,
            "没有上报时缩放必须保持 1"
        );
        let outcome = c.compact_outcome(&msgs, None, None, None).await;
        match outcome {
            CompactionOutcome::Applied {
                below_trigger,
                event,
                ..
            } => {
                assert!(below_trigger, "无上报时按本地估算判定达标: {event:?}");
                assert_eq!(event.strategy, ContextCompactedStrategy::Microcompact);
                assert!(event.after_tokens < 2_000);
            }
            CompactionOutcome::Noop { reason, .. } => {
                panic!("无上报时本地超阈必须压缩: {reason}")
            }
        }
    }

    /// 本地估算 = 50_000(大列表),LLM 上报 = 8_000,
    /// trigger = 10_000 → 必须触发,因为 `max(estimate, llm_reported)
    /// = 50_000 > 10_000`。LLM 上报值**不**允许压制真实的本地超限。
    #[tokio::test]
    async fn max_of_estimate_and_llm_triggers_when_estimate_is_big() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 10_000,
                keep_recent_microcompact: 5,
                keep_recent_smart_prune: 5,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: false,
            }),
        );
        let msgs = huge_message_list();
        let (_out, evt, _mem) = c
            .compact_with_prior_and_tokens(&msgs, None, Some(8_000))
            .await;
        assert!(
            !matches!(evt.strategy, ContextCompactedStrategy::Noop),
            "estimate 50k over trigger 10k must still fire even with llm=8k, got strategy {:?}",
            evt.strategy
        );
    }

    /// 本地估算 > micro-threshold(trigger * 0.7),LLM 上报 = 0
    /// (异常),trigger = 10_000 → 必须触发,因为本地估算
    /// 防 LLM 误报。
    #[tokio::test]
    async fn llm_reported_zero_falls_back_to_local_estimate() {
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 10_000,
                keep_recent_microcompact: 5,
                keep_recent_smart_prune: 5,
                ..Default::default()
            },
            Arc::new(MockSummarizer {
                canned: String::new(),
                fail: false,
            }),
        );
        // 构造一个本地估算落在 micro-threshold(trigger * 0.7 = 7_000)
        // 之上的中等消息列表。每条工具结果 4000 字符 ≈ 1143 token
        // × 20 = 22_860 token。
        let mut msgs: Vec<ChatMessage> = vec![ChatMessage::System("s".repeat(100))]; // ~29 tokens
        for i in 0..20 {
            msgs.push(ChatMessage::Tool(ToolResult {
                call_id: format!("c{i}"),
                content: vec![ContentBlock::Text {
                    text: "x".repeat(4000),
                }],
                is_error: false,
            }));
        }
        // 自检:本地估算应 > micro-threshold(7000)。
        let local = estimate_messages(&msgs);
        assert!(
            local > 7000,
            "test setup wrong: local estimate {local} should exceed micro-threshold 7000"
        );
        let (_out, evt, _mem) = c.compact_with_prior_and_tokens(&msgs, None, Some(0)).await;
        assert!(
            !matches!(evt.strategy, ContextCompactedStrategy::Noop),
            "llm=0 must not suppress local-estimate over-budget, got strategy {:?}",
            evt.strategy
        );
    }

    /// v1.x:捕获 `summarize_full_with_focus` 的 focus 实参的 mock。
    struct FocusCaptureSummarizer {
        seen: std::sync::Mutex<Vec<Option<String>>>,
    }

    #[async_trait]
    impl Summarizer for FocusCaptureSummarizer {
        async fn summarize_full(
            &self,
            _: &[ChatMessage],
        ) -> Result<String, crate::summarizer::SummarizerError> {
            Ok("# 摘要".into())
        }
        async fn summarize_recent(
            &self,
            _: &[ChatMessage],
            _: Option<&str>,
        ) -> Result<String, crate::summarizer::SummarizerError> {
            Ok("# 摘要".into())
        }
        async fn summarize_full_with_focus(
            &self,
            _: &[ChatMessage],
            focus: Option<&str>,
        ) -> Result<String, crate::summarizer::SummarizerError> {
            self.seen.lock().unwrap().push(focus.map(String::from));
            Ok("# 摘要".into())
        }
    }

    /// v1.x:`/compact <指示>` 的 focus 经 `compact_outcome` 传到摘要器;
    /// `None` 传 None(自动压缩路径不携带残留指示)。
    #[tokio::test]
    async fn compact_focus_flows_into_summarizer() {
        let mock = Arc::new(FocusCaptureSummarizer {
            seen: std::sync::Mutex::new(Vec::new()),
        });
        // target_ratio 压到 0.01:prune 后仍高于 target,才能走到 LLM 摘要步
        // (与 consolidation 系测试同一配方)。
        let c = Compactor::new(
            CompactorConfig {
                trigger_tokens: 100,
                keep_recent_microcompact: 5,
                keep_recent_smart_prune: 5,
                target_ratio: 0.01,
                summarize_after: true,
                ..Default::default()
            },
            Arc::clone(&mock) as Arc<dyn Summarizer>,
        );
        let msgs = huge_message_list();

        let outcome = c
            .compact_outcome(&msgs, None, None, Some("聚焦数据库改动"))
            .await;
        assert!(matches!(outcome, CompactionOutcome::Applied { .. }));
        assert_eq!(
            *mock.seen.lock().unwrap(),
            vec![Some("聚焦数据库改动".to_string())],
            "focus 必须原样传到摘要器"
        );

        let outcome = c.compact_outcome(&msgs, None, None, None).await;
        assert!(matches!(outcome, CompactionOutcome::Applied { .. }));
        assert_eq!(*mock.seen.lock().unwrap().last().unwrap(), None);
    }
}
