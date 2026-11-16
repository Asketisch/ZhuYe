//! # Hook 体系扩展(F61)
//!
//! 插件钩子补新事件 + 「一段提示词当裁判」的 hook + HTTP 回调 hook。
//!
//! ## 新增钩点
//!
//! 现有:`PreToolUse` / `PostToolUse` / `Stop` / `SessionStart` 等。新增:
//!
//! - [`HookKind::BeforeModel`] —— 模型调用**前**(可改 prompt / 拒绝调用);
//! - [`HookKind::AfterModel`] —— 模型调用**后**(可改输出 / 注入提醒);
//! - [`HookKind::UserPromptTransformed`] —— 用户输入被处理后;
//! - [`HookKind::PreMcpToolCall`] —— MCP 工具调用前(与 PreToolUse 分开,
//!   因为 MCP 工具来自外部,治理策略可能不同)。
//!
//! ## 三种 hook 形态
//!
//! 1. **进程内**([`InProcessHook`])—— 类型化接口,最可靠;
//! 2. **HTTP 回调**([`HttpHook`])—— 外部服务,exit 2 语义映射为 deny;
//! 3. **提示词裁判**([`PromptJudgeHook`])—— 用一段提示词 + 小模型判
//!    "该不该放行"。
//!
//! ## 为什么 `exit 2 = deny` 要显式兼容
//!
//! Claude Code 的 shell hook 约定 exit 2 = 阻止。很多社区 hook 按这个
//! 约定写。本模块显式兼容它 —— 不兼容意味着用户从别处搬来的 hook 会
//! **静默失效**(exit 2 被当成普通错误忽略)。

use serde::{Deserialize, Serialize};

/// hook 事件类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    // ── 现有 ──
    PreToolUse,
    PostToolUse,
    Stop,
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    // ── F61 新增 ──
    /// 模型调用前。
    BeforeModel,
    /// 模型调用后。
    AfterModel,
    /// 用户输入被转换后。
    UserPromptTransformed,
    /// MCP 工具调用前。
    PreMcpToolCall,
}

impl HookKind {
    pub const ALL: [HookKind; 10] = [
        HookKind::PreToolUse,
        HookKind::PostToolUse,
        HookKind::Stop,
        HookKind::SessionStart,
        HookKind::SessionEnd,
        HookKind::UserPromptSubmit,
        HookKind::BeforeModel,
        HookKind::AfterModel,
        HookKind::UserPromptTransformed,
        HookKind::PreMcpToolCall,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            HookKind::PreToolUse => "pre_tool_use",
            HookKind::PostToolUse => "post_tool_use",
            HookKind::Stop => "stop",
            HookKind::SessionStart => "session_start",
            HookKind::SessionEnd => "session_end",
            HookKind::UserPromptSubmit => "user_prompt_submit",
            HookKind::BeforeModel => "before_model",
            HookKind::AfterModel => "after_model",
            HookKind::UserPromptTransformed => "user_prompt_transformed",
            HookKind::PreMcpToolCall => "pre_mcp_tool_call",
        }
    }

    pub fn parse(s: &str) -> Option<HookKind> {
        HookKind::ALL
            .iter()
            .copied()
            .find(|k| k.as_str() == s.trim().to_ascii_lowercase())
    }

    /// 是否 F61 新增。
    pub fn is_new(self) -> bool {
        matches!(
            self,
            HookKind::BeforeModel
                | HookKind::AfterModel
                | HookKind::UserPromptTransformed
                | HookKind::PreMcpToolCall
        )
    }

    /// 该钩点能否**拒绝**(vs 只能注入提醒)。
    pub fn can_deny(self) -> bool {
        matches!(
            self,
            HookKind::PreToolUse | HookKind::PreMcpToolCall | HookKind::BeforeModel
        )
    }
}

/// hook 裁决。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookVerdict {
    /// 放行。
    Allow,
    /// 拒绝(带理由)。
    Deny { reason: String },
    /// 注入提醒(不阻断)。
    Inject { text: String },
    /// 改写内容(如 BeforeModel 改 prompt)。
    Modify { content: String },
}

impl HookVerdict {
    /// 是否阻断。
    pub fn is_blocking(&self) -> bool {
        matches!(self, HookVerdict::Deny { .. })
    }

