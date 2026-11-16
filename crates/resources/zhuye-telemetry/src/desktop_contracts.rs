//! # 渲染与桌面体验契约(F51 / F52 / F54 / F57)
//!
//! 这四项的主体在 Desktop / CLI 仓库,但**引擎侧需要提供契约**:
//!
//! - **F51 渲染套件** —— [`RenderableBlock`] 声明"这段内容该用什么渲染器"
//!   (mermaid / math / diff / image),前端据此选组件。没有契约,前端只能
//!   靠正则猜。
//! - **F52 桌面体验** —— [`ContextGauge`] 上下文用量仪表 + [`ScmChange`]
//!   会话内 SCM 面板数据。
//! - **F54 观测长尾** —— [`TraceSpan`] 轨迹回放 + [`StepSummary`] 人话摘要。
//! - **F57 更新与常驻** —— [`UpdateManifest`] 签名校验 + [`ServiceUnit`]
//!   系统服务单元生成。
//!
//! ## 为什么契约放引擎
//!
//! 前端要渲染"这是 mermaid 图",前提是**引擎告诉它**。让前端自己正则
//! 判断语言标记会漂移(不同模型输出的围栏写法不一)。引擎在生成内容时
//! 就知道类型,直接标注最可靠。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ── F51:渲染块契约 ───────────────────────────────────────────────────────

/// 渲染器类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Renderer {
    /// 普通 Markdown。
    Markdown,
    /// Mermaid 图。
    Mermaid,
    /// 数学公式(LaTeX)。
    Math,
    /// 代码 diff。
    Diff,
    /// 图片。
    Image,
    /// 波形图(Wavedrom)。
    Waveform,
    /// 表格。
    Table,
}

impl Renderer {
    pub fn as_str(self) -> &'static str {
        match self {
            Renderer::Markdown => "markdown",
            Renderer::Mermaid => "mermaid",
            Renderer::Math => "math",
            Renderer::Diff => "diff",
            Renderer::Image => "image",
            Renderer::Waveform => "waveform",
            Renderer::Table => "table",
        }
    }

    /// 是否需要沙箱渲染(可执行 / 可注入)。
    pub fn needs_sandbox(self) -> bool {
        matches!(self, Renderer::Mermaid | Renderer::Waveform)
    }

    /// 是否支持缩放浮层(灯箱)。
    pub fn supports_zoom(self) -> bool {
        matches!(self, Renderer::Image | Renderer::Mermaid | Renderer::Math)
    }
}

/// 可渲染块。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderableBlock {
    pub renderer: Renderer,
    /// 内容(源码 / 文本 / 图片 id)。
    pub content: String,
    /// 内容指纹(前端缓存 key)。
    #[serde(default)]
    pub fingerprint: Option<String>,
    /// 锚点 id(长会话导航用)。
    #[serde(default)]
    pub anchor: Option<String>,
}

impl RenderableBlock {
    pub fn new(renderer: Renderer, content: impl Into<String>) -> RenderableBlock {
        RenderableBlock {
            renderer,
            content: content.into(),
            fingerprint: None,
            anchor: None,
        }
    }

    /// 从 markdown 围栏语言标记推断渲染器。
    ///
    /// 这是**兜底路径**:引擎能直接标注时应直接标注。
    pub fn from_fence_language(lang: &str, content: impl Into<String>) -> RenderableBlock {
        let renderer = match lang.trim().to_ascii_lowercase().as_str() {
            "mermaid" => Renderer::Mermaid,
            "math" | "latex" | "katex" => Renderer::Math,
            "diff" | "patch" => Renderer::Diff,
            "wavedrom" | "wave" => Renderer::Waveform,
            _ => Renderer::Markdown,
        };
        RenderableBlock::new(renderer, content)
    }

    /// 设置锚点。
    pub fn with_anchor(mut self, anchor: impl Into<String>) -> RenderableBlock {
        self.anchor = Some(anchor.into());
        self
    }

    /// 内容是否为空。
    pub fn is_empty(&self) -> bool {
        self.content.trim().is_empty()
    }
}

/// 渲染套件能力(前端声明支持哪些渲染器)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RendererSupport {
    pub supported: Vec<Renderer>,
}

