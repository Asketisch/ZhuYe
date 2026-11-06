//! JSONL → Markdown 导出 —— 把 `RolloutRecord` 序列化为人类可读的 markdown。
//!
//! v1.x S3:解锁 `/export` slash 命令。`App::export_pill` 走
//! `index::resolve_session_index` + `reader::replay` 拿到
//! `Vec<RolloutRecord>`,然后调 [`to_markdown`] 渲染成 markdown 文本,
//! 最终走 Pill 推到 history(未来 v2.x 可加 "save to file" 选项)。
//!
//! 设计要点:
//!
//! - **不**重新实现 markdown 渲染 —— RolloutRecord schema 固定 5 个变体
//!   (`SessionMeta` / `Message` / `Compaction` / `Fork` /
//!   `DiscussionTranscript`),逐个手工拼字符串。比拉进 `pulldown-cmark` /
//!   `markdown` crate 简单一个数量级。
//! - **Message.content** 是 `serde_json::Value` —— 大多数时候是 `"plain
//!   string"`,但也可能是 `{"text": "..."}` / `[{"type": "text", ...}]`
//!   等结构。helper [`format_content`] 取 `&Value` 优先字符串、再 `to_string`
//!   走 JSON(对齐 Markdown 经验上**永远**不会想渲染嵌套 JSON 树,所以
//!   退化到 pretty JSON 也无害)。
//! - **长度截断**:`MAX_MARKDOWN_CHARS = 4000` 与 `/memory show` 对齐
//!   (都走 Pill,不能让 Pill 超长导致 UI 抖动)。截断时追加 `[truncated;
//!   total N chars]` 注脚,便于用户判断是看完了还是被截了。
//! - **空 records**:返回 `# ZhuYe Session\n\n(empty)\n` —— 不是空字符串,
//!   让 Pill 渲染时仍能展示"已尝试 export"。
//!
//! 不在 S3 范围(留 v2.x):
//!
//! - 写到文件(`/export <path>`)—— 当前只走 Pill。
//! - 多 session 批量 export —— 当前单 session。
//! - YAML front-matter(obsidian 兼容)—— 留 v2.x 接 `serde_yaml`。

use std::path::Path;

use zhuye_protocol::{MessageRole, RolloutRecord};

/// 输出长度硬上限(单位:字符,不是字节)。超出后追加 `[truncated]` 注脚。
///
/// 选 4000 是因为:`/memory show` 用 2000,`/session ls` 用 ~50 条 × ~80 字
/// ≈ 4000;`/export` 内容密度相近,沿用同一数量级。
pub const MAX_MARKDOWN_CHARS: usize = 4000;

