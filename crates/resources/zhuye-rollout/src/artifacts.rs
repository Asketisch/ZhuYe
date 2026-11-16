//! # GenUI 产物系统(F42)
//!
//! agent 直接生成小工具 / 图表 / 页面,嵌在聊天里用。成果即所得。
//!
//! ## 三块
//!
//! 1. **产物注册**([`ArtifactStore`])—— agent 生成的 HTML / 组件按内容
//!    寻址(sha256)存起来,消息里只引用 id。
//! 2. **可见性控制**([`ArtifactVisibility`])—— 产物可能是私有的(含敏感
//!    数据),导出 / 分享时必须显式授权。
//! 3. **沙箱渲染契约**([`RenderPolicy`])—— 产物在**不透明源**(opaque
//!    origin)+ CSP 下渲染,不能访问宿主页面 / cookie / localStorage。
//!
//! ## 为什么内容寻址
//!
//! 同一份产物被多处引用时,内容寻址天然去重;产物更新后 id 变化,
//! 旧的引用不会静默指向新内容(这很重要:用户看到的"刚才那个图表"
//! 不该突然变成另一个)。
//!
//! ## 为什么渲染必须隔离
//!
//! agent 生成的 HTML 可能含任意脚本。在宿主 origin 里渲染 = 给它完整
//! 权限(读 cookie、调宿主 API)。opaque origin(`sandbox` iframe 无
//! `allow-same-origin`)+ CSP 把能力压到"只能画自己"。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 产物类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// HTML 片段(可含内联样式/脚本)。
    Html,
    /// 图表(SVG / canvas 数据)。
    Chart,
    /// 交互小工具(表单 / 按钮组)。
    Widget,
    /// Markdown 增强块。
    Markdown,
    /// 图片(生成或引用)。
    Image,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactKind::Html => "html",
            ArtifactKind::Chart => "chart",
            ArtifactKind::Widget => "widget",
            ArtifactKind::Markdown => "markdown",
            ArtifactKind::Image => "image",
        }
    }

    /// 该类型是否可能含可执行内容(需沙箱)。
    pub fn may_execute(self) -> bool {
        matches!(
            self,
            ArtifactKind::Html | ArtifactKind::Widget | ArtifactKind::Chart
        )
    }
}

/// 产物可见性。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactVisibility {
    /// 仅本会话可见(默认,最安全)。
    #[default]
    Private,
    /// 本机可见(可导出到工作区)。
    Local,
    /// 可分享(显式授权后)。
    Shareable,
}

impl ArtifactVisibility {
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactVisibility::Private => "private",
            ArtifactVisibility::Local => "local",
            ArtifactVisibility::Shareable => "shareable",
        }
    }

    /// 是否允许导出。
    pub fn can_export(self) -> bool {
        !matches!(self, ArtifactVisibility::Private)
    }

    /// 是否允许对外分享。
    pub fn can_share(self) -> bool {
        matches!(self, ArtifactVisibility::Shareable)
    }
}

/// 一个产物。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// 内容寻址 id(`sha256:<hex>`)。
    pub id: String,
    pub kind: ArtifactKind,
    /// 内容(HTML / SVG / markdown 源)。
    pub content: String,
    /// 字节数。
    pub bytes: usize,
    /// 可见性。
    pub visibility: ArtifactVisibility,
    /// 标题(UI 卡片)。
    #[serde(default)]
    pub title: String,
    /// 生成时间。
    pub created_at: u64,
}

impl Artifact {
    /// 是否可安全内联渲染(小体积)。
    pub fn is_small(&self, limit: usize) -> bool {
        self.bytes <= limit
    }

    /// UI 卡片摘要。
    pub fn card_summary(&self) -> String {
        let title = if self.title.is_empty() {
            "(未命名产物)"
        } else {
            self.title.as_str()
        };
        format!("{} · {} · {} 字节", title, self.kind.as_str(), self.bytes)
    }
}

/// 产物存储(内容寻址)。
/// 内容哈希回调(注入,避免本 crate 引加密依赖)。
pub type HashFn = Box<dyn Fn(&str) -> String + Send + Sync>;

pub struct ArtifactStore {
    artifacts: BTreeMap<String, Artifact>,
    /// 内容哈希函数(注入,避免引加密依赖)。
    hash_fn: Option<HashFn>,
    /// 单产物最大字节(超限拒绝,防内存爆炸)。
    max_bytes: usize,
}

