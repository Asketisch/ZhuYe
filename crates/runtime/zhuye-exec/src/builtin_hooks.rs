//! v1.11 重构:内置上下文注入 hooks(归属 exec 层,不进引擎循环)。
//!
//! 两个特性原先实现在 `zhuye-core` 的 `pre_loop` 节点(core prompt 组装),
//! 按「核心层稳定、特性外移」原则改为 **UserPromptSubmit 内置 hook**:
//! 注入通道从 core prompt 块变为 `<system-reminder>` 包裹的 user 消息
//! (与外部 agent 的项目指令文件注入同款形态),引擎循环零触碰。
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
    /// 笔记根目录覆盖。`None` 走 [`zhuye_notes::resolve_notes_home`]
    /// (读 `ZHUYE_HOME`);测试注入临时目录,避免与同进程其他改 env
    /// 的用例互相踩掉数据根。
    notes_home: Option<std::path::PathBuf>,
    /// v1.11 M5a:workspace 归属过滤(sqlite 记忆后端时由 bootstrap
    /// 注入)。`Some(w)` 只召回同归属笔记,旧无归属笔记不参加跨项目
    /// 召回;`None` = 旧口径(不过滤)。
    workspace_filter: Option<String>,
}

impl CrossSessionRecallHook {
    pub fn new(current_thread: impl Into<String>) -> Self {
        Self {
            current_thread: current_thread.into(),
            notes_home: None,
            workspace_filter: None,
        }
    }

    /// 指定笔记根(其下 `session-notes/*.jsonl`),不读进程环境变量。
    pub fn with_notes_home(mut self, home: impl Into<std::path::PathBuf>) -> Self {
        self.notes_home = Some(home.into());
        self
    }