impl RendererSupport {
    pub fn new() -> RendererSupport {
        RendererSupport::default()
    }

    pub fn with(mut self, renderer: Renderer) -> RendererSupport {
        if !self.supported.contains(&renderer) {
            self.supported.push(renderer);
        }
        self
    }

    pub fn supports(&self, renderer: Renderer) -> bool {
        self.supported.contains(&renderer)
    }

    /// 降级:不支持的渲染器回落到 Markdown。
    pub fn fallback_for(&self, renderer: Renderer) -> Renderer {
        if self.supports(renderer) {
            renderer
        } else {
            Renderer::Markdown
        }
    }
}

// ── F52:桌面体验 ─────────────────────────────────────────────────────────

/// 上下文用量仪表。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ContextGauge {
    /// 已用 token。
    pub used_tokens: u64,
    /// 窗口大小。
    pub window_tokens: u64,
}

impl ContextGauge {
    pub fn new(used_tokens: u64, window_tokens: u64) -> ContextGauge {
        ContextGauge {
            used_tokens,
            window_tokens,
        }
    }

    /// 使用率(0.0–1.0)。
    pub fn ratio(&self) -> f64 {
        if self.window_tokens == 0 {
            return 0.0;
        }
        (self.used_tokens as f64 / self.window_tokens as f64).clamp(0.0, 1.0)
    }

    /// 告警档位。
    pub fn level(&self) -> GaugeLevel {
        let r = self.ratio();
        if r >= 0.9 {
            GaugeLevel::Critical
        } else if r >= 0.75 {
            GaugeLevel::Warning
        } else {
            GaugeLevel::Normal
        }
    }

    /// 剩余 token。
    pub fn remaining(&self) -> u64 {
        self.window_tokens.saturating_sub(self.used_tokens)
    }

    /// 环形仪表文案。
    pub fn label(&self) -> String {
        format!(
            "{:.0}% ({}/{})",
            self.ratio() * 100.0,
            self.used_tokens,
            self.window_tokens
        )
    }
}

/// 仪表档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GaugeLevel {
    Normal,
    Warning,
    Critical,
}

/// 会话内 SCM 变更(消息 → 文件)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmChange {
    /// 归属的 turn id。
    pub turn_id: String,
    pub path: String,
    /// 改动行数(增 + 删)。
    pub lines_changed: usize,
    /// 变更类型。
    pub kind: String,
}

impl ScmChange {
    /// 人读一行。
    pub fn render(&self) -> String {
        format!("{} {} ({} 行)", self.kind, self.path, self.lines_changed)
    }
}

/// 会话 SCM 面板。
#[derive(Debug, Default)]
pub struct ScmPanel {
    changes: Vec<ScmChange>,
}

impl ScmPanel {
    pub fn new() -> ScmPanel {
        ScmPanel::default()
    }

    pub fn push(&mut self, change: ScmChange) {
        self.changes.push(change);
    }

    /// 某回合的改动。
    pub fn for_turn(&self, turn_id: &str) -> Vec<&ScmChange> {
        self.changes
            .iter()
            .filter(|c| c.turn_id == turn_id)
            .collect()
    }

    /// 全部改动(去重按 path 汇总行数)。
    pub fn summary(&self) -> BTreeMap<&str, usize> {
        let mut out = BTreeMap::new();
        for c in &self.changes {
            *out.entry(c.path.as_str()).or_insert(0) += c.lines_changed;
        }
        out
    }

    pub fn len(&self) -> usize {
        self.changes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

// ── F54:观测 ─────────────────────────────────────────────────────────────

/// 一个 trace span。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceSpan {
    pub span_id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    /// 操作名(`model_call` / `tool:bash` / `compact`).
    pub name: String,
    /// 开始时间(毫秒)。
    pub start_ms: u64,
    /// 时长(毫秒)。
    pub duration_ms: u64,
    /// 属性(模型 / 工具 / token 数)。
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
}

impl TraceSpan {
    /// 结束时间。
    pub fn end_ms(&self) -> u64 {
        self.start_ms + self.duration_ms
    }

    /// 是否为某 span 的子 span。
    pub fn is_child_of(&self, parent: &str) -> bool {
        self.parent_id.as_deref() == Some(parent)
    }
}

/// 轨迹(可回放)。
#[derive(Debug, Default)]
pub struct Trace {
    spans: Vec<TraceSpan>,
}

impl Trace {
    pub fn new() -> Trace {
        Trace::default()
    }