    /// 从 shell hook 的退出码 + stdout 解析(兼容 Claude Code 约定)。
    ///
    /// - exit 0 + 空 stdout → Allow
    /// - exit 0 + 非空 stdout → Inject(把 stdout 当提醒)
    /// - exit 2 → Deny(约定:exit 2 = 阻止,stderr 当理由)
    /// - 其它非零 → Deny(保守:异常退出视为拒绝)
    pub fn from_shell(exit_code: i32, stdout: &str, stderr: &str) -> HookVerdict {
        match exit_code {
            0 => {
                let trimmed = stdout.trim();
                if trimmed.is_empty() {
                    HookVerdict::Allow
                } else {
                    HookVerdict::Inject {
                        text: trimmed.to_string(),
                    }
                }
            }
            2 => HookVerdict::Deny {
                reason: if stderr.trim().is_empty() {
                    "blocked by hook (exit 2)".to_string()
                } else {
                    stderr.trim().to_string()
                },
            },
            other => HookVerdict::Deny {
                reason: format!("hook failed with exit code {other}"),
            },
        }
    }
}

/// hook 声明(配置形态)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookDecl {
    /// hook 名(唯一)。
    pub name: String,
    /// 关心的事件。
    pub events: Vec<HookKind>,
    /// 形态。
    pub form: HookForm,
    /// 是否启用。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 超时(秒)。
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_true() -> bool {
    true
}
fn default_timeout() -> u64 {
    10
}

/// hook 形态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookForm {
    /// 进程内(名字对应内置实现)。
    InProcess { handler: String },
    /// shell 命令。
    Shell { command: String },
    /// HTTP 回调。
    Http { url: String, method: String },
    /// 提示词裁判(用模型判定)。
    PromptJudge {
        /// 评判提示词。
        prompt: String,
        /// 用哪个模型(空 = 默认小模型)。
        #[serde(default)]
        model: Option<String>,
    },
}

impl HookForm {
    /// 形态名。
    pub fn kind_name(&self) -> &'static str {
        match self {
            HookForm::InProcess { .. } => "in_process",
            HookForm::Shell { .. } => "shell",
            HookForm::Http { .. } => "http",
            HookForm::PromptJudge { .. } => "prompt_judge",
        }
    }

    /// 是否需要网络(HTTP / PromptJudge)。
    pub fn needs_network(&self) -> bool {
        matches!(self, HookForm::Http { .. } | HookForm::PromptJudge { .. })
    }
}

impl HookDecl {
    /// 是否关心某事件。
    pub fn handles(&self, kind: HookKind) -> bool {
        self.enabled && self.events.contains(&kind)
    }
}

/// hook 注册表。
#[derive(Debug, Default)]
pub struct HookRegistry {
    hooks: Vec<HookDecl>,
}

impl HookRegistry {
    pub fn new() -> HookRegistry {
        HookRegistry::default()
    }

    pub fn register(&mut self, decl: HookDecl) {
        self.hooks.push(decl);
    }

    /// 关心某事件的 hook(按注册顺序)。
    pub fn for_event(&self, kind: HookKind) -> Vec<&HookDecl> {
        self.hooks.iter().filter(|h| h.handles(kind)).collect()
    }