impl std::fmt::Debug for ArtifactStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactStore")
            .field("artifacts", &self.artifacts.len())
            .field("has_hasher", &self.hash_fn.is_some())
            .field("max_bytes", &self.max_bytes)
            .finish()
    }
}

impl Default for ArtifactStore {
    fn default() -> Self {
        ArtifactStore::new()
    }
}

impl ArtifactStore {
    pub fn new() -> ArtifactStore {
        ArtifactStore {
            artifacts: BTreeMap::new(),
            hash_fn: None,
            max_bytes: 4 * 1024 * 1024, // 4 MB
        }
    }

    /// 注入哈希函数(通常 `zhuye_sanitize::prompt_guard::fingerprint`)。
    pub fn with_hasher<F>(mut self, f: F) -> ArtifactStore
    where
        F: Fn(&str) -> String + Send + Sync + 'static,
    {
        self.hash_fn = Some(Box::new(f));
        self
    }

    /// 设置最大字节。
    pub fn with_max_bytes(mut self, max: usize) -> ArtifactStore {
        self.max_bytes = max;
        self
    }

    /// 计算内容 id。
    fn content_id(&self, content: &str) -> String {
        match &self.hash_fn {
            Some(f) => format!("sha256:{}", f(content)),
            // 无注入时用长度 + 首尾片段做退化 id(仅测试用)。
            None => {
                let head: String = content.chars().take(16).collect();
                let tail: String = content
                    .chars()
                    .rev()
                    .take(16)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                format!("len{}:{head}…{tail}", content.len())
            }
        }
    }

    /// 注册产物。返回 id。
    ///
    /// 相同内容重复注册返回**同一个 id**(天然去重)。
    pub fn register(
        &mut self,
        kind: ArtifactKind,
        content: impl Into<String>,
        visibility: ArtifactVisibility,
        title: impl Into<String>,
        created_at: u64,
    ) -> Result<String, ArtifactError> {
        let content = content.into();
        let bytes = content.len();
        if bytes > self.max_bytes {
            return Err(ArtifactError::TooLarge {
                bytes,
                max: self.max_bytes,
            });
        }
        if content.trim().is_empty() {
            return Err(ArtifactError::Empty);
        }
        let id = self.content_id(&content);
        if self.artifacts.contains_key(&id) {
            return Ok(id); // 去重
        }
        self.artifacts.insert(
            id.clone(),
            Artifact {
                id: id.clone(),
                kind,
                content,
                bytes,
                visibility,
                title: title.into(),
                created_at,
            },
        );
        Ok(id)
    }

    /// 查产物。
    pub fn get(&self, id: &str) -> Option<&Artifact> {
        self.artifacts.get(id)
    }

    /// 修改可见性。
    pub fn set_visibility(
        &mut self,
        id: &str,
        visibility: ArtifactVisibility,
    ) -> Result<(), ArtifactError> {
        let artifact = self
            .artifacts
            .get_mut(id)
            .ok_or_else(|| ArtifactError::Unknown(id.to_string()))?;
        artifact.visibility = visibility;
        Ok(())
    }

    /// 删除(引用方应自行处理悬空引用)。
    pub fn remove(&mut self, id: &str) -> bool {
        self.artifacts.remove(id).is_some()
    }

    /// 全部产物(按创建时间)。
    pub fn all(&self) -> Vec<&Artifact> {
        let mut out: Vec<&Artifact> = self.artifacts.values().collect();
        out.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        out
    }

    /// 可导出的产物。
    pub fn exportable(&self) -> Vec<&Artifact> {
        self.artifacts
            .values()
            .filter(|a| a.visibility.can_export())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.artifacts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.artifacts.is_empty()
    }

    /// 总占用字节。
    pub fn total_bytes(&self) -> usize {
        self.artifacts.values().map(|a| a.bytes).sum()
    }
}

/// 产物错误。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ArtifactError {
    #[error("artifact too large: {bytes} > {max} bytes")]
    TooLarge { bytes: usize, max: usize },
    #[error("artifact content is empty")]
    Empty,
    #[error("unknown artifact: {0}")]
    Unknown(String),
}

/// 渲染策略(沙箱契约)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderPolicy {
    /// 是否允许脚本(即使允许也只在沙箱内)。
    pub allow_scripts: bool,
    /// 是否允许网络请求(默认禁)。
    pub allow_network: bool,
    /// CSP 指令(最小集)。
    pub csp: Vec<String>,
    /// 是否要求不透明源(禁 same-origin)。
    pub opaque_origin: bool,
}