    pub fn push(&mut self, span: TraceSpan) {
        self.spans.push(span);
    }

    /// 按开始时间排序的 span。
    pub fn ordered(&self) -> Vec<&TraceSpan> {
        let mut out: Vec<&TraceSpan> = self.spans.iter().collect();
        out.sort_by_key(|s| s.start_ms);
        out
    }

    /// 根 span(无 parent)。
    pub fn roots(&self) -> Vec<&TraceSpan> {
        self.spans
            .iter()
            .filter(|s| s.parent_id.is_none())
            .collect()
    }

    /// 某 span 的子 span。
    pub fn children_of(&self, parent: &str) -> Vec<&TraceSpan> {
        self.spans
            .iter()
            .filter(|s| s.is_child_of(parent))
            .collect()
    }

    /// 总耗时(根 span 的结束 - 开始)。
    pub fn total_duration_ms(&self) -> u64 {
        let roots = self.roots();
        if roots.is_empty() {
            return 0;
        }
        let start = roots.iter().map(|s| s.start_ms).min().unwrap_or(0);
        let end = roots.iter().map(|s| s.end_ms()).max().unwrap_or(0);
        end.saturating_sub(start)
    }

    /// 按操作名聚合耗时(`name -> 总毫秒`),降序。
    pub fn duration_by_name(&self) -> Vec<(String, u64)> {
        let mut map: BTreeMap<String, u64> = BTreeMap::new();
        for s in &self.spans {
            *map.entry(s.name.clone()).or_insert(0) += s.duration_ms;
        }
        let mut out: Vec<(String, u64)> = map.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}

/// 人话步骤摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepSummary {
    /// 步骤序号。
    pub step: usize,
    /// 人话描述("读取了 src/lib.rs" / "运行了 cargo test").
    pub description: String,
    /// 结果(`ok` / `error` / `skipped`).
    pub outcome: String,
}

impl StepSummary {
    /// 渲染一行。
    pub fn render(&self) -> String {
        format!("{}. {} [{}]", self.step, self.description, self.outcome)
    }
}

/// 从 trace span 生成人话摘要(简化规则)。
pub fn summarize_spans(trace: &Trace) -> Vec<StepSummary> {
    trace
        .ordered()
        .into_iter()
        .enumerate()
        .map(|(idx, span)| {
            let description = match span.name.as_str() {
                name if name.starts_with("tool:") => {
                    format!("调用了工具 {}", &name[5..])
                }
                "model_call" => "调用了模型".to_string(),
                "compact" => "压缩了上下文".to_string(),
                other => other.to_string(),
            };
            let outcome = span
                .attributes
                .get("outcome")
                .cloned()
                .unwrap_or_else(|| "ok".to_string());
            StepSummary {
                step: idx + 1,
                description,
                outcome,
            }
        })
        .collect()
}

// ── F57:更新与常驻 ───────────────────────────────────────────────────────

/// 更新清单。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateManifest {
    /// 版本号。
    pub version: String,
    /// 平台(`darwin-x64` / `linux-arm64` / `windows-x64`).
    pub platform: String,
    /// 下载 URL。
    pub url: String,
    /// 包 sha256。
    pub sha256: String,
    /// 签名(base64;sigstore / minisign)。
    pub signature: String,
    /// 是否允许回滚到上一版。
    #[serde(default = "default_true")]
    pub rollback_available: bool,
}

fn default_true() -> bool {
    true
}

impl UpdateManifest {
    /// 校验(必填字段非空)。
    pub fn validate(&self) -> Result<(), String> {
        if self.version.trim().is_empty() {
            return Err("版本号为空".to_string());
        }
        if self.sha256.trim().is_empty() {
            return Err("缺少 sha256".to_string());
        }
        if self.signature.trim().is_empty() {
            return Err("缺少签名 —— 无签名的更新包不可信".to_string());
        }
        Ok(())
    }

    /// 是否比当前版本新(极简 semver 比较)。
    pub fn is_newer_than(&self, current: &str) -> bool {
        parse_version(&self.version) > parse_version(current)
    }