    /// 全部 hook。
    pub fn all(&self) -> &[HookDecl] {
        &self.hooks
    }

    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// 按事件统计 hook 数。
    pub fn counts_by_event(&self) -> std::collections::BTreeMap<&'static str, usize> {
        let mut out = std::collections::BTreeMap::new();
        for kind in HookKind::ALL {
            let n = self.for_event(kind).len();
            if n > 0 {
                out.insert(kind.as_str(), n);
            }
        }
        out
    }

    /// 合并多个裁决:任一 Deny → Deny(Deny 优先);
    /// 否则 Inject 拼接;否则 Modify 取最后一个;否则 Allow。
    pub fn merge(verdicts: &[HookVerdict]) -> HookVerdict {
        // Deny 优先
        for v in verdicts {
            if let HookVerdict::Deny { reason } = v {
                return HookVerdict::Deny {
                    reason: reason.clone(),
                };
            }
        }
        // Modify 取最后
        let modified = verdicts.iter().rev().find_map(|v| match v {
            HookVerdict::Modify { content } => Some(content.clone()),
            _ => None,
        });
        // Inject 拼接
        let injected: Vec<&str> = verdicts
            .iter()
            .filter_map(|v| match v {
                HookVerdict::Inject { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        if let Some(content) = modified {
            return HookVerdict::Modify { content };
        }
        if !injected.is_empty() {
            return HookVerdict::Inject {
                text: injected.join("\n"),
            };
        }
        HookVerdict::Allow
    }
}

/// 提示词裁判 hook 的提示词构造。
///
/// 把「要判断什么 + 待判内容」拼成固定形状,并要求模型只回 `allow` / `deny`。
pub fn render_judge_prompt(criteria: &str, subject: &str) -> String {
    format!(
        "你是审批裁判。根据以下标准判断是否放行。\n\n\
         ## 标准\n{criteria}\n\n\
         ## 待判内容\n{subject}\n\n\
         ## 输出格式\n\
         只输出一行:`allow` 或 `deny: <理由>`\n"
    )
}

/// 解析裁判模型输出。
pub fn parse_judge_output(output: &str) -> HookVerdict {
    let trimmed = output.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("allow") {
        HookVerdict::Allow
    } else if let Some(rest) = lower.strip_prefix("deny") {
        // 取原始大小写版本的理由。
        let original_reason = trimmed
            .get(4..)
            .unwrap_or("")
            .trim_start_matches([':', ' ', '-'])
            .trim();
        let reason = if original_reason.is_empty() {
            "denied by prompt judge".to_string()
        } else {
            original_reason.to_string()
        };
        let _ = rest;
        HookVerdict::Deny { reason }
    } else {
        // 无法解析 → 保守拒绝(裁判失效应 fail-closed)。
        HookVerdict::Deny {
            reason: format!("judge output unparseable: {}", truncate(trimmed, 80)),
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_kind_roundtrip() {
        for k in HookKind::ALL {
            assert_eq!(HookKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(HookKind::parse("bogus"), None);
    }

    #[test]
    fn new_hook_kinds_classified() {
        assert!(HookKind::BeforeModel.is_new());
        assert!(HookKind::AfterModel.is_new());
        assert!(HookKind::PreMcpToolCall.is_new());
        assert!(!HookKind::PreToolUse.is_new());
    }

    #[test]
    fn deny_capability() {
        assert!(HookKind::PreToolUse.can_deny());
        assert!(HookKind::BeforeModel.can_deny());
        assert!(HookKind::PreMcpToolCall.can_deny());
        assert!(!HookKind::PostToolUse.can_deny(), "事后不能拒绝");
        assert!(!HookKind::AfterModel.can_deny());
    }

    #[test]
    fn shell_exit_zero_empty_is_allow() {
        assert_eq!(HookVerdict::from_shell(0, "", ""), HookVerdict::Allow);
        assert_eq!(HookVerdict::from_shell(0, "  \n", ""), HookVerdict::Allow);
    }

    #[test]
    fn shell_exit_zero_with_output_injects() {
        let v = HookVerdict::from_shell(0, "记得跑测试", "");
        assert_eq!(
            v,
            HookVerdict::Inject {
                text: "记得跑测试".into()
            }
        );
    }

    #[test]
    fn shell_exit_two_is_deny() {
        let v = HookVerdict::from_shell(2, "", "不要删这个文件");
        assert_eq!(
            v,
            HookVerdict::Deny {
                reason: "不要删这个文件".into()
            }
        );
    }

    #[test]
    fn shell_exit_two_without_stderr() {
        let v = HookVerdict::from_shell(2, "", "");
        assert!(matches!(v, HookVerdict::Deny { .. }));
        if let HookVerdict::Deny { reason } = v {
            assert!(reason.contains("exit 2"), "{reason}");
        }
    }

    #[test]
    fn shell_other_nonzero_is_deny() {
        let v = HookVerdict::from_shell(1, "", "");
        assert!(matches!(v, HookVerdict::Deny { .. }), "异常退出应拒绝");
    }

    #[test]
    fn hook_form_classification() {
        let http = HookForm::Http {
            url: "http://x".into(),
            method: "POST".into(),
        };
        assert!(http.needs_network());
        assert_eq!(http.kind_name(), "http");

        let inproc = HookForm::InProcess {
            handler: "builtin".into(),
        };
        assert!(!inproc.needs_network());

        let judge = HookForm::PromptJudge {
            prompt: "p".into(),
            model: None,
        };
        assert!(judge.needs_network());
    }

    #[test]
    fn registry_for_event() {
        let mut reg = HookRegistry::new();
        reg.register(HookDecl {
            name: "guard".into(),
            events: vec![HookKind::PreToolUse, HookKind::BeforeModel],
            form: HookForm::InProcess {
                handler: "guard".into(),
            },
            enabled: true,
            timeout_secs: 10,
        });
        reg.register(HookDecl {
            name: "disabled".into(),
            events: vec![HookKind::PreToolUse],
            form: HookForm::Shell {
                command: "true".into(),
            },
            enabled: false,
            timeout_secs: 10,
        });
        assert_eq!(reg.for_event(HookKind::PreToolUse).len(), 1, "禁用不计");
        assert_eq!(reg.for_event(HookKind::BeforeModel).len(), 1);
        assert!(reg.for_event(HookKind::Stop).is_empty());
    }

    #[test]
    fn registry_counts_by_event() {
        let mut reg = HookRegistry::new();
        reg.register(HookDecl {
            name: "a".into(),
            events: vec![HookKind::Stop],
            form: HookForm::Shell {
                command: "true".into(),
            },
            enabled: true,
            timeout_secs: 5,
        });
        reg.register(HookDecl {
            name: "b".into(),
            events: vec![HookKind::Stop],
            form: HookForm::Shell {
                command: "true".into(),
            },
            enabled: true,
            timeout_secs: 5,
        });
        let counts = reg.counts_by_event();
        assert_eq!(counts["stop"], 2);
    }

    #[test]
    fn merge_deny_wins() {
        let merged = HookRegistry::merge(&[
            HookVerdict::Allow,
            HookVerdict::Inject {
                text: "note".into(),
            },
            HookVerdict::Deny {
                reason: "blocked".into(),
            },
        ]);
        assert!(merged.is_blocking());
        assert_eq!(
            merged,
            HookVerdict::Deny {
                reason: "blocked".into()
            }
        );
    }

    #[test]
    fn merge_injects_concatenated() {
        let merged = HookRegistry::merge(&[
            HookVerdict::Inject { text: "a".into() },
            HookVerdict::Inject { text: "b".into() },
        ]);
        assert_eq!(
            merged,
            HookVerdict::Inject {
                text: "a\nb".into()
            }
        );
    }

    #[test]
    fn merge_modify_last_wins() {
        let merged = HookRegistry::merge(&[
            HookVerdict::Modify {
                content: "first".into(),
            },
            HookVerdict::Modify {
                content: "second".into(),
            },
        ]);
        assert_eq!(
            merged,
            HookVerdict::Modify {
                content: "second".into()
            }
        );
    }

    #[test]
    fn merge_all_allow() {
        assert_eq!(
            HookRegistry::merge(&[HookVerdict::Allow, HookVerdict::Allow]),
            HookVerdict::Allow
        );
        assert_eq!(HookRegistry::merge(&[]), HookVerdict::Allow);
    }

    #[test]
    fn judge_prompt_shape() {
        let prompt = render_judge_prompt("禁止删除生产数据库", "rm -rf /data/prod");
        assert!(prompt.contains("标准"), "{prompt}");
        assert!(prompt.contains("禁止删除生产数据库"));
        assert!(prompt.contains("rm -rf /data/prod"));
        assert!(prompt.contains("`allow` 或 `deny"), "{prompt}");
    }

    #[test]
    fn parse_judge_allow() {
        assert_eq!(parse_judge_output("allow"), HookVerdict::Allow);
        assert_eq!(parse_judge_output("  ALLOW  "), HookVerdict::Allow);
    }

    #[test]
    fn parse_judge_deny_with_reason() {
        let v = parse_judge_output("deny: 会删除生产数据");
        assert_eq!(
            v,
            HookVerdict::Deny {
                reason: "会删除生产数据".into()
            }
        );
    }

    #[test]
    fn parse_judge_deny_without_reason() {
        let v = parse_judge_output("deny");
        assert!(matches!(v, HookVerdict::Deny { .. }));
    }

    #[test]
    fn parse_judge_garbage_fails_closed() {
        let v = parse_judge_output("我不知道");
        assert!(v.is_blocking(), "无法解析应保守拒绝");
        if let HookVerdict::Deny { reason } = v {
            assert!(reason.contains("unparseable"), "{reason}");
        }
    }

    #[test]
    fn hook_decl_serializes() {
        let decl = HookDecl {
            name: "h".into(),
            events: vec![HookKind::BeforeModel],
            form: HookForm::PromptJudge {
                prompt: "p".into(),
                model: Some("small".into()),
            },
            enabled: true,
            timeout_secs: 10,
        };
        let json = serde_json::to_string(&decl).expect("serialize");
        let back: HookDecl = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, decl);
    }
}
