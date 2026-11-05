//! 导入中间表示(IR)—— 与具体 Harness 无关的会话模型。
//!
//! 三家来源(Claude Code JSONL / Codex rollout / ZCode SQLite)各自经
//! adapter 解析为这里的 [`ImportedSession`],再由 `convert` 层统一降为
//! 烛夜 `RolloutRecord` 序列。IR 刻意贴近烛夜的 turn 模型(一个 turn =
//! 一条 user 消息 + 一条时序折叠的 assistant 消息),让转换层保持薄。

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use zhuye_protocol::{ThreadId, TurnId};

/// 支持的外部 Harness 来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HarnessKind {
    /// Claude Code:`~/.claude/projects/<escaped-cwd>/<session-uuid>.jsonl`。
    ClaudeCode,
    /// Codex CLI / Desktop:`~/.codex/sessions/**/rollout-*.jsonl`。
    Codex,
    /// ZCode:`~/.zcode/cli/db/db.sqlite`(message/part 两张 JSON 表)。
    Zcode,
}

impl HarnessKind {
    /// CLI / MCP 参数面的 kebab-case 名。
    pub fn as_str(&self) -> &'static str {
        match self {
            HarnessKind::ClaudeCode => "claude-code",
            HarnessKind::Codex => "codex",
            HarnessKind::Zcode => "zcode",
        }
    }

    /// 解析 CLI / MCP 参数;`None` = 未知来源名。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude-code" | "claude_code" | "claude" => Some(HarnessKind::ClaudeCode),
            "codex" => Some(HarnessKind::Codex),
            "zcode" => Some(HarnessKind::Zcode),
            _ => None,
        }
    }
}

impl std::fmt::Display for HarnessKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一个待导入的会话来源:要么是单个 JSONL 文件,要么是 SQLite 库里的
/// 一个会话(以 `sess_<uuid>` 形式的 key 定位)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportSource {
    /// Claude Code / Codex 的单文件来源。
    JsonlFile(PathBuf),
    /// ZCode 的 SQLite 来源:`db` 为库文件路径,`key` 为 `session.id`
    /// 列原值(如 `sess_022f91d6-…`,子代理为 `sess_subagent_agent_…`)。
    SqliteSession { db: PathBuf, key: String },
}

impl ImportSource {
    /// 展示用短标识(错误信息与报告行使用)。
    pub fn display(&self) -> String {
        match self {
            ImportSource::JsonlFile(p) => p.display().to_string(),
            ImportSource::SqliteSession { db, key } => {
                format!("{}#{key}", db.display())
            }
        }
    }
}

/// 解析 / 转换期的可选开关(CLI 与 MCP 工具共用同一组)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportOptions {
    /// 保留 reasoning / thinking 内容(折叠为 assistant 文本块)。
    /// 默认丢弃:烛夜 rollout 无独立 thinking 块,且历史思考链对续跑
    /// 价值低、体积大。
    pub include_thinking: bool,
    /// 保留 Harness 注入的系统上下文行(Codex `<environment_context>`、
    /// Claude `<command-name>`、ZCode `# Environment` 等)。默认过滤:
    /// 它们是原生 Harness 的运行时脚手架,导入后对新 Harness 是噪音。
    pub keep_injected: bool,
    /// 保留子代理 / sidechain 行(Claude Code `isSidechain:true`)。
    /// 默认跳过:子代理轨迹与主对话交织,直接合并会打乱 turn 结构。
    pub include_sidechain: bool,
}

/// 一条导入警告(不中断,累积到 `ImportedSession.warnings`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportWarning {
    /// 机器可读代码(如 `"unknown-record"` / `"invalid-uuid"`)。
    pub code: &'static str,
    /// 人类可读详情。
    pub detail: String,
}

impl ImportWarning {
    /// 便捷构造。
    pub fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for ImportWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.detail)
    }
}

/// user 侧内容块(仅 Text / Image 两种,与烛夜 user 消息域一致)。
#[derive(Debug, Clone, PartialEq)]
pub enum UserBlock {
    /// 纯文本。
    Text { text: String },
    /// 图片(字节与 MIME;源为 base64 时 adapter 已解码)。
    Image { data: Vec<u8>, mime_type: String },
}

/// assistant 侧时序块,一一对应烛夜 `ContentBlock` 的可导入子集。
#[derive(Debug, Clone, PartialEq)]
pub enum IrBlock {
    /// 可见文本回复。
    Text { text: String },
    /// 思考链(默认在 convert 层丢弃 / 折叠)。
    Thinking { text: String },
    /// 工具调用。
    ToolUse {
        id: String,
        name: String,
        args: serde_json::Value,
    },
    /// 工具结果(文本化;非字符串输出由 adapter 先做 JSON 序列化)。
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
}

/// 单轮 turn 的 token 用量(三家源的粒度与字段名不一,统一到此)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportedUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 命中缓存的输入 token(计在 input 内,不重复计入总量)。
    pub cached_tokens: u64,
}

/// 一个对话轮:一条 user 输入 + 之后全部 assistant 时序块。
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedTurn {
    pub turn_id: TurnId,
    /// 轮起始时间(源行时间戳;缺失则 `None`,convert 层回退会话级时间)。
    pub started_at: Option<DateTime<Utc>>,
    pub user: Vec<UserBlock>,
    pub assistant: Vec<IrBlock>,
    pub usage: Option<ImportedUsage>,
}

impl ImportedTurn {
    /// 新建一个空轮(新 turn_id)。
    pub fn empty() -> Self {
        Self {
            turn_id: TurnId::new(),
            started_at: None,
            user: Vec::new(),
            assistant: Vec::new(),
            usage: None,
        }
    }

    /// 是否没有任何可导入内容(convert 层据此跳过)。
    pub fn is_empty(&self) -> bool {
        self.user.is_empty() && self.assistant.is_empty()
    }
}

/// 会话级条目:要么是一个 turn,要么是位置敏感的压缩摘要。
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// 对话轮。
    Turn(ImportedTurn),
    /// 压缩摘要(源顺序保持在所处 turn 之后,与原生 compaction 语义一致)。
    Compaction {
        summary: String,
        at: Option<DateTime<Utc>>,
    },
}

/// 解析完成的完整会话(待 convert 层降为 `RolloutRecord` 序列)。
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedSession {
    pub kind: HarnessKind,
    /// 源会话 id 原字符串(审计与幂等判断用)。
    pub source_id: String,
    /// 目标烛夜 thread id:源 id 是合法 UUID 则原样保留(去 `sess_` /
    /// `sess_subagent_agent_` 前缀),否则随机分配并记 warning。
    pub thread_id: ThreadId,
    pub model: String,
    pub started_at: DateTime<Utc>,
    /// 源会话工作区(cwd / directory),透传给烛夜 `SessionMeta.workspace`。
    pub workspace: Option<String>,
    /// 源显式标题(如 Claude `ai-title`);写入 `_names/<id>.name`。
    pub title: Option<String>,
    pub entries: Vec<Entry>,
    pub warnings: Vec<ImportWarning>,
}

impl ImportedSession {
    /// 统计可导入的 turn 数(空轮不计)。
    pub fn turn_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e, Entry::Turn(t) if !t.is_empty()))
            .count()
    }
}

/// 把 RFC3339 / RFC3339+millis 时间戳容错解析为 UTC;失败返回 `None`。
pub(crate) fn parse_timestamp(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// 从路径提取不含扩展名的文件名(会话 id 提取用)。
pub(crate) fn file_stem(path: &Path) -> Option<&str> {
    path.file_stem().and_then(|s| s.to_str())
}