impl Default for RenderPolicy {
    fn default() -> Self {
        RenderPolicy {
            allow_scripts: true,
            allow_network: false,
            csp: vec![
                "default-src 'none'".to_string(),
                "style-src 'unsafe-inline'".to_string(),
                "img-src data: blob:".to_string(),
                "script-src 'unsafe-inline'".to_string(),
                "connect-src 'none'".to_string(),
            ],
            opaque_origin: true,
        }
    }
}

impl RenderPolicy {
    /// 无脚本的严格策略(纯静态 HTML)。
    pub fn strict() -> RenderPolicy {
        RenderPolicy {
            allow_scripts: false,
            allow_network: false,
            csp: vec![
                "default-src 'none'".to_string(),
                "style-src 'unsafe-inline'".to_string(),
                "img-src data:".to_string(),
            ],
            opaque_origin: true,
        }
    }

    /// 为某产物生成策略。
    pub fn for_artifact(kind: ArtifactKind) -> RenderPolicy {
        if kind.may_execute() {
            RenderPolicy::default()
        } else {
            RenderPolicy::strict()
        }
    }

    /// 渲染成 `Content-Security-Policy` 头值。
    pub fn csp_header(&self) -> String {
        self.csp.join("; ")
    }

    /// 是否安全(不透明源 + 无网络)。
    pub fn is_safe(&self) -> bool {
        self.opaque_origin && !self.allow_network
    }

