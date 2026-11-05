//! `resolver` —— `PermissionResolver` trait + 内存 + store-backed 实现。
//!
//! resolver 是 ApprovalGate 的依赖:gate 在 `ask_tool` 短路 Allow / Deny,
//! 通过 resolver 查"这个 tool 当前有没有显式规则"。

use std::sync::Arc;

use crate::policy::PolicyEngine;
use crate::rules::RuleMatch;
use crate::store::PermissionStore;

/// Resolver trait。`async` 简化未来扩展(network / db 后端)。
///
/// 设计取舍:`async fn` 而不是 `fn(&self) -> RuleMatch` —— 即使当前
/// 两个 impl(File + InMemory)都是 sync,async 让 store 端可以走
/// `tokio::fs` 而不阻塞 reactor。代价是 caller 多一层 `.await`,但
/// `ask_tool` 本来就是 async 函数,无影响。
///
/// 两个方法:
/// - [`resolve`](Self::resolve):无上下文,只按工具名匹配。用于无 bash
///   命令可参考的路径(如 `ask_user_opts`)。default 实现转发到
///   [`resolve_with_context`](Self::resolve_with_context) 传 `None`。
/// - [`resolve_with_context`](Self::resolve_with_context):带 bash 命令
///   上下文,让 `shell_pattern` 规则(如 `Bash: git *`)能命中。**有 bash
///   命令的调用点(如 `ApprovalGate::ask_tool`)应优先调它**,否则
///   `shell_pattern` 规则静默失效。
#[async_trait::async_trait]
pub trait PermissionResolver: Send + Sync {
    /// 无 bash 上下文的解析(default 实现):转发到 `resolve_with_context`
    /// 传 `None`。仅当 caller 确实没有 bash 命令时用(如非 Bash 工具、或
    /// ask_user 路径)。
    async fn resolve(&self, tool_name: &str) -> RuleMatch {
        self.resolve_with_context(tool_name, None).await
    }

    /// 带 bash 命令上下文的解析。`bash_command = Some(cmd)` 时,
    /// `shell_pattern` 规则(仅 `Bash`/`bash` 工具)参与匹配;`None` 时
    /// 跳过 shell 规则(与旧 `resolve` 行为一致)。
    async fn resolve_with_context(&self, tool_name: &str, bash_command: Option<&str>) -> RuleMatch;

    /// 带资源上下文的解析;已有自定义实现可继续使用原有接口。
    async fn resolve_with_resource(
        &self,
        tool_name: &str,
        bash_command: Option<&str>,
        _resource: Option<&str>,
    ) -> RuleMatch {
        self.resolve_with_context(tool_name, bash_command).await
    }

    /// 多个等价资源表示;旧实现按最严格结果合并,存储实现保持规则顺序。
    async fn resolve_with_resources(
        &self,
        tool_name: &str,
        bash_command: Option<&str>,
        resources: &[&str],
    ) -> RuleMatch {
        if resources.is_empty() {
            return self
                .resolve_with_resource(tool_name, bash_command, None)
                .await;
        }
        let mut result = RuleMatch::NoMatch;
        for resource in resources {
            match self
                .resolve_with_resource(tool_name, bash_command, Some(resource))
                .await
            {
                RuleMatch::Deny => return RuleMatch::Deny,
                RuleMatch::Ask => result = RuleMatch::Ask,
                RuleMatch::Allow if result == RuleMatch::NoMatch => result = RuleMatch::Allow,
                _ => {}
            }
        }
        result
    }
}

// ── StorePermissionResolver ─────────────────────────────────────────────

/// 把 `PermissionStore` 包成 resolver。权限规则读取或解析失败时拒绝工具，
/// 避免损坏的文件使整条规则链中的拒绝规则静默失效。
///
/// 存储错误通过 `tracing::warn!` 留下排查痕迹。
pub struct StorePermissionResolver {
    store: Arc<dyn PermissionStore>,
    /// v1.9(1.5):v2 策略引擎。默认配置 = v1 兼容(DenyWins +
    /// Fallthrough)+ 两项安全底线:风险标签硬拒(凭据外传/提权)与
    /// 控制面自保护(改权限/配置的工具不可被规则自动放行)。
    engine: PolicyEngine,
}

// 手写 Debug:`Arc<dyn PermissionStore>` 没有 derive Debug。
impl std::fmt::Debug for StorePermissionResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorePermissionResolver")
            .field("store", &"<dyn PermissionStore>")
            .field("engine", &self.engine)
            .finish()
    }
}

impl StorePermissionResolver {
    pub fn new(store: Arc<dyn PermissionStore>) -> Self {
        Self {
            store,
            engine: PolicyEngine::default(),
        }
    }