    /// 人读摘要。
    pub fn summary(&self) -> String {
        format!("{} ({})", self.version, self.platform)
    }
}

/// 解析 `1.2.3` → `(1,2,3)`;无法解析返回 `(0,0,0)`。
fn parse_version(v: &str) -> (u64, u64, u64) {
    let parts: Vec<u64> = v
        .trim_start_matches('v')
        .split('.')
        .take(3)
        .map(|p| p.split('-').next().unwrap_or("0").parse().unwrap_or(0))
        .collect();
    (
        parts.first().copied().unwrap_or(0),
        parts.get(1).copied().unwrap_or(0),
        parts.get(2).copied().unwrap_or(0),
    )
}

/// 系统服务类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceManager {
    /// macOS launchd。
    Launchd,
    /// Linux systemd。
    Systemd,
    /// Windows 服务。
    WindowsService,
}

/// 服务单元生成。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceUnit {
    pub manager: ServiceManager,
    /// 服务名。
    pub name: String,
    /// 可执行路径。
    pub exec_path: String,
    /// 重启策略(`on-failure` / `always` / `never`).
    pub restart: String,
}

impl ServiceUnit {
    /// 渲染成单元文件内容。
    pub fn render(&self) -> String {
        match self.manager {
            ServiceManager::Launchd => format!(
                "<?xml version=\"1.0\"?>\n\
                 <plist version=\"1.0\"><dict>\n\
                 <key>Label</key><string>{}</string>\n\
                 <key>ProgramArguments</key><array><string>{}</string></array>\n\
                 <key>KeepAlive</key><true/>\n\
                 <key>RunAtLoad</key><true/>\n\
                 </dict></plist>\n",
                self.name, self.exec_path
            ),
            ServiceManager::Systemd => format!(
                "[Unit]\nDescription={}\n\n\
                 [Service]\nExecStart={}\nRestart={}\n\n\
                 [Install]\nWantedBy=default.target\n",
                self.name, self.exec_path, self.restart
            ),
            ServiceManager::WindowsService => format!(
                "; Windows service wrapper for {}\n\
                 sc create {} binPath= \"{}\" start= auto\n",
                self.name, self.name, self.exec_path
            ),
        }
    }

    /// 文件应放的位置(相对 HOME / 系统)。
    pub fn install_path(&self) -> String {
        match self.manager {
            ServiceManager::Launchd => format!("~/Library/LaunchAgents/{}.plist", self.name),
            ServiceManager::Systemd => format!("~/.config/systemd/user/{}.service", self.name),
            ServiceManager::WindowsService => "(system service registry)".to_string(),
        }
    }
}

/// 重启哨兵(GPU 问题防死循环)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartSentinel {
    /// 连续崩溃次数。
    pub crash_count: u32,
    /// 允许的最大连续崩溃。
    pub max_crashes: u32,
}

impl RestartSentinel {
    pub fn new(crash_count: u32, max_crashes: u32) -> RestartSentinel {
        RestartSentinel {
            crash_count,
            max_crashes,
        }
    }

    /// 是否应停止重启(进入"需人工介入")。
    pub fn should_stop(self) -> bool {
        self.crash_count >= self.max_crashes
    }

    /// 是否应回退到 GPU 禁用模式。
    ///
    /// 连续崩溃 2 次以上怀疑是 GPU 问题。
    pub fn should_disable_gpu(self) -> bool {
        self.crash_count >= 2 && !self.should_stop()
    }