    /// 校验产物内容是否违反策略(简单检查)。
    ///
    /// 检出 `<script>` 但策略禁脚本 → 拒绝。
    pub fn validate_content(&self, content: &str) -> Result<(), String> {
        if !self.allow_scripts && content.to_lowercase().contains("<script") {
            return Err("产物含 <script>,但渲染策略禁止脚本".to_string());
        }
        if !self.allow_network && (content.contains("fetch(") || content.contains("XMLHttpRequest"))
        {
            return Err("产物含网络请求,但渲染策略禁止网络".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> ArtifactStore {
        ArtifactStore::new()
    }

    #[test]
    fn register_and_get() {
        let mut s = store();
        let id = s
            .register(
                ArtifactKind::Html,
                "<div>hi</div>",
                ArtifactVisibility::Private,
                "Greeting",
                100,
            )
            .expect("register");
        let a = s.get(&id).expect("artifact");
        assert_eq!(a.kind, ArtifactKind::Html);
        assert_eq!(a.title, "Greeting");
        assert_eq!(a.visibility, ArtifactVisibility::Private);
    }

    #[test]
    fn content_addressing_dedups() {
        let mut s = store();
        let id1 = s
            .register(
                ArtifactKind::Html,
                "<div>x</div>",
                ArtifactVisibility::Private,
                "",
                1,
            )
            .unwrap();
        let id2 = s
            .register(
                ArtifactKind::Html,
                "<div>x</div>",
                ArtifactVisibility::Local,
                "",
                2,
            )
            .unwrap();
        assert_eq!(id1, id2, "相同内容应去重");
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn different_content_different_id() {
        let mut s = store();
        let id1 = s
            .register(ArtifactKind::Html, "a", ArtifactVisibility::Private, "", 1)
            .unwrap();
        let id2 = s
            .register(ArtifactKind::Html, "b", ArtifactVisibility::Private, "", 2)
            .unwrap();
        assert_ne!(id1, id2);
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn empty_content_rejected() {
        let mut s = store();
        assert_eq!(
            s.register(
                ArtifactKind::Html,
                "   ",
                ArtifactVisibility::Private,
                "",
                1
            ),
            Err(ArtifactError::Empty)
        );
    }

    #[test]
    fn oversize_rejected() {
        let mut s = store().with_max_bytes(10);
        let big = "x".repeat(100);
        assert!(matches!(
            s.register(ArtifactKind::Html, big, ArtifactVisibility::Private, "", 1),
            Err(ArtifactError::TooLarge { .. })
        ));
    }

    #[test]
    fn visibility_export_rules() {
        assert!(!ArtifactVisibility::Private.can_export());
        assert!(ArtifactVisibility::Local.can_export());
        assert!(ArtifactVisibility::Shareable.can_export());
        assert!(!ArtifactVisibility::Local.can_share());
        assert!(ArtifactVisibility::Shareable.can_share());
    }

    #[test]
    fn set_visibility() {
        let mut s = store();
        let id = s
            .register(ArtifactKind::Html, "x", ArtifactVisibility::Private, "", 1)
            .unwrap();
        s.set_visibility(&id, ArtifactVisibility::Shareable)
            .unwrap();
        assert!(s.get(&id).unwrap().visibility.can_share());
        assert!(
            s.set_visibility("ghost", ArtifactVisibility::Local)
                .is_err()
        );
    }

    #[test]
    fn exportable_filter() {
        let mut s = store();
        s.register(ArtifactKind::Html, "a", ArtifactVisibility::Private, "", 1)
            .unwrap();
        s.register(ArtifactKind::Html, "b", ArtifactVisibility::Local, "", 2)
            .unwrap();
        s.register(
            ArtifactKind::Chart,
            "c",
            ArtifactVisibility::Shareable,
            "",
            3,
        )
        .unwrap();
        assert_eq!(s.exportable().len(), 2, "private 不可导出");
    }

    #[test]
    fn remove_artifact() {
        let mut s = store();
        let id = s
            .register(ArtifactKind::Html, "x", ArtifactVisibility::Private, "", 1)
            .unwrap();
        assert!(s.remove(&id));
        assert!(!s.remove(&id));
        assert!(s.is_empty());
    }

    #[test]
    fn total_bytes() {
        let mut s = store();
        s.register(
            ArtifactKind::Html,
            "12345",
            ArtifactVisibility::Private,
            "",
            1,
        )
        .unwrap();
        s.register(
            ArtifactKind::Html,
            "678",
            ArtifactVisibility::Private,
            "",
            2,
        )
        .unwrap();
        assert_eq!(s.total_bytes(), 8);
    }

    #[test]
    fn all_sorted_by_created() {
        let mut s = store();
        s.register(
            ArtifactKind::Html,
            "late",
            ArtifactVisibility::Private,
            "",
            200,
        )
        .unwrap();
        s.register(
            ArtifactKind::Html,
            "early",
            ArtifactVisibility::Private,
            "",
            100,
        )
        .unwrap();
        let all = s.all();
        assert_eq!(all[0].content, "early");
    }

    #[test]
    fn artifact_kind_execution() {
        assert!(ArtifactKind::Html.may_execute());
        assert!(ArtifactKind::Widget.may_execute());
        assert!(ArtifactKind::Chart.may_execute());
        assert!(!ArtifactKind::Markdown.may_execute());
        assert!(!ArtifactKind::Image.may_execute());
    }

    #[test]
    fn render_policy_default_is_safe() {
        let p = RenderPolicy::default();
        assert!(p.is_safe(), "默认应不透明源 + 无网络");
        assert!(p.opaque_origin);
        assert!(!p.allow_network);
        let header = p.csp_header();
        assert!(header.contains("default-src 'none'"), "{header}");
        assert!(header.contains("connect-src 'none'"), "{header}");
    }

    #[test]
    fn strict_policy_blocks_scripts() {
        let p = RenderPolicy::strict();
        assert!(!p.allow_scripts);
        assert!(p.validate_content("<script>evil()</script>").is_err());
        assert!(p.validate_content("<div>safe</div>").is_ok());
    }

    #[test]
    fn policy_blocks_network_calls() {
        let p = RenderPolicy::default();
        assert!(p.validate_content("<div>fetch('http://x')</div>").is_err());
        assert!(p.validate_content("<div>XMLHttpRequest</div>").is_err());
        assert!(p.validate_content("<div>static</div>").is_ok());
    }

    #[test]
    fn policy_for_artifact_kind() {
        let html = RenderPolicy::for_artifact(ArtifactKind::Html);
        assert!(html.allow_scripts, "HTML 允许脚本(沙箱内)");
        let md = RenderPolicy::for_artifact(ArtifactKind::Markdown);
        assert!(!md.allow_scripts, "Markdown 不需要脚本");
    }

    #[test]
    fn card_summary() {
        let a = Artifact {
            id: "sha256:abc".into(),
            kind: ArtifactKind::Chart,
            content: "svg".into(),
            bytes: 3,
            visibility: ArtifactVisibility::Private,
            title: "Sales".into(),
            created_at: 1,
        };
        let summary = a.card_summary();
        assert!(summary.contains("Sales"), "{summary}");
        assert!(summary.contains("chart"), "{summary}");
    }

    #[test]
    fn artifact_serializes() {
        let a = Artifact {
            id: "id".into(),
            kind: ArtifactKind::Html,
            content: "c".into(),
            bytes: 1,
            visibility: ArtifactVisibility::Local,
            title: "t".into(),
            created_at: 5,
        };
        let json = serde_json::to_string(&a).expect("serialize");
        let back: Artifact = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, a);
    }
}