/// 把一组 `RolloutRecord` 渲染成 markdown 文本。
///
/// 输出骨架:
///
/// ```text
/// # ZhuYe Session <session_id>          # 一级标题:session 标题
/// (或 `# ZhuYe Session (empty)` —— 若 records 为空且不含 SessionMeta)
///
/// ## Session                              # 二级标题:session 元信息
/// - id: <uuid>                            # session id
/// - model: <model>                        # 使用的模型
/// - started_at: <iso8601>                 # 开始时间
///
/// ## Turn 1                               # 二级标题:第 1 轮
///
/// ### User                                # 三级标题:用户发言
///
/// <content>                               # 用户消息正文
///
/// ### Assistant                           # 三级标题:助手发言
///
/// <content>                               # 助手消息正文
///
/// ## Compaction                           # 二级标题:压缩记录
/// - strategy: microcompact                # 压缩策略名
/// - removed_count: 12                     # 被移除消息条数
/// - summary: ...                          # 压缩摘要
///
/// ## Fork                                 # 二级标题:分支记录
/// - parent_session_id: <uuid>             # 父 session id
/// - branch_name: <name>                   # 分支名
///
/// ## Discussion <uuid> (<mode>)           # 二级标题:讨论记录
/// - participants: a, b                    # 参与 agent 列表
/// - transcript:                           # 转录内容(嵌套 JSON)
///   <json pretty-printed>
/// ```
///
/// 截断发生在所有 record 渲染完之后,而不是逐 record 截断 —— 后者会让
/// 末尾 record 出现"半句话",前者保留 markdown 结构完整性。
pub fn to_markdown(records: &[RolloutRecord]) -> String {
    let mut out = String::new();
    let mut turn_counter: usize = 0;
    let mut session_meta_emitted = false;

    for rec in records {
        match rec {
            RolloutRecord::SessionMeta {
                session_id,
                model,
                started_at,
                workspace: _,
            } => {
                if session_meta_emitted {
                    // 理论上 SessionMeta 只 emit 一次,但 JSONL forward-compat
                    // 容许有多条(M9 → v0.3 都没用上,先留着预防 future)。
                    continue;
                }
                push_header(&mut out, "Session");
                out.push_str(&format!("- id: {session_id}\n"));
                out.push_str(&format!("- model: {model}\n"));
                out.push_str(&format!(
                    "- started_at: {}\n\n",
                    started_at.format("%Y-%m-%dT%H:%M:%SZ")
                ));
                session_meta_emitted = true;
            }
            RolloutRecord::Message { role, content, .. } => {
                turn_counter += 1;
                out.push_str(&format!("## Turn {turn_counter}\n\n"));
                out.push_str(&format!("### {}\n\n", role_heading(*role)));
                out.push_str(&format_content(content));
                out.push_str("\n\n");
            }
            RolloutRecord::Compaction {
                strategy,
                removed_count,
                summary,
                before_tokens,
                after_tokens,
                ..
            } => {
                push_header(&mut out, "Compaction");
                out.push_str(&format!("- strategy: {strategy}\n"));
                out.push_str(&format!("- removed_count: {removed_count}\n"));
                // v1.x:压缩前后 token(旧记录两字段为 0,不渲染误导行)。
                if *before_tokens > 0 || *after_tokens > 0 {
                    out.push_str(&format!("- tokens: {before_tokens} → {after_tokens}\n"));
                }
                out.push_str(&format!("- summary: {summary}\n\n"));
            }
            RolloutRecord::Fork {
                parent_session_id,
                branch_name,
            } => {
                push_header(&mut out, "Fork");
                out.push_str(&format!("- parent_session_id: {parent_session_id}\n"));
                out.push_str(&format!("- branch_name: {branch_name}\n\n"));
            }
            RolloutRecord::DiscussionTranscript {
                discussion_id,
                mode,
                participants,
                agent_id,
                transcript,
            } => {
                push_header(&mut out, &format!("Discussion {discussion_id}"));
                out.push_str(&format!("- mode: {mode}\n"));
                if let Some(aid) = agent_id {
                    out.push_str(&format!("- agent_id: {aid}\n"));
                }
                out.push_str(&format!(
                    "- participants: {}\n",
                    if participants.is_empty() {
                        "(none)".to_string()
                    } else {
                        participants.join(", ")
                    }
                ));
                // transcript 是 opaque JSON;做 pretty-print 让 markdown 阅读友好。
                let pretty = serde_json::to_string_pretty(transcript)
                    .unwrap_or_else(|_| "<unprintable json>".to_string());
                out.push_str("- transcript:\n```json\n");
                // transcript 自身也可能 16 KiB+;单独再限 2000 字符,
                // 否则单条 Discussion 就能吃光整 4000 字符预算。
                if pretty.chars().count() > 2000 {
                    let cut: String = pretty.chars().take(2000).collect();
                    out.push_str(&cut);
                    out.push_str(&format!(
                        "\n```\n- _transcript truncated, {} chars total_\n\n",
                        pretty.chars().count()
                    ));
                } else {
                    out.push_str(&pretty);
                    out.push_str("\n```\n\n");
                }
            }
            // v1.2 P0-3:checkpoint / rewind 记录渲染为简短 marker 段。
            RolloutRecord::Checkpoint {
                sha,
                label,
                created_at,
                ..
            } => {
                push_header(&mut out, "Checkpoint");
                out.push_str(&format!("- sha: `{sha}`\n"));
                if let Some(l) = label {
                    out.push_str(&format!("- label: {l}\n"));
                }
                out.push_str(&format!(
                    "- created_at: {}\n\n",
                    created_at.format("%Y-%m-%dT%H:%M:%SZ")
                ));
            }
            RolloutRecord::Rewind {
                target_sha,
                from_sha,
                at,
                ..
            } => {
                push_header(&mut out, "Rewind");
                out.push_str(&format!("- target_sha: `{target_sha}`\n"));
                out.push_str(&format!("- from_sha: `{from_sha}`\n"));
                out.push_str(&format!("- at: {}\n\n", at.format("%Y-%m-%dT%H:%M:%SZ")));
            }
            // v1.x:TokenCount 渲染为简短的 per-turn usage 段(累计展示交给 CLI
            // `session ls/show`,markdown 导出这里只逐条记录)。
            RolloutRecord::TokenCount {
                turn_id,
                usage,
                cost_usd,
                model,
                at,
                ..
            } => {
                push_header(&mut out, &format!("TokenCount {turn_id}"));
                // v1.x:模型归属(旧记录无该字段时不渲染此行)。
                if let Some(m) = &model {
                    out.push_str(&format!("- model: {m}\n"));
                }
                out.push_str(&format!("- input: {}\n", usage.input_tokens));
                out.push_str(&format!("- output: {}\n", usage.output_tokens));
                out.push_str(&format!("- cached: {}\n", usage.cached_tokens));
                out.push_str(&format!("- cache_write: {}\n", usage.cache_write_tokens));
                out.push_str(&format!("- total: {}\n", usage.total_tokens));
                if let Some(c) = cost_usd {
                    out.push_str(&format!("- cost_usd: {c:.6}\n"));
                }
                out.push_str(&format!("- at: {}\n\n", at.format("%Y-%m-%dT%H:%M:%SZ")));
            }
            // v1.x Plan mode:PlanRequest / PlanReady / PlanRejected 三类
            // 生命周期事件,各渲染为简短 marker 段(对齐 Checkpoint/Rewind 风格)。
            RolloutRecord::PlanRequest { plan_id, task, at } => {
                push_header(&mut out, &format!("PlanRequest {plan_id}"));
                out.push_str(&format!("- task: {task}\n"));
                out.push_str(&format!("- at: {}\n\n", at.format("%Y-%m-%dT%H:%M:%SZ")));
            }
            RolloutRecord::PlanReady {
                plan_id,
                markdown,
                path,
                at,
            } => {
                push_header(&mut out, &format!("PlanReady {plan_id}"));
                if let Some(p) = path {
                    out.push_str(&format!("- path: `{}`\n", p.display()));
                }
                out.push_str(&format!("- at: {}\n\n", at.format("%Y-%m-%dT%H:%M:%SZ")));
                // markdown 全文:单条再限 2000 字符(对齐 Discussion transcript
                // 的预算策略),外层 4000 字符 cap 会再兜底。
                out.push_str("```markdown\n");
                let md: String = markdown.chars().take(2000).collect();
                out.push_str(&md);
                if markdown.chars().count() > 2000 {
                    out.push_str(&format!(
                        "\n```\n_markdown truncated, {} chars total_\n\n",
                        markdown.chars().count()
                    ));
                } else {
                    out.push_str("\n```\n\n");
                }
            }
            RolloutRecord::PlanRejected {
                plan_id,
                reason,
                at,
            } => {
                push_header(&mut out, &format!("PlanRejected {plan_id}"));
                if let Some(r) = reason {
                    out.push_str(&format!("- reason: {r}\n"));
                }
                out.push_str(&format!("- at: {}\n\n", at.format("%Y-%m-%dT%H:%M:%SZ")));
            }
            RolloutRecord::PermissionModeChanged { from, to, at } => {
                push_header(&mut out, "PermissionModeChanged");
                out.push_str(&format!("- {} → {}\n", from.as_str(), to.as_str()));
                out.push_str(&format!("- at: {}\n\n", at.format("%Y-%m-%dT%H:%M:%SZ")));
            }
            // v1.x:记忆访问审计(读 = 检索注入;写 = compact 固化)。
            RolloutRecord::MemoryAccess {
                op,
                scope,
                agent_type,
                chars,
                entries,
                at,
                ..
            } => {
                let op_str = match op {
                    zhuye_protocol::MemoryOp::Read => "read",
                    zhuye_protocol::MemoryOp::Write => "write",
                };
                push_header(&mut out, &format!("MemoryAccess ({op_str})"));
                out.push_str(&format!("- scope: {scope}\n"));
                out.push_str(&format!("- agent_type: {agent_type}\n"));
                out.push_str(&format!("- chars: {chars}\n"));
                if *entries > 0 {
                    out.push_str(&format!("- entries: {entries}\n"));
                }
                out.push_str(&format!("- at: {}\n\n", at.format("%Y-%m-%dT%H:%M:%SZ")));
            }
        }
    }

    // 头部 + footer 包裹
    let body = wrap_header(records, out);

    if body.chars().count() > MAX_MARKDOWN_CHARS {
        // 截断到 N - footer_len,留 footer 位置,避免「截完内容后
        // 仍超长」的边角。
        let footer = format!(
            "\n\n[truncated at {MAX_MARKDOWN_CHARS} chars; full export has {} chars]",
            body.chars().count()
        );
        let budget = MAX_MARKDOWN_CHARS.saturating_sub(footer.chars().count() + 1);
        let head: String = body.chars().take(budget).collect();
        format!("{head}{footer}")
    } else {
        body
    }
}