    /// 记录一次崩溃。
    pub fn on_crash(mut self) -> RestartSentinel {
        self.crash_count += 1;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── F51 ──

    #[test]
    fn fence_language_inference() {
        assert_eq!(
            RenderableBlock::from_fence_language("mermaid", "graph TD").renderer,
            Renderer::Mermaid
        );
        assert_eq!(
            RenderableBlock::from_fence_language("latex", "x^2").renderer,
            Renderer::Math
        );
        assert_eq!(
            RenderableBlock::from_fence_language("diff", "- a\n+ b").renderer,
            Renderer::Diff
        );
        assert_eq!(
            RenderableBlock::from_fence_language("rust", "fn main() {}").renderer,
            Renderer::Markdown,
            "未知语言回落 markdown"
        );
    }

    #[test]
    fn renderer_properties() {
        assert!(Renderer::Mermaid.needs_sandbox());
        assert!(Renderer::Waveform.needs_sandbox());
        assert!(!Renderer::Diff.needs_sandbox());
        assert!(Renderer::Image.supports_zoom());
        assert!(!Renderer::Table.supports_zoom());
    }

    #[test]
    fn renderer_support_fallback() {
        let support = RendererSupport::new().with(Renderer::Mermaid);
        assert_eq!(support.fallback_for(Renderer::Mermaid), Renderer::Mermaid);
        assert_eq!(support.fallback_for(Renderer::Math), Renderer::Markdown);
    }

    #[test]
    fn block_empty_check() {
        assert!(RenderableBlock::new(Renderer::Markdown, "  ").is_empty());
        assert!(!RenderableBlock::new(Renderer::Markdown, "x").is_empty());
    }

    #[test]
    fn block_anchor() {
        let b = RenderableBlock::new(Renderer::Mermaid, "graph").with_anchor("sec-1");
        assert_eq!(b.anchor.as_deref(), Some("sec-1"));
    }

    // ── F52 ──

    #[test]
    fn context_gauge_levels() {
        assert_eq!(ContextGauge::new(10, 100).level(), GaugeLevel::Normal);
        assert_eq!(ContextGauge::new(80, 100).level(), GaugeLevel::Warning);
        assert_eq!(ContextGauge::new(95, 100).level(), GaugeLevel::Critical);
    }

    #[test]
    fn context_gauge_ratio_clamped() {
        assert!((ContextGauge::new(50, 100).ratio() - 0.5).abs() < 1e-9);
        assert_eq!(ContextGauge::new(150, 100).ratio(), 1.0);
        assert_eq!(ContextGauge::new(10, 0).ratio(), 0.0);
    }

    #[test]
    fn context_gauge_label_and_remaining() {
        let g = ContextGauge::new(25, 100);
        assert!(g.label().contains("25%"), "{}", g.label());
        assert_eq!(g.remaining(), 75);
    }

    #[test]
    fn scm_panel_by_turn_and_summary() {
        let mut panel = ScmPanel::new();
        panel.push(ScmChange {
            turn_id: "t1".into(),
            path: "a.rs".into(),
            lines_changed: 10,
            kind: "modified".into(),
        });
        panel.push(ScmChange {
            turn_id: "t2".into(),
            path: "a.rs".into(),
            lines_changed: 5,
            kind: "modified".into(),
        });
        assert_eq!(panel.for_turn("t1").len(), 1);
        let summary = panel.summary();
        assert_eq!(summary["a.rs"], 15, "跨回合汇总");
        assert!(panel.for_turn("t1")[0].render().contains("a.rs"));
    }

    // ── F54 ──

    fn span(id: &str, parent: Option<&str>, name: &str, start: u64, dur: u64) -> TraceSpan {
        TraceSpan {
            span_id: id.into(),
            parent_id: parent.map(String::from),
            name: name.into(),
            start_ms: start,
            duration_ms: dur,
            attributes: BTreeMap::new(),
        }
    }

    #[test]
    fn trace_tree() {
        let mut trace = Trace::new();
        trace.push(span("root", None, "turn", 0, 1000));
        trace.push(span("c1", Some("root"), "model_call", 10, 500));
        trace.push(span("c2", Some("root"), "tool:bash", 520, 300));
        assert_eq!(trace.roots().len(), 1);
        assert_eq!(trace.children_of("root").len(), 2);
        assert_eq!(trace.total_duration_ms(), 1000);
    }

    #[test]
    fn trace_ordered_by_start() {
        let mut trace = Trace::new();
        trace.push(span("late", None, "b", 500, 10));
        trace.push(span("early", None, "a", 100, 10));
        let ordered = trace.ordered();
        assert_eq!(ordered[0].span_id, "early");
    }

    #[test]
    fn trace_duration_by_name() {
        let mut trace = Trace::new();
        trace.push(span("1", None, "model_call", 0, 300));
        trace.push(span("2", None, "model_call", 400, 200));
        trace.push(span("3", None, "tool:bash", 700, 100));
        let by_name = trace.duration_by_name();
        assert_eq!(by_name[0].0, "model_call");
        assert_eq!(by_name[0].1, 500);
    }

    #[test]
    fn trace_empty() {
        let trace = Trace::new();
        assert_eq!(trace.total_duration_ms(), 0);
        assert!(trace.is_empty());
    }

    #[test]
    fn step_summary_from_spans() {
        let mut trace = Trace::new();
        trace.push(span("1", None, "tool:bash", 0, 100));
        trace.push(span("2", None, "model_call", 200, 100));
        let summaries = summarize_spans(&trace);
        assert_eq!(summaries.len(), 2);
        assert!(
            summaries[0].description.contains("bash"),
            "{:?}",
            summaries[0]
        );
        assert!(summaries[0].render().contains("[ok]"));
    }

    #[test]
    fn step_summary_outcome_from_attributes() {
        let mut s = span("1", None, "tool:bash", 0, 100);
        s.attributes.insert("outcome".into(), "error".into());
        let mut trace = Trace::new();
        trace.push(s);
        let summaries = summarize_spans(&trace);
        assert_eq!(summaries[0].outcome, "error");
    }

    // ── F57 ──

    #[test]
    fn update_manifest_validate() {
        let good = UpdateManifest {
            version: "1.2.3".into(),
            platform: "darwin-x64".into(),
            url: "https://x".into(),
            sha256: "abc".into(),
            signature: "sig".into(),
            rollback_available: true,
        };
        assert!(good.validate().is_ok());

        let unsigned = UpdateManifest {
            signature: String::new(),
            ..good.clone()
        };
        assert!(unsigned.validate().is_err(), "无签名不可信");

        let no_hash = UpdateManifest {
            sha256: String::new(),
            ..good
        };
        assert!(no_hash.validate().is_err());
    }

    #[test]
    fn update_version_comparison() {
        let m = UpdateManifest {
            version: "1.2.4".into(),
            platform: "p".into(),
            url: "u".into(),
            sha256: "h".into(),
            signature: "s".into(),
            rollback_available: true,
        };
        assert!(m.is_newer_than("1.2.3"));
        assert!(!m.is_newer_than("1.2.4"));
        assert!(!m.is_newer_than("1.3.0"));
        assert!(m.is_newer_than("v1.2.3"), "支持 v 前缀");
    }

    #[test]
    fn service_unit_systemd() {
        let unit = ServiceUnit {
            manager: ServiceManager::Systemd,
            name: "zhuye".into(),
            exec_path: "/usr/bin/zhuye".into(),
            restart: "on-failure".into(),
        };
        let rendered = unit.render();
        assert!(rendered.contains("[Unit]"), "{rendered}");
        assert!(rendered.contains("ExecStart=/usr/bin/zhuye"), "{rendered}");
        assert!(rendered.contains("Restart=on-failure"), "{rendered}");
        assert!(unit.install_path().contains(".config/systemd/user"));
    }

    #[test]
    fn service_unit_launchd() {
        let unit = ServiceUnit {
            manager: ServiceManager::Launchd,
            name: "com.zhuye.agent".into(),
            exec_path: "/usr/local/bin/zhuye".into(),
            restart: "always".into(),
        };
        let rendered = unit.render();
        assert!(rendered.contains("<plist"), "{rendered}");
        assert!(rendered.contains("com.zhuye.agent"), "{rendered}");
        assert!(unit.install_path().contains("LaunchAgents"));
    }

    #[test]
    fn restart_sentinel_progression() {
        let s = RestartSentinel::new(0, 5);
        assert!(!s.should_stop());
        assert!(!s.should_disable_gpu());

        let s = s.on_crash();
        assert!(!s.should_disable_gpu(), "1 次崩溃还不够");

        let s = s.on_crash();
        assert!(s.should_disable_gpu(), "2 次怀疑 GPU");
        assert!(!s.should_stop());

        let s = RestartSentinel::new(5, 5);
        assert!(s.should_stop(), "达上限停止重启");
        assert!(!s.should_disable_gpu(), "停止时不谈降级");
    }

    #[test]
    fn trace_span_serializes() {
        let s = span("1", None, "x", 0, 10);
        let json = serde_json::to_string(&s).expect("serialize");
        let back: TraceSpan = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, s);
    }
}