    /// v1.11 M5a:限定召回归属 workspace(sqlite 记忆后端装配用)。
    pub fn with_workspace_filter(mut self, workspace: impl Into<String>) -> Self {
        self.workspace_filter = Some(workspace.into());
        self
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
        let Some(home) = self
            .notes_home
            .clone()
            .or_else(zhuye_notes::resolve_notes_home)
        else {
            return Ok(HookDecision::Allow);
        };
        let hits = zhuye_notes::recall_cross_session_scoped(
            &home,
            &self.current_thread,
            query,
            zhuye_notes::RECALL_LIMIT,
            self.workspace_filter.as_deref(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造 UserPromptSubmit 事件(ctx 字段对本 hook 无意义,给占位值)。
    fn prompt_event(text: &str) -> HookEvent {
        HookEvent::UserPromptSubmit {
            text: text.to_string(),
            ctx: zhuye_hooks::event::HookContext {
                session_id: zhuye_protocol::ThreadId::new(),
                turn_id: zhuye_protocol::TurnId::new(),
                workspace: std::path::PathBuf::from("/tmp"),
                permission_mode: zhuye_protocol::PermissionMode::Auto,
                approval_policy: None,
            },
        }
    }

    /// `read_capped`:正常文件 → `Source:` 来源标注 + 原文全文。
    #[test]
    fn read_capped_加来源标注且保留全文() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("AGENTS.md");
        std::fs::write(&p, "规则甲\n规则乙\n").unwrap();
        let out = read_capped(&p).unwrap();
        assert!(
            out.starts_with(&format!("Source: {}\n\n", p.display())),
            "缺来源标注:{out:?}"
        );
        assert!(
            out.contains("规则甲") && out.contains("规则乙"),
            "原文丢失:{out:?}"
        );
    }

    /// `read_capped`:空白文件 / 不存在的文件都返回 `None`(不注入空段)。
    #[test]
    fn read_capped_空白与缺失返回无值() {
        let dir = tempfile::tempdir().unwrap();
        let blank = dir.path().join("blank.md");
        std::fs::write(&blank, "   \n\t\n").unwrap();
        assert!(read_capped(&blank).is_none(), "空白文件应返回 None");
        assert!(
            read_capped(&dir.path().join("nope.md")).is_none(),
            "缺失文件应返回 None"
        );
    }

    /// `read_capped`:超过 32K 字符上限时截断并追加截断标记(标记必须
    /// 指引用户用 read 工具取回完整内容)。
    #[test]
    fn read_capped_超长截断加标记() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.md");
        // 超上限 10 个字符,便于断言「恰好截到上限」。
        let over = AGENTS_MD_MAX_CHARS + 10;
        std::fs::write(&p, "x".repeat(over)).unwrap();
        let out = read_capped(&p).unwrap();
        assert!(
            out.contains("[AGENTS.md 超长已截断"),
            "缺截断标记:{:?}",
            &out[out.len() - 80..]
        );
        // 剥掉 `Source: <path>` 头(路径里可能含 'x')后再数正文截断长度。
        let body = out.split_once("\n\n").expect("Source 头后必有空行").1;
        assert_eq!(
            body.matches('x').count(),
            AGENTS_MD_MAX_CHARS,
            "截断长度应为上限值"
        );
    }

    /// 合并语义:用户级在前、项目级在后,`---` 分隔(项目级更具体应压轴)。
    #[test]
    fn read_agents_md_merged_用户级在前项目级在后() {
        let ws = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        std::fs::write(user.path().join("AGENTS.md"), "用户级总则").unwrap();
        std::fs::write(ws.path().join("AGENTS.md"), "项目级细则").unwrap();
        let merged = read_agents_md_merged(ws.path(), Some(user.path().to_path_buf()));
        let u = merged.find("用户级总则").expect("用户级丢失");
        let p = merged.find("项目级细则").expect("项目级丢失");
        assert!(u < p, "用户级必须在前:{merged:?}");
        assert!(merged.contains("\n\n---\n\n"), "缺分隔符:{merged:?}");
        assert_eq!(merged.matches("Source:").count(), 2, "两个来源都要标注");
    }

    /// 仅一源时无分隔符;双缺失时返回空串(hook 层据此跳过注入)。
    #[test]
    fn read_agents_md_merged_仅一源与双缺失() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("AGENTS.md"), "只有项目级").unwrap();
        let only_ws = read_agents_md_merged(ws.path(), None);
        assert!(only_ws.contains("只有项目级"));
        assert!(!only_ws.contains("---"), "单源不应有分隔符:{only_ws:?}");

        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            read_agents_md_merged(empty.path(), None),
            String::new(),
            "双缺失应为空串"
        );
    }

    /// `AgentsMdHook`:首轮注入(`## Project Instructions` 段 + 合并内容),
    /// 同一实例第二轮直接放行 —— 每会话只注一次,不随轮次堆叠。
    #[tokio::test]
    async fn agents_md_hook_首轮注入次轮放行() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("AGENTS.md"), "只注一次的规则").unwrap();
        let hook = AgentsMdHook::new(ws.path(), None);
        let first = hook.handle(&prompt_event("第一轮")).await.unwrap();
        match first {
            HookDecision::InjectMessage(m) => {
                assert!(
                    m.content.contains("## Project Instructions"),
                    "缺段头:{:?}",
                    m.content
                );
                assert!(
                    m.content.contains("只注一次的规则"),
                    "缺正文:{:?}",
                    m.content
                );
            }
            other => panic!("首轮应注入,实际 {other:?}"),
        }
        let second = hook.handle(&prompt_event("第二轮")).await.unwrap();
        assert!(
            matches!(second, HookDecision::Allow),
            "次轮必须放行,实际 {second:?}"
        );
    }

    /// `AgentsMdHook`:无任何 AGENTS.md 时首轮也放行(不注入空段)。
    #[tokio::test]
    async fn agents_md_hook_无文件不注入() {
        let ws = tempfile::tempdir().unwrap();
        let hook = AgentsMdHook::new(ws.path(), None);
        let d = hook.handle(&prompt_event("第一轮")).await.unwrap();
        assert!(matches!(d, HookDecision::Allow), "无文件应放行:{d:?}");
    }

    /// `CrossSessionRecallHook` 三段语义:
    /// 1. 命中时注入格式化笔记列表(序号 / 时间 / 来源会话 / 原文);
    /// 2. 空查询直接放行;
    /// 3. 当前会话自己的笔记被排除(`exclude_thread` —— 否则等于自言自语)。
    /// 笔记根经 `with_notes_home` 注入,不改 `ZHUYE_HOME`(该变量是进程级的,
    /// 与同进程其他 crate 的 env 测试并发时会互相清掉,CI 上表现为「命中却 Allow」)。
    #[tokio::test]
    async fn cross_session_recall_hook_命中注入_空查询放行_排除自身() {
        let home = tempfile::tempdir().unwrap();
        let notes = home.path().join("session-notes");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::write(
            notes.join("other-thread.jsonl"),
            "{\"text\": \"数据库是 postgres 16\", \"created_at\": \"2026-01-02T03:04:05Z\"}\n",
        )
        .unwrap();
        std::fs::write(
            notes.join("me.jsonl"),
            "{\"text\": \"selfonly 独有笔记\", \"created_at\": \"2026-01-01T00:00:00Z\"}\n",
        )
        .unwrap();

        let hook = CrossSessionRecallHook::new("current-thread").with_notes_home(home.path());
        // 空查询:不召回,直接放行。
        let empty_q = hook.handle(&prompt_event("   ")).await.unwrap();
        assert!(
            matches!(empty_q, HookDecision::Allow),
            "空查询应放行:{empty_q:?}"
        );
        // 相关查询(BM25 按空白分词,用 ASCII 关键词保证命中):注入且字段齐全。
        let hit = hook
            .handle(&prompt_event("postgres database"))
            .await
            .unwrap();
        match hit {
            HookDecision::InjectMessage(m) => {
                assert!(
                    m.content.contains("数据库是 postgres 16"),
                    "缺原文:{:?}",
                    m.content
                );
                assert!(
                    m.content.contains("other-thread"),
                    "缺来源会话:{:?}",
                    m.content
                );
                assert!(m.content.contains("2026-01-02"), "缺时间:{:?}", m.content);
                assert!(m.content.contains("1. "), "缺序号:{:?}", m.content);
            }
            other => panic!("命中应注入,实际 {other:?}"),
        }

        // 排除当前会话:关键词 "selfonly" 只在 "me" 自己的笔记里,hook 绑定
        // "me" 时唯一候选被排除 → 零候选放行。
        let self_hook = CrossSessionRecallHook::new("me").with_notes_home(home.path());
        let self_hit = self_hook.handle(&prompt_event("selfonly")).await.unwrap();
        assert!(
            matches!(self_hit, HookDecision::Allow),
            "自身笔记应被排除:{self_hit:?}"
        );
    }
}