/// 把渲染好的 body 加上 `# ZhuYe Session <id>` 头部。
///
/// 空 records 路径:`# ZhuYe Session (empty)` —— 让 Pill 至少能展示
/// "已尝试 export 且这条 session 没有内容"而不是空白。
fn wrap_header(records: &[RolloutRecord], body: String) -> String {
    let title = match records.first() {
        Some(RolloutRecord::SessionMeta { session_id, .. }) => {
            format!("# ZhuYe Session {session_id}\n\n")
        }
        _ => "# ZhuYe Session (empty)\n\n".to_string(),
    };
    format!("{title}{body}")
}

/// 推 `## <name>\n` 头部(无尾部 `\n`,由各 caller 自己加内容)。
fn push_header(out: &mut String, name: &str) {
    out.push_str(&format!("## {name}\n"));
}

// ── v1.x:完整转写稿导出(文件向)────────────────────────────────────
//
// 与 [`to_markdown`] 的分工:后者面向 Pill / slash 命令的**行内预览**,
// 有 4000 字符硬上限、结构紧凑;本节面向**落盘文件**(`session export`),
// 不截断,且逐条渲染全部 13 个 `RolloutRecord` 变体 + 文末 Usage 段
// (模型级 token 拆分 / 压缩节省 / 记忆访问审计)。
//
// 收编自 ReflectDesktop `src-tauri/src/commands/export.rs` —— 此前只有
// 桌面端能导出带 Usage 段的完整稿,CLI 的 `to_markdown` 缺 per-model
// 表与审计行。渲染拆成两个纯函数(无 IO,便于单测)。

use zhuye_fs::write_atomic;
use zhuye_protocol::{SessionStats, ThreadId};

/// 导出产物:落盘路径 + 字节数。
#[derive(Debug, Clone)]
pub struct MarkdownExport {
    pub path: std::path::PathBuf,
    pub bytes: u64,
}