    /// v1.9(1.5):显式指定策略引擎(嵌入方自定义 precedence /
    /// 默认全拒 / 无人值守模式等)。
    pub fn with_engine(store: Arc<dyn PermissionStore>, engine: PolicyEngine) -> Self {
        Self { store, engine }
    }

    /// 策略引擎只读访问(调用方查 Decision reason)。
    pub fn engine(&self) -> &PolicyEngine {
        &self.engine
    }
}

#[async_trait::async_trait]
impl PermissionResolver for StorePermissionResolver {
    async fn resolve_with_context(&self, tool_name: &str, bash_command: Option<&str>) -> RuleMatch {
        self.resolve_with_resource(tool_name, bash_command, None)
            .await
    }

    async fn resolve_with_resource(
        &self,
        tool_name: &str,
        bash_command: Option<&str>,
        resource: Option<&str>,
    ) -> RuleMatch {
        self.resolve_with_resources(tool_name, bash_command, resource.as_slice())
            .await
    }

    async fn resolve_with_resources(
        &self,
        tool_name: &str,
        bash_command: Option<&str>,
        resources: &[&str],
    ) -> RuleMatch {
        let rules = match self.store.list().await {
            Ok(rules) => rules,
            Err(e) => {
                tracing::warn!(error = %e, tool = %tool_name, "权限规则读取失败，拒绝工具调用");
                return RuleMatch::Deny;
            }
        };
        self.engine
            .evaluate_resources(&rules, tool_name, bash_command, resources)
            .action
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{PermissionAction, PermissionRule};
    use crate::store::InMemoryPermissionStore;

    #[tokio::test]
    async fn corrupt_permission_file_denies_until_repaired() {
        use crate::store::FilePermissionStore;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.toml");
        std::fs::write(&path, "[[rule\n").unwrap();
        let resolver =
            StorePermissionResolver::new(Arc::new(FilePermissionStore::with_path(path.clone())));
        assert_eq!(
            resolver.resolve_with_context("echo", None).await,
            RuleMatch::Deny
        );
        std::fs::write(&path, "").unwrap();
        assert_eq!(
            resolver.resolve_with_context("echo", None).await,
            RuleMatch::NoMatch
        );
    }

    #[tokio::test]
    async fn configured_policy_and_resource_are_applied() {
        use crate::policy::{PolicyConfig, Precedence, UnspecifiedDefault};
        let store = Arc::new(InMemoryPermissionStore::new());
        for action in [PermissionAction::Deny, PermissionAction::Allow] {
            store
                .add(PermissionRule {
                    tool: "read".into(),
                    action,
                    tool_glob: None,
                    shell_pattern: None,
                    resource_glob: Some("/safe/**".into()),
                })
                .await
                .unwrap();
        }
        let resolver = StorePermissionResolver::with_engine(
            store,
            PolicyEngine::new(PolicyConfig {
                precedence: Precedence::LastMatch,
                unspecified_default: UnspecifiedDefault::Deny,
                deny_labels: Vec::new(),
                self_protection: false,
                unattended: false,
            }),
        );
        assert_eq!(
            resolver
                .resolve_with_resource("read", None, Some("/safe/file"))
                .await,
            RuleMatch::Allow
        );
        assert_eq!(
            resolver
                .resolve_with_resource("read", None, Some("/other/file"))
                .await,
            RuleMatch::Deny
        );
    }

    #[tokio::test]
    async fn default_policy_enforces_risk_labels() {
        let resolver = StorePermissionResolver::new(Arc::new(InMemoryPermissionStore::new()));
        assert_eq!(
            resolver.resolve_with_context("bash", Some("sudo id")).await,
            RuleMatch::Deny
        );
    }

    #[tokio::test]
    async fn resolver_returns_allow_when_rule_present() {
        let s = Arc::new(InMemoryPermissionStore::new());
        s.add(PermissionRule {
            tool: "Bash".into(),
            action: PermissionAction::Allow,
            tool_glob: None,
            shell_pattern: None,
            resource_glob: None,
        })
        .await
        .unwrap();
        let r = StorePermissionResolver::new(s);
        assert_eq!(r.resolve("Bash").await, RuleMatch::Allow);
    }

    #[tokio::test]
    async fn resolver_returns_deny_when_rule_present() {
        let s = Arc::new(InMemoryPermissionStore::new());
        s.add(PermissionRule {
            tool: "Write".into(),
            action: PermissionAction::Deny,
            tool_glob: None,
            shell_pattern: None,
            resource_glob: None,
        })
        .await
        .unwrap();
        let r = StorePermissionResolver::new(s);
        assert_eq!(r.resolve("Write").await, RuleMatch::Deny);
    }

    #[tokio::test]
    async fn resolver_returns_no_match_for_unknown_tool() {
        let s = Arc::new(InMemoryPermissionStore::new());
        let r = StorePermissionResolver::new(s);
        assert_eq!(r.resolve("Bash").await, RuleMatch::NoMatch);
    }

    #[tokio::test]
    async fn resolve_with_context_matches_shell_pattern() {
        // shell_pattern 规则在 bash_command 传入时应命中(这是此前 bug 的回归测试:
        // 旧 resolve 只调 evaluate 无 bash 上下文,shell_pattern 静默失效)。
        let s = Arc::new(InMemoryPermissionStore::new());
        s.add(PermissionRule {
            tool: "Bash".into(),
            action: PermissionAction::Allow,
            tool_glob: None,
            shell_pattern: Some("git *".into()),
            resource_glob: None,
        })
        .await
        .unwrap();
        let r = StorePermissionResolver::new(s);
        // 传匹配的 bash 命令 → Allow。
        assert_eq!(
            r.resolve_with_context("Bash", Some("git status")).await,
            RuleMatch::Allow,
            "shell_pattern='git *' 应匹配 'git status'"
        );
        // 不匹配的命令 → NoMatch(回退默认审批)。
        assert_eq!(
            r.resolve_with_context("Bash", Some("rm -rf /")).await,
            RuleMatch::NoMatch,
            "shell_pattern='git *' 不应匹配 'rm -rf /'"
        );
        // 无 bash 上下文 → **Allow** 类 shell_pattern 规则不参与(它需要
        // 看到命令才能判断是否放行)→ NoMatch。
        assert_eq!(
            r.resolve_with_context("Bash", None).await,
            RuleMatch::NoMatch,
            "无 bash 上下文时 Allow 类 shell_pattern 规则不参与"
        );
    }

    /// 回归(0.4):**Deny** 类 shell_pattern 规则在无 bash 上下文时必须
    /// fail-closed 返回 Deny,不能被静默跳过。
    ///
    /// 修复前 `match_rule` 一开头就是 `let cmd = bash_command?;`,于是
    /// `{tool="Bash", shell_pattern="rm -rf *", action="deny"}` 在任何
    /// 忘记传命令的调用点(或未来新增的调用点)上完全失效。
    #[tokio::test]
    async fn resolve_deny_shell_pattern_fails_closed_without_context() {
        let s = Arc::new(InMemoryPermissionStore::new());
        s.add(PermissionRule {
            tool: "Bash".into(),
            action: PermissionAction::Deny,
            tool_glob: None,
            shell_pattern: Some("rm -rf *".into()),
            resource_glob: None,
        })
        .await
        .unwrap();
        let r = StorePermissionResolver::new(s);
        assert_eq!(
            r.resolve_with_context("Bash", None).await,
            RuleMatch::Deny,
            "Deny shell_pattern 规则在无命令上下文时必须 fail-closed"
        );
        // 有上下文且命令命中 → 仍 Deny。
        assert_eq!(
            r.resolve_with_context("Bash", Some("rm -rf /tmp/x")).await,
            RuleMatch::Deny
        );
    }

    /// 回归(0.4):Deny shell_pattern 必须拦住复合命令里的附加段。
    /// 修复前整条命令只做一次 glob,`git status; rm -rf /` 作为单个字符串
    /// 匹配不上 `rm -rf *`(因为开头不是 rm),于是漏过。
    #[tokio::test]
    async fn deny_shell_pattern_matches_every_segment() {
        let s = Arc::new(InMemoryPermissionStore::new());
        s.add(PermissionRule {
            tool: "Bash".into(),
            action: PermissionAction::Deny,
            tool_glob: None,
            shell_pattern: Some("rm -rf *".into()),
            resource_glob: None,
        })
        .await
        .unwrap();
        let r = StorePermissionResolver::new(s);
        for cmd in [
            "rm -rf /tmp/x",
            "git status; rm -rf /tmp/x",
            "echo hi && rm -rf /tmp/x",
            "cat f | rm -rf /tmp/x",
        ] {
            assert_eq!(
                r.resolve_with_context("Bash", Some(cmd)).await,
                RuleMatch::Deny,
                "复合命令中的危险段必须被 deny: {cmd}"
            );
        }
    }

    #[tokio::test]
    async fn resolve_default_forwards_none_context() {
        // 默认 resolve() 应转发 resolve_with_context(_, None),
        // 故 shell_pattern 规则不参与(与上面 None 用例一致)。
        let s = Arc::new(InMemoryPermissionStore::new());
        s.add(PermissionRule {
            tool: "Bash".into(),
            action: PermissionAction::Allow,
            tool_glob: None,
            shell_pattern: Some("git *".into()),
            resource_glob: None,
        })
        .await
        .unwrap();
        let r = StorePermissionResolver::new(s);
        assert_eq!(r.resolve("Bash").await, RuleMatch::NoMatch);
    }
}
