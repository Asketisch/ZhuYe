//! v1.11 重构:内置上下文注入 hooks(归属 exec 层,不进引擎循环)。
//!
//! 两个特性原先实现在 `zhuye-core` 的 `pre_loop` 节点(core prompt 组装),
//! 按「核心层稳定、特性外移」原则改为 **UserPromptSubmit 内置 hook**:
//! 注入通道从 core prompt 块变为 `<system-reminder>` 包裹的 user 消息
//! (与 Claude Code 的 CLAUDE.md 注入同款形态),引擎循环零触碰。
//!
//! - [`AgentsMdHook`] —— 会话首轮注入 AGENTS.md(用户级 + 项目级合并,
//!   `[active] agents_md = false` 时由注册方跳过注册);`AtomicBool`
//!   保证每会话只注一次,不随轮次堆叠。
//! - [`CrossSessionRecallHook`] —— 每轮以当前 prompt 为查询,BM25 召回
//!   其他会话的笔记(`zhuye_notes::recall_cross_session`),零相关不注入。
//!
//! 注册点:`headless::bootstrap_normal` / `bootstrap_resumed`(serve/exec
//! 共用);Desktop 走 facade 的 hook_engine 同样注册(见 thread_factory)。

use std::sync::atomic::{AtomicBool, Ordering};

use zhuye_hooks::decision::{HookDecision, SystemMessage};
use zhuye_hooks::event::{HookEvent, HookEventKind};
use zhuye_hooks::hook::{Hook, HookError};

/// 单文件注入字符上限(与原 pre_loop 实现一致)。
const AGENTS_MD_MAX_CHARS: usize = 32 * 1024;

/// 读取单个 AGENTS.md 文件并加来源标注;超长截断。
fn read_capped(path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let mut out = format!("Source: {}\n\n", path.display());
    if raw.chars().count() > AGENTS_MD_MAX_CHARS {
        out.push_str(&raw.chars().take(AGENTS_MD_MAX_CHARS).collect::<String>());
        out.push_str("\n\n[AGENTS.md 超长已截断,完整内容请用 read 工具查看该文件]");
    } else {
        out.push_str(raw);
    }
    Some(out)
}

/// 合并用户级(`<data_dir>/AGENTS.md`)与项目级(`<workspace>/AGENTS.md`)。
/// 用户级在前、项目级在后(项目级更具体)。与原 pre_loop 实现同逻辑。
fn read_agents_md_merged(
    workspace: &std::path::Path,
    user_data_dir: Option<std::path::PathBuf>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(dir) = user_data_dir {
        if let Some(text) = read_capped(&dir.join("AGENTS.md")) {
            parts.push(text);
        }
    }
    if let Some(text) = read_capped(&workspace.join("AGENTS.md")) {
        parts.push(text);
    }
    parts.join("\n\n---\n\n")
}

/// AGENTS.md 会话首轮注入 hook。
///
/// 注入文本沿用 `## Project Instructions` 段格式(与原 pre_loop / core
/// prompt 块一致,通过 [`zhuye_prompt::append_agents_md_section`] 生成,
/// 保证两种通道下的段格式不漂移)。
pub struct AgentsMdHook {
    workspace: std::path::PathBuf,
    user_data_dir: Option<std::path::PathBuf>,
    /// 每会话只注一次(hook 实例随 thread 生命周期,SessionStart 后首个
    /// UserPromptSubmit 注入,后续跳过 —— 避免逐轮堆叠)。
    injected: AtomicBool,
}

impl AgentsMdHook {
    pub fn new(
        workspace: impl Into<std::path::PathBuf>,
        user_data_dir: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            workspace: workspace.into(),
            user_data_dir,
            injected: AtomicBool::new(false),
        }
    }
}

#[async_trait::async_trait]
impl Hook for AgentsMdHook {
    fn name(&self) -> &str {
        "agents-md-inject"
    }

    fn description(&self) -> &str {
        "会话首轮把 AGENTS.md(用户级 + 项目级)注入模型上下文"
    }

    fn events(&self) -> &[HookEventKind] {
        &[HookEventKind::UserPromptSubmit]
    }

    async fn handle(&self, _event: &HookEvent) -> Result<HookDecision, HookError> {
        // 首轮语义:swap 防并发双注入(cargo test 多线程下同 hook 也不会
        // 并发同一会话,但防御性取值)。
        if self.injected.swap(true, Ordering::Relaxed) {
            return Ok(HookDecision::Allow);
        }
        let content = read_agents_md_merged(&self.workspace, self.user_data_dir.clone());
        if content.trim().is_empty() {
            return Ok(HookDecision::Allow);
        }
        let mut core = String::new();
        zhuye_prompt::append_agents_md_section(&mut core, &content);
        // append_agents_md_section 会加段头;这里只需正文段(strip 掉外层
        // 空行由 SystemMessage 包装 system-reminder 时统一)。
        Ok(HookDecision::InjectMessage(SystemMessage::new(core)))
    }
}

/// 跨会话笔记召回 hook:每轮以当前 prompt 为查询,BM25 取其他会话的
/// `session-notes/*.jsonl` 笔记 top-k,零相关不注入(宁缺毋滥)。
pub struct CrossSessionRecallHook {
    /// 排除的当前会话 id(`session-notes/<thread>.jsonl` 文件名 stem)。
    current_thread: String,
}

impl CrossSessionRecallHook {
    pub fn new(current_thread: impl Into<String>) -> Self {
        Self {
            current_thread: current_thread.into(),
        }
    }
}

#[async_trait::async_trait]
impl Hook for CrossSessionRecallHook {
    fn name(&self) -> &str {
        "cross-session-recall"
    }

    fn description(&self) -> &str {
        "按查询相关度召回其他会话的笔记(BM25 top-k,零相关不注入)"
    }

    fn events(&self) -> &[HookEventKind] {
        &[HookEventKind::UserPromptSubmit]
    }

    async fn handle(&self, event: &HookEvent) -> Result<HookDecision, HookError> {
        let HookEvent::UserPromptSubmit { text, .. } = event else {
            return Ok(HookDecision::Allow);
        };
        let query = text.trim();
        if query.is_empty() {
            return Ok(HookDecision::Allow);
        }
        let Some(home) = zhuye_notes::resolve_notes_home() else {
            return Ok(HookDecision::Allow);
        };
        let hits = zhuye_notes::recall_cross_session(
            &home,
            &self.current_thread,
            query,
            zhuye_notes::RECALL_LIMIT,
        );
        if hits.is_empty() {
            return Ok(HookDecision::Allow);
        }
        let mut body = String::from("来自其他会话的相关笔记(BM25 召回,供参考;不保证仍有效):\n");
        for (i, n) in hits.iter().enumerate() {
            let at = n
                .created_at
                .map_or_else(|| "未知时间".to_string(), |t| t.to_rfc3339());
            body.push_str(&format!(
                "{}. [{at} 会话 {}] {}\n",
                i + 1,
                n.thread_id,
                n.text
            ));
        }
        Ok(HookDecision::InjectMessage(SystemMessage::new(body)))
    }
}