/// 把 rollout 记录渲染成完整 markdown 转写稿(纯函数,无 IO)。
///
/// 覆盖全部 13 个变体;`TokenCount` / `Plan*` / `PermissionModeChanged`
/// 显式忽略(token 用量由文末 Usage 段统一聚合,plan / 权限轨迹不属于
/// 对话内容),显式忽略同时保证 match 穷尽 —— 未来新增变体编译期报错。
pub fn render_transcript(id: &ThreadId, records: &[RolloutRecord]) -> String {
    let mut out = String::new();
    out.push_str(&format!("# ZhuYe session `{id}`\n\n"));
    let mut session_meta_emitted = false;
    let mut count: usize = 0;
    for r in records {
        match r {
            RolloutRecord::SessionMeta { model, .. } => {
                if !session_meta_emitted {
                    out.push_str(&format!("**model**: `{model}`\n\n"));
                    session_meta_emitted = true;
                }
            }
            RolloutRecord::Message { role, content, .. } => {
                count += 1;
                // `content` 是 serde_json::Value;字符串直接当作文本。
                let text = match content {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let label = match role {
                    MessageRole::User => "**user**",
                    MessageRole::Assistant => "**agent**",
                    MessageRole::System => "**system**",
                    MessageRole::Tool => "**tool**",
                };
                out.push_str(&format!("\n## Turn {count}\n\n{label}: "));
                out.push_str(&text);
                out.push_str("\n\n");
            }
            RolloutRecord::Compaction {
                strategy,
                removed_count,
                summary,
                before_tokens,
                after_tokens,
                ..
            } => {
                // v1.x:before/after 是压缩前后的上下文 token 估算(旧 JSONL 缺省为 0)。
                let saved = before_tokens.saturating_sub(*after_tokens);
                out.push_str(&format!(
                    "\n---\n*compaction* (`{strategy}`, removed {removed_count} messages"
                ));
                if *before_tokens > 0 {
                    out.push_str(&format!(
                        ", {before_tokens} → {after_tokens} tokens (saved {saved})"
                    ));
                }
                out.push_str(")\n\n");
                if !summary.is_empty() {
                    out.push_str("> ");
                    out.push_str(summary);
                    out.push_str("\n\n");
                }
            }
            RolloutRecord::Fork {
                parent_session_id,
                branch_name,
            } => {
                out.push_str(&format!(
                    "\n*forked from `{parent_session_id}` as `{branch_name}`*\n\n"
                ));
            }
            RolloutRecord::Checkpoint { sha, label, .. } => {
                let tag = label.clone().unwrap_or_else(|| "checkpoint".to_string());
                out.push_str(&format!("\n*checkpoint `{tag}` @ `{sha}`*\n\n"));
            }
            RolloutRecord::Rewind {
                target_sha,
                from_sha,
                ..
            } => {
                out.push_str(&format!("\n*rewind* (`{from_sha}` → `{target_sha}`)\n\n"));
            }
            RolloutRecord::DiscussionTranscript {
                mode,
                participants,
                transcript,
                ..
            } => {
                out.push_str(&format!(
                    "\n## Discussion ({mode})\n\nParticipants: {}\n\n",
                    participants.join(", ")
                ));
                let pretty = match transcript {
                    serde_json::Value::String(s) => s.clone(),
                    other => serde_json::to_string_pretty(other).unwrap_or_default(),
                };
                out.push_str(&pretty);
                out.push_str("\n\n");
            }
            // v1.x:记忆访问审计(检索注入 / 固化写入)。落盘后可在导出稿
            // 回看「这个会话注入了哪些记忆 / 固化了什么」。
            RolloutRecord::MemoryAccess {
                op,
                scope,
                agent_type,
                chars,
                entries,
                ..
            } => {
                let (label, detail) = match op {
                    zhuye_protocol::MemoryOp::Read => (
                        "memory read",
                        format!("scope `{scope}`, agent `{agent_type}`, {chars} chars"),
                    ),
                    zhuye_protocol::MemoryOp::Write => (
                        "memory write",
                        format!(
                            "scope `{scope}`, agent `{agent_type}`, {entries} entries, {chars} chars"
                        ),
                    ),
                };
                out.push_str(&format!("\n*{label}* ({detail})\n\n"));
            }
            // v1.x:per-turn token 用量快照(累计自 EventMsg::TokenCount)。
            // 不逐条渲染 —— 改由文末「Usage」段统一聚合(含模型级拆分),
            // 此处显式忽略以保证 match 穷尽。
            RolloutRecord::TokenCount { .. } => {}
            // Plan 工作流与权限模式变更记录:转写稿不渲染,显式忽略以保证
            // match 穷尽。
            RolloutRecord::PlanRequest { .. }
            | RolloutRecord::PlanReady { .. }
            | RolloutRecord::PlanRejected { .. }
            | RolloutRecord::PermissionModeChanged { .. } => {}
        }
    }
    out
}

/// 渲染文末「Usage」段(纯函数)。
///
/// 复用 [`crate::index::session_stats`] 的规范聚合(模型级拆分 + 压缩
/// 节省 + 记忆访问),不在导出层复刻 token 聚合逻辑。统计缺省(会话尚无
/// 落盘记录)→ 空串。
pub fn render_usage_section(stats: Option<&SessionStats>) -> String {
    let Some(stats) = stats else {
        return String::new();
    };
    let mut out = String::from("\n## Usage\n\n");
    out.push_str(&format!("- model: `{}`\n", stats.model));
    out.push_str(&format!("- messages: {}\n", stats.message_count));
    out.push_str(&format!("- tool calls: {}\n", stats.tool_calls));
    out.push_str(&format!(
        "- tokens: {} in / {} out / {} total\n",
        stats.input_tokens, stats.output_tokens, stats.total_tokens
    ));
    if stats.cached_tokens > 0 {
        out.push_str(&format!("- cached: {}\n", stats.cached_tokens));
    }
    if let Some(cost) = stats.cost_usd {
        out.push_str(&format!("- cost: ${cost:.4}\n"));
    }
    if stats.compactions > 0 {
        out.push_str(&format!(
            "- compactions: {} (saved {} tokens)\n",
            stats.compactions,
            stats.compaction_tokens_saved()
        ));
    }
    if stats.memory_reads > 0 || stats.memory_writes > 0 {
        out.push_str(&format!(
            "- memory: {} reads / {} writes\n",
            stats.memory_reads, stats.memory_writes
        ));
    }
    if !stats.per_model.is_empty() {
        out.push_str("\n### Per model\n\n");
        out.push_str("| model | provider | calls | input | output | total | cost |\n");
        out.push_str("| --- | --- | ---: | ---: | ---: | ---: | ---: |\n");
        for m in &stats.per_model {
            let cost = m
                .cost_usd
                .map_or_else(|| "-".to_string(), |c| format!("${c:.4}"));
            out.push_str(&format!(
                "| `{}` | {} | {} | {} | {} | {} | {} |\n",
                m.model,
                m.provider.as_deref().unwrap_or("-"),
                m.calls,
                m.input_tokens,
                m.output_tokens,
                m.total_tokens,
                cost
            ));
        }
    }
    out.push('\n');
    out
}

/// 导出目录:`<sessions_base>/../exports`,即 `~/.zhuye/exports`。
///
/// 生产语义与 `zhuye_config::data_path(&["exports"])` 等价(base 恒为
/// `<data_dir>/sessions`);从 base 推导而不是直读 env,便于测试用任意
/// 临时目录驱动。
pub fn exports_dir(base: &Path) -> std::path::PathBuf {
    base.parent()
        .map_or_else(|| base.join("exports"), |p| p.join("exports"))
}

/// 回放 session → 完整转写稿 + Usage 段 → 原子写落盘,返回产物信息。
///
/// - `base` 为 sessions 根(`~/.zhuye/sessions`);产物写
///   [`exports_dir`] = `~/.zhuye/exports/<id>.md`;
/// - 写走 [`zhuye_fs::write_atomic`](symlink 穿透 / 权限不放大 /
///   失败保全)—— 桌面端旧实现的 `write_export` 会把 symlink 替换成
///   普通文件,这里统一到框架契约(导出稿同样可能是运维挂载的链接);
/// - 找不到 session 文件 → `Ok(None)`(与 `session_stats` 的 None 口径
///   一致,调用方自行决定报错还是跳过)。
pub async fn export_session_markdown(
    base: &Path,
    id: ThreadId,
) -> anyhow::Result<Option<MarkdownExport>> {
    let records = crate::reader::replay(base, id).await?;
    if records.is_empty() {
        return Ok(None);
    }
    let mut out = render_transcript(&id, &records);
    out.push_str(&render_usage_section(
        crate::index::session_stats(base, id).as_ref(),
    ));

    let dir = exports_dir(base);
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(format!("{id}.md"));
    write_atomic(&dest, out.as_bytes())?;
    Ok(Some(MarkdownExport {
        path: dest,
        bytes: out.len() as u64,
    }))
}

/// 把 `MessageRole` 映射成 markdown 标题大小写(`User` / `Assistant` /
/// `Tool` / `System` 四个枚举值)。
fn role_heading(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "User",
        MessageRole::Assistant => "Assistant",
        MessageRole::Tool => "Tool",
        MessageRole::System => "System",
    }
}

/// 把 `serde_json::Value` 渲染成 markdown 段落。
///
/// 优先级:
/// 1. 纯字符串 → 原样输出(`"hello"` → `hello`)。
/// 2. `{ "text": "..." }` / `{ "content": "..." }` 单字段对象 → 字段值
///    作为正文,匹配 `zhuye_core` 实际写入的 `ContentBlock` 形态。
/// 3. 其他(数组 / 嵌套对象) → pretty JSON(带 ```` ```json ```` fence),
///    让结构化内容不至于糊掉 markdown。
///
/// 不做内容截断;外层 `to_markdown` 的 4000 字符 cap 会兜底。
fn format_content(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) => {
            // 优先提取 text / content 单字段(对齐 zhuye_core 的 ContentBlock
            // 写入形态;同时也对 `Vec<ContentBlock>` 的伪 JSON 表示友好)。
            if let Some(serde_json::Value::String(t)) = map.get("text") {
                return t.clone();
            }
            if let Some(serde_json::Value::String(c)) = map.get("content") {
                return c.clone();
            }
            if map.len() == 1
                && let Some((_, v)) = map.iter().next()
                && let serde_json::Value::String(s) = v
            {
                return s.clone();
            }
            let pretty = serde_json::to_string_pretty(value).unwrap_or_default();
            format!("```json\n{pretty}\n```")
        }
        other => {
            let pretty = serde_json::to_string_pretty(other).unwrap_or_default();
            format!("```json\n{pretty}\n```")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use zhuye_protocol::{ThreadId, TurnId};

    // ── v1.x:完整转写稿(文件向)测试 ──────────────────────────────

    fn turn_id() -> TurnId {
        TurnId::parse_str("33333333-4444-5555-6666-777777777777").unwrap()
    }

    fn message(role: MessageRole, text: &str) -> RolloutRecord {
        RolloutRecord::Message {
            turn_id: turn_id(),
            role,
            content: serde_json::Value::String(text.to_string()),
        }
    }

    #[test]
    fn checkpoint_sha_is_rendered_as_balanced_inline_code() {
        let record = RolloutRecord::Checkpoint {
            turn_id: turn_id(),
            sha: "abc123".into(),
            label: Some("before".into()),
            created_at: Utc::now(),
        };
        let markdown = render_transcript(&ThreadId::new(), &[record]);
        assert!(markdown.contains("*checkpoint `before` @ `abc123`*"));
    }

    #[test]
    fn transcript_renders_messages_and_model() {
        let id = ThreadId::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let records = vec![
            RolloutRecord::SessionMeta {
                session_id: id,
                model: "openai/gpt-4o".into(),
                started_at: Utc::now(),
                workspace: None,
            },
            message(MessageRole::User, "hello"),
            message(MessageRole::Assistant, "hi"),
        ];
        let md = render_transcript(&id, &records);
        assert!(md.contains("**model**: `openai/gpt-4o`"));
        assert!(md.contains("## Turn 1"));
        assert!(md.contains("## Turn 2"));
        assert!(md.contains("**user**: hello"));
        assert!(md.contains("**agent**: hi"));
    }

    #[test]
    fn transcript_renders_memory_access_audit() {
        // v1.x:记忆访问审计(读 / 写两种 op)。
        let id = ThreadId::new();
        let records = vec![
            RolloutRecord::MemoryAccess {
                turn_id: turn_id(),
                op: zhuye_protocol::MemoryOp::Read,
                scope: "project,user".into(),
                agent_type: "coder".into(),
                chars: 1234,
                entries: 0,
                at: Utc::now(),
            },
            RolloutRecord::MemoryAccess {
                turn_id: turn_id(),
                op: zhuye_protocol::MemoryOp::Write,
                scope: "project".into(),
                agent_type: "coder".into(),
                chars: 56,
                entries: 3,
                at: Utc::now(),
            },
        ];
        let md = render_transcript(&id, &records);
        assert!(md.contains("*memory read* (scope `project,user`, agent `coder`, 1234 chars)"));
        assert!(
            md.contains("*memory write* (scope `project`, agent `coder`, 3 entries, 56 chars)")
        );
    }

    #[test]
    fn transcript_renders_compaction_token_saving() {
        let id = ThreadId::new();
        let records = vec![RolloutRecord::Compaction {
            turn_id: turn_id(),
            strategy: "microcompact".into(),
            removed_count: 12,
            summary: "<summary/>".into(),
            before_tokens: 9000,
            after_tokens: 2000,
        }];
        let md = render_transcript(&id, &records);
        assert!(md.contains("9000 → 2000 tokens (saved 7000)"));
    }

    #[test]
    fn transcript_omits_token_delta_for_legacy_records() {
        // 旧 JSONL 的 compaction 无 before/after 字段 → 反序列化为 0,
        // 此时不渲染 token 段(避免误导性的 "0 → 0")。
        let id = ThreadId::new();
        let records = vec![RolloutRecord::Compaction {
            turn_id: turn_id(),
            strategy: "microcompact".into(),
            removed_count: 3,
            summary: String::new(),
            before_tokens: 0,
            after_tokens: 0,
        }];
        let md = render_transcript(&id, &records);
        assert!(md.contains("removed 3 messages"));
        assert!(!md.contains("tokens (saved"));
    }

    #[test]
    fn usage_section_is_empty_without_stats() {
        assert_eq!(render_usage_section(None), "");
    }

    #[test]
    fn usage_section_renders_per_model_table() {
        // v1.x:文末 Usage 段 —— 总量 + 模型级拆分 + 压缩节省 + 记忆访问。
        let id = ThreadId::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let stats = SessionStats {
            session_id: id,
            model: "openai/gpt-4o".into(),
            started_at: Utc::now(),
            message_count: 12,
            tool_calls: 3,
            compactions: 1,
            compaction_before_tokens: 9000,
            compaction_after_tokens: 2000,
            memory_reads: 2,
            memory_writes: 1,
            input_tokens: 1000,
            output_tokens: 200,
            cached_tokens: 250,
            cache_write_tokens: 0,
            total_tokens: 1200,
            cost_usd: Some(0.0234),
            per_model: vec![zhuye_protocol::ModelUsage {
                model: "openai/gpt-4o".into(),
                provider: Some("openai".into()),
                calls: 4,
                input_tokens: 1000,
                output_tokens: 200,
                cached_tokens: 250,
                cache_write_tokens: 0,
                total_tokens: 1200,
                cost_usd: Some(0.0234),
            }],
        };
        let md = render_usage_section(Some(&stats));
        assert!(md.contains("## Usage"));
        assert!(md.contains("- model: `openai/gpt-4o`"));
        assert!(md.contains("- tokens: 1000 in / 200 out / 1200 total"));
        assert!(md.contains("- cost: $0.0234"));
        assert!(md.contains("- compactions: 1 (saved 7000 tokens)"));
        assert!(md.contains("- memory: 2 reads / 1 writes"));
        assert!(md.contains("### Per model"));
        assert!(md.contains("| `openai/gpt-4o` | openai | 4 | 1000 | 200 | 1200 | $0.0234 |"));
    }

    #[tokio::test]
    async fn export_session_markdown_writes_full_transcript() {
        // 落盘端到端:转写稿 + Usage 段 + exports 目录推导。
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("sessions");
        let sid = ThreadId::new();
        let day = base.join("2026/11/02");
        std::fs::create_dir_all(&day).unwrap();
        let mut body = String::new();
        body.push_str(
            &serde_json::to_string(&RolloutRecord::SessionMeta {
                session_id: sid,
                model: "openai/gpt-4o".into(),
                started_at: Utc::now(),
                workspace: None,
            })
            .unwrap(),
        );
        body.push('\n');
        body.push_str(&serde_json::to_string(&message(MessageRole::User, "你好")).unwrap());
        body.push('\n');
        body.push_str(
            &serde_json::to_string(&RolloutRecord::TokenCount {
                turn_id: turn_id(),
                usage: zhuye_protocol::TokenUsage::new(100, 50, 0),
                cost_usd: Some(0.01),
                model: Some("openai/gpt-4o".into()),
                provider: Some("openai".into()),
                at: Utc::now(),
            })
            .unwrap(),
        );
        body.push('\n');
        std::fs::write(day.join(format!("{sid}.jsonl")), body).unwrap();

        let out = export_session_markdown(&base, sid).await.unwrap().unwrap();
        assert_eq!(
            out.path,
            tmp.path().join("exports").join(format!("{sid}.md"))
        );
        let text = std::fs::read_to_string(&out.path).unwrap();
        assert!(text.contains("**model**: `openai/gpt-4o`"), "缺 model 行");
        assert!(text.contains("**user**: 你好"), "缺对话内容");
        assert!(text.contains("## Usage"), "缺 Usage 段");
        assert!(text.contains("### Per model"), "缺 per-model 表");
        assert_eq!(out.bytes, text.len() as u64);
    }

    #[tokio::test]
    async fn export_session_markdown_none_for_unknown_session() {
        let tmp = tempfile::tempdir().unwrap();
        let out = export_session_markdown(&tmp.path().join("sessions"), ThreadId::new())
            .await
            .unwrap();
        assert!(out.is_none(), "无记录的 session 应返回 None");
    }

    /// 拼一个最小的 SessionMeta + 1 条 User message,作为 fixture helper。
    fn fixture_basic() -> Vec<RolloutRecord> {
        let sid = ThreadId::new();
        let tid = TurnId::new();
        vec![
            RolloutRecord::SessionMeta {
                session_id: sid,
                model: "anthropic/claude-3-5-sonnet-latest".into(),
                started_at: Utc.with_ymd_and_hms(2026, 6, 18, 12, 0, 0).unwrap(),
                workspace: None,
            },
            RolloutRecord::message(tid, MessageRole::User, serde_json::json!("hi")),
        ]
    }

    /// 空 records 也要有 header —— Pill 不渲染空字符串。
    #[test]
    fn empty_records_returns_empty_header() {
        let md = to_markdown(&[]);
        assert!(md.contains("# ZhuYe Session (empty)"), "got: {md:?}");
    }

    /// SessionMeta + 单条 User message:头部含 model,正文含 "hi"。
    #[test]
    fn renders_session_meta_and_user_message() {
        let recs = fixture_basic();
        let sid_str = match recs.first().unwrap() {
            RolloutRecord::SessionMeta { session_id, .. } => session_id.to_string(),
            _ => unreachable!(),
        };
        let md = to_markdown(&recs);
        assert!(md.contains("# ZhuYe Session "), "missing H1, got: {md}");
        assert!(md.contains(&sid_str), "session id missing, got: {md}");
        assert!(
            md.contains("anthropic/claude-3-5-sonnet-latest"),
            "model missing, got: {md}"
        );
        assert!(md.contains("## Session"), "missing Session H2, got: {md}");
        assert!(md.contains("### User"), "missing User H3, got: {md}");
        assert!(md.contains("hi"), "user message body missing, got: {md}");
        assert!(md.contains("## Turn 1"), "missing Turn 1, got: {md}");
    }

    /// Assistant 文本 + Compaction + Fork 都各自正确渲染。
    #[test]
    fn renders_assistant_compaction_and_fork() {
        let sid = ThreadId::new();
        let tid = TurnId::new();
        let parent = ThreadId::new();
        let recs = vec![
            RolloutRecord::SessionMeta {
                session_id: sid,
                model: "openai/gpt-4o".into(),
                started_at: Utc.with_ymd_and_hms(2026, 6, 18, 13, 0, 0).unwrap(),
                workspace: None,
            },
            RolloutRecord::message(tid, MessageRole::User, serde_json::json!("q")),
            RolloutRecord::message(tid, MessageRole::Assistant, serde_json::json!("a")),
            RolloutRecord::Compaction {
                turn_id: tid,
                strategy: "microcompact".into(),
                removed_count: 7,
                summary: "<summary>old context</summary>".into(),
                before_tokens: 12_000,
                after_tokens: 3_000,
            },
            RolloutRecord::Fork {
                parent_session_id: parent,
                branch_name: "explorer".into(),
            },
        ];
        let md = to_markdown(&recs);
        assert!(md.contains("### Assistant"), "missing Assistant, got: {md}");
        assert!(
            md.contains("## Compaction"),
            "missing Compaction, got: {md}"
        );
        assert!(
            md.contains("- strategy: microcompact"),
            "missing strategy, got: {md}"
        );
        assert!(
            md.contains("- removed_count: 7"),
            "missing removed_count, got: {md}"
        );
        assert!(
            md.contains("<summary>old context</summary>"),
            "missing summary, got: {md}"
        );
        assert!(md.contains("## Fork"), "missing Fork, got: {md}");
        assert!(
            md.contains("- branch_name: explorer"),
            "missing branch_name, got: {md}"
        );
    }

    /// DiscussionTranscript 走 fenced JSON + participants 拼接。
    #[test]
    fn renders_discussion_transcript() {
        use uuid::Uuid;
        let did = Uuid::new_v4();
        let recs = vec![
            RolloutRecord::SessionMeta {
                session_id: ThreadId::new(),
                model: "m".into(),
                started_at: Utc::now(),
                workspace: None,
            },
            RolloutRecord::DiscussionTranscript {
                discussion_id: did,
                mode: "sequential".into(),
                participants: vec!["advocate".into(), "skeptic".into()],
                agent_id: None,
                transcript: serde_json::json!([{"from": "a", "text": "go"}]),
            },
        ];
        let md = to_markdown(&recs);
        assert!(
            md.contains(&format!("## Discussion {did}")),
            "missing Discussion H2, got: {md}"
        );
        assert!(md.contains("- mode: sequential"), "missing mode, got: {md}");
        assert!(
            md.contains("- participants: advocate, skeptic"),
            "missing participants, got: {md}"
        );
        assert!(md.contains("```json"), "missing JSON fence, got: {md}");
        assert!(
            md.contains("\"text\": \"go\""),
            "missing transcript body, got: {md}"
        );
    }

    /// v1.x Plan mode 四类变体各自渲染为简短 marker 段;
    /// PlanReady 的 markdown 全文进 fenced 块。
    #[test]
    fn renders_plan_lifecycle_and_permission_mode() {
        use zhuye_protocol::{PermissionMode, PlanId};
        let pid = PlanId::new();
        let recs = vec![
            RolloutRecord::SessionMeta {
                session_id: ThreadId::new(),
                model: "m".into(),
                started_at: Utc.with_ymd_and_hms(2026, 8, 5, 10, 0, 0).unwrap(),
                workspace: None,
            },
            RolloutRecord::PlanRequest {
                plan_id: pid,
                task: "refactor auth".into(),
                at: Utc.with_ymd_and_hms(2026, 8, 5, 10, 1, 0).unwrap(),
            },
            RolloutRecord::PlanReady {
                plan_id: pid,
                markdown: "## Plan\n- step 1".into(),
                path: Some(std::path::PathBuf::from("/ws/.zhuye/plan/abc.md")),
                at: Utc.with_ymd_and_hms(2026, 8, 5, 10, 2, 0).unwrap(),
            },
            RolloutRecord::PlanRejected {
                plan_id: pid,
                reason: Some("needs more detail".into()),
                at: Utc.with_ymd_and_hms(2026, 8, 5, 10, 3, 0).unwrap(),
            },
            RolloutRecord::PermissionModeChanged {
                from: PermissionMode::Prompt,
                to: PermissionMode::Plan,
                at: Utc.with_ymd_and_hms(2026, 8, 5, 10, 4, 0).unwrap(),
            },
        ];
        let md = to_markdown(&recs);
        // PlanRequest(计划请求)
        assert!(md.contains(&format!("## PlanRequest {pid}")), "got: {md}");
        assert!(md.contains("- task: refactor auth"), "got: {md}");
        // PlanReady:含 path + fenced markdown 全文
        assert!(md.contains(&format!("## PlanReady {pid}")), "got: {md}");
        assert!(md.contains("- path: `/ws/.zhuye/plan/abc.md`"), "got: {md}");
        assert!(md.contains("```markdown"), "got: {md}");
        assert!(md.contains("- step 1"), "got: {md}");
        // PlanRejected(计划被拒)
        assert!(md.contains(&format!("## PlanRejected {pid}")), "got: {md}");
        assert!(md.contains("- reason: needs more detail"), "got: {md}");
        // PermissionModeChanged(权限模式变更)
        assert!(md.contains("## PermissionModeChanged"), "got: {md}");
        assert!(md.contains("- prompt → plan"), "got: {md}");
    }

    /// PlanReady 的 markdown 超过 2000 字符时,fenced 块被截断并带注脚。
    #[test]
    fn renders_plan_ready_truncates_long_markdown() {
        use zhuye_protocol::PlanId;
        let big: String = "x".repeat(3000);
        let recs = vec![RolloutRecord::PlanReady {
            plan_id: PlanId::new(),
            markdown: big,
            path: None,
            at: Utc.with_ymd_and_hms(2026, 8, 5, 10, 0, 0).unwrap(),
        }];
        let md = to_markdown(&recs);
        assert!(
            md.contains("_markdown truncated, 3000 chars total_"),
            "got: {md}"
        );
    }

    /// 超过 `MAX_MARKDOWN_CHARS` 时追加 `[truncated ...]` 注脚,
    /// 并保留 markdown 结构完整性(footer 在 `\n\n` 后追加)。
    #[test]
    fn truncates_with_footer_when_oversize() {
        // 拼一个 5000 字符的 user message,单条就能撑爆预算。
        let big: String = "x".repeat(5000);
        let recs = vec![
            RolloutRecord::SessionMeta {
                session_id: ThreadId::new(),
                model: "m".into(),
                started_at: Utc::now(),
                workspace: None,
            },
            RolloutRecord::message(TurnId::new(), MessageRole::User, serde_json::json!(big)),
        ];
        let md = to_markdown(&recs);
        assert!(
            md.contains("[truncated at"),
            "missing truncation footer, got len {}",
            md.len()
        );
        assert!(
            md.chars().count() <= MAX_MARKDOWN_CHARS + 100,
            "truncated output still too long: {} chars",
            md.chars().count()
        );
    }

    /// `format_content` 对 `{text: "..."}` 单字段对象要解出 text,
    /// 对纯字符串直通,对数组走 fenced JSON。
    #[test]
    fn format_content_handles_shapes() {
        // 字符串直通。
        assert_eq!(format_content(&serde_json::json!("hello")), "hello");
        // `{text: ...}` 解 text。
        assert_eq!(format_content(&serde_json::json!({"text": "hi"})), "hi");
        // `{content: ...}` 解 content(对齐 ContentBlock 的另一个常见字段)。
        assert_eq!(format_content(&serde_json::json!({"content": "yo"})), "yo");
        // 数组 / 嵌套 → fenced JSON(`serde_json::to_string_pretty` 多行,
        // 所以断言子串即可)。
        let out = format_content(&serde_json::json!([1, 2, 3]));
        assert!(out.contains("```json"), "array must fence, got: {out}");
        assert!(out.contains("1"), "array body missing, got: {out}");
        assert!(out.contains("3"), "array tail missing, got: {out}");
    }
}
