//! # IDE 桥与端侧推理(F66 / F67 / F68)
//!
//! 三块合成一个模块(都属「新接入面」):
//!
//! ## F66 IDE 编辑器集成
//!
//! [`IdeBridge`] —— 把 SDK/serve 协议桥给 VS Code / JetBrains:
//! 命令面板、侧栏会话、diff 视图、审批弹窗四个接入点。桥只做**协议
//! 翻译**(IDE 消息 ↔ Op/Event),不含业务逻辑。
//!
//! ## F67 端侧推理 / ONNX
//!
//! [`OnnxProviderSpec`] —— 进程内 ONNX Runtime / LiteRT 推理 provider:
//! 模型目录、量化档、基准。完全离线,不依赖外部服务。
//!
//! ## F68 多模态生成
//!
//! [`MediaGenRequest`] —— 图像 / TTS / 音乐 / 视频生成(per-key 出站
//! API),产物落 `<workspace>/.zhuye/artifacts/`。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ── F66:IDE 桥 ──────────────────────────────────────────────────────────

/// IDE 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdeKind {
    #[default]
    VsCode,
    JetBrains,
    Zed,
    Neovim,
}

impl IdeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            IdeKind::VsCode => "vscode",
            IdeKind::JetBrains => "jetbrains",
            IdeKind::Zed => "zed",
            IdeKind::Neovim => "neovim",
        }
    }
}

/// IDE 接入点。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdeSurface {
    /// 命令面板。
    CommandPalette,
    /// 侧栏会话。
    SidebarSessions,
    /// diff 视图。
    DiffView,
    /// 审批弹窗。
    ApprovalDialog,
}

impl IdeSurface {
    pub const ALL: [IdeSurface; 4] = [
        IdeSurface::CommandPalette,
        IdeSurface::SidebarSessions,
        IdeSurface::DiffView,
        IdeSurface::ApprovalDialog,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            IdeSurface::CommandPalette => "command_palette",
            IdeSurface::SidebarSessions => "sidebar_sessions",
            IdeSurface::DiffView => "diff_view",
            IdeSurface::ApprovalDialog => "approval_dialog",
        }
    }

    /// 该接入点需要的能力。
    pub fn required_capability(self) -> &'static str {
        match self {
            IdeSurface::CommandPalette => "commands",
            IdeSurface::SidebarSessions => "webview",
            IdeSurface::DiffView => "diff",
            IdeSurface::ApprovalDialog => "modal",
        }
    }
}

/// IDE 桥能力协商。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdeBridge {
    pub kind: IdeKind,
    /// IDE 声明支持的接入点。
    pub supported: Vec<IdeSurface>,
}

impl IdeBridge {
    pub fn new(kind: IdeKind) -> IdeBridge {
        IdeBridge {
            kind,
            supported: Vec::new(),
        }
    }

    /// 声明支持某接入点。
    pub fn with(mut self, surface: IdeSurface) -> IdeBridge {
        if !self.supported.contains(&surface) {
            self.supported.push(surface);
        }
        self
    }

    /// 是否支持某接入点。
    pub fn supports(&self, surface: IdeSurface) -> bool {
        self.supported.contains(&surface)
    }

    /// 可用的接入点(按 ALL 顺序)。
    pub fn available_surfaces(&self) -> Vec<IdeSurface> {
        IdeSurface::ALL
            .iter()
            .copied()
            .filter(|s| self.supports(*s))
            .collect()
    }

    /// 缺失的接入点(需降级提示)。
    pub fn missing_surfaces(&self) -> Vec<IdeSurface> {
        IdeSurface::ALL
            .iter()
            .copied()
            .filter(|s| !self.supports(*s))
            .collect()
    }

    /// 是否满足最低可用(至少命令面板)。
    pub fn is_usable(&self) -> bool {
        self.supports(IdeSurface::CommandPalette)
    }
}

// ── F67:端侧推理 ─────────────────────────────────────────────────────────

/// 端侧推理运行时。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceRuntime {
    /// ONNX Runtime。
    OnnxRuntime,
    /// LiteRT(TFLite 后继)。
    LiteRt,
    /// llama.cpp(本地 LLM)。
    LlamaCpp,
}

impl InferenceRuntime {
    pub fn as_str(self) -> &'static str {
        match self {
            InferenceRuntime::OnnxRuntime => "onnxruntime",
            InferenceRuntime::LiteRt => "litert",
            InferenceRuntime::LlamaCpp => "llama_cpp",
        }
    }
}

/// 量化档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quantization {
    /// 不量化(FP32)。
    None,
    /// FP16。
    Fp16,
    /// INT8。
    Int8,
    /// INT4(最省内存,质量损失最大)。
    Int4,
}

impl Quantization {
    /// 相对 FP32 的体积比例。
    pub fn size_ratio(self) -> f64 {
        match self {
            Quantization::None => 1.0,
            Quantization::Fp16 => 0.5,
            Quantization::Int8 => 0.25,
            Quantization::Int4 => 0.125,
        }
    }

    /// 是否推荐用于本地推理(平衡体积与质量)。
    pub fn is_recommended(self) -> bool {
        matches!(self, Quantization::Int8 | Quantization::Fp16)
    }
}

/// 端侧模型声明。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnnxProviderSpec {
    /// 模型 id。
    pub id: String,
    pub runtime: InferenceRuntime,
    pub quantization: Quantization,
    /// 文件字节数(未量化基准)。
    pub base_bytes: u64,
    /// sha256(完整性)。
    pub sha256: String,
    /// 是否已下载。
    #[serde(default)]
    pub installed: bool,
}

impl OnnxProviderSpec {
    /// 实际占用字节(按量化档折算)。
    pub fn actual_bytes(&self) -> u64 {
        (self.base_bytes as f64 * self.quantization.size_ratio()) as u64
    }

    /// 是否可加载。
    pub fn is_ready(&self) -> bool {
        self.installed && !self.sha256.trim().is_empty()
    }
}

/// 端侧模型基准。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OnnxBenchmark {
    pub model_id: String,
    /// 首 token 延迟(毫秒)。
    pub ttft_ms: f64,
    /// 每秒 token 数。
    pub tokens_per_sec: f64,
    /// 峰值内存(MB)。
    pub peak_memory_mb: f64,
}

impl OnnxBenchmark {
    /// 是否适合交互式使用(首 token < 1s 且 > 10 tok/s)。
    pub fn is_interactive(&self) -> bool {
        self.ttft_ms < 1000.0 && self.tokens_per_sec > 10.0
    }

    /// 人读摘要。
    pub fn summary(&self) -> String {
        format!(
            "{}: {:.0}ms 首 token · {:.1} tok/s · {:.0}MB",
            self.model_id, self.ttft_ms, self.tokens_per_sec, self.peak_memory_mb
        )
    }
}

// ── F68:多模态生成 ───────────────────────────────────────────────────────

/// 生成媒体类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Tts,
    Music,
    Video,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Tts => "tts",
            MediaKind::Music => "music",
            MediaKind::Video => "video",
        }
    }

    /// 文件扩展名。
    pub fn extension(self) -> &'static str {
        match self {
            MediaKind::Image => "png",
            MediaKind::Tts => "mp3",
            MediaKind::Music => "mp3",
            MediaKind::Video => "mp4",
        }
    }

    /// 是否异步生成(视频 / 音乐通常耗时较长)。
    pub fn is_long_running(self) -> bool {
        matches!(self, MediaKind::Music | MediaKind::Video)
    }
}

/// 生成请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaGenRequest {
    pub kind: MediaKind,
    /// 提示词 / 文本。
    pub prompt: String,
    /// 使用的 provider key 标签(per-key 出站)。
    pub credential_label: String,
    /// 额外参数(尺寸 / 音色 / 时长)。
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

impl MediaGenRequest {
    pub fn new(
        kind: MediaKind,
        prompt: impl Into<String>,
        credential_label: impl Into<String>,
    ) -> MediaGenRequest {
        MediaGenRequest {
            kind,
            prompt: prompt.into(),
            credential_label: credential_label.into(),
            params: BTreeMap::new(),
        }
    }

    pub fn with_param(
        mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> MediaGenRequest {
        self.params.insert(key.into(), value.into());
        self
    }

    /// 产物路径(落在 `<workspace>/.zhuye/artifacts/`)。
    ///
    /// 文件名由 `prompt` 指纹 + 扩展名组成(内容寻址,不覆盖)。
    pub fn artifact_path(&self, workspace: &str, fingerprint: &str) -> String {
        format!(
            "{}/.zhuye/artifacts/{}-{}.{}",
            workspace.trim_end_matches('/'),
            self.kind.as_str(),
            &fingerprint[..fingerprint.len().min(16)],
            self.kind.extension()
        )
    }

    /// 校验(prompt 非空 + 凭据非空)。
    pub fn validate(&self) -> Result<(), String> {
        if self.prompt.trim().is_empty() {
            return Err("生成提示词不能为空".to_string());
        }
        if self.credential_label.trim().is_empty() {
            return Err("缺少 provider 凭据标签".to_string());
        }
        Ok(())
    }
}

/// 生成结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaGenResult {
    pub kind: MediaKind,
    /// 产物路径。
    pub path: String,
    /// 字节数。
    pub bytes: u64,
    /// 是否仍在生成(长任务)。
    pub pending: bool,
}

impl MediaGenResult {
    /// 是否可用。
    pub fn is_ready(&self) -> bool {
        !self.pending && self.bytes > 0
    }

    /// 人读摘要。
    pub fn summary(&self) -> String {
        if self.pending {
            format!("{} 生成中…", self.kind.as_str())
        } else {
            format!(
                "{} 已生成 · {} 字节 · {}",
                self.kind.as_str(),
                self.bytes,
                self.path
            )
        }
    }
}

/// 媒体 provider 注册表。
#[derive(Debug, Default)]
pub struct MediaProviderRegistry {
    /// `kind -> provider 名列表`。
    providers: BTreeMap<&'static str, Vec<String>>,
}

impl MediaProviderRegistry {
    pub fn new() -> MediaProviderRegistry {
        MediaProviderRegistry::default()
    }

    /// 注册某类型可用的 provider。
    pub fn register(&mut self, kind: MediaKind, provider: impl Into<String>) {
        self.providers
            .entry(kind.as_str())
            .or_default()
            .push(provider.into());
    }

    /// 某类型的 provider 列表。
    pub fn providers_for(&self, kind: MediaKind) -> &[String] {
        self.providers
            .get(kind.as_str())
            .map_or(&[][..], Vec::as_slice)
    }

    /// 是否支持某类型。
    pub fn supports(&self, kind: MediaKind) -> bool {
        !self.providers_for(kind).is_empty()
    }

    /// 支持的类型。
    pub fn supported_kinds(&self) -> Vec<MediaKind> {
        [
            MediaKind::Image,
            MediaKind::Tts,
            MediaKind::Music,
            MediaKind::Video,
        ]
        .into_iter()
        .filter(|k| self.supports(*k))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── F66 ──

    #[test]
    fn ide_bridge_surfaces() {
        let bridge = IdeBridge::new(IdeKind::VsCode)
            .with(IdeSurface::CommandPalette)
            .with(IdeSurface::DiffView);
        assert!(bridge.supports(IdeSurface::CommandPalette));
        assert!(!bridge.supports(IdeSurface::ApprovalDialog));
        assert_eq!(bridge.available_surfaces().len(), 2);
        assert_eq!(bridge.missing_surfaces().len(), 2);
        assert!(bridge.is_usable());
    }

    #[test]
    fn ide_bridge_no_palette_unusable() {
        let bridge = IdeBridge::new(IdeKind::JetBrains).with(IdeSurface::DiffView);
        assert!(!bridge.is_usable(), "缺命令面板不可用");
    }

    #[test]
    fn ide_surface_capabilities() {
        assert_eq!(IdeSurface::CommandPalette.required_capability(), "commands");
        assert_eq!(IdeSurface::DiffView.required_capability(), "diff");
        assert_eq!(IdeSurface::ApprovalDialog.required_capability(), "modal");
    }

    // ── F67 ──

    #[test]
    fn quantization_ratios() {
        assert_eq!(Quantization::None.size_ratio(), 1.0);
        assert_eq!(Quantization::Fp16.size_ratio(), 0.5);
        assert_eq!(Quantization::Int8.size_ratio(), 0.25);
        assert_eq!(Quantization::Int4.size_ratio(), 0.125);
    }

    #[test]
    fn quantization_recommendation() {
        assert!(Quantization::Int8.is_recommended());
        assert!(Quantization::Fp16.is_recommended());
        assert!(!Quantization::None.is_recommended());
        assert!(!Quantization::Int4.is_recommended(), "质量损失过大");
    }

    #[test]
    fn onnx_actual_bytes() {
        let spec = OnnxProviderSpec {
            id: "m".into(),
            runtime: InferenceRuntime::OnnxRuntime,
            quantization: Quantization::Int8,
            base_bytes: 1_000_000,
            sha256: "abc".into(),
            installed: true,
        };
        assert_eq!(spec.actual_bytes(), 250_000);
    }

    #[test]
    fn onnx_ready_requires_install_and_hash() {
        let mut spec = OnnxProviderSpec {
            id: "m".into(),
            runtime: InferenceRuntime::LiteRt,
            quantization: Quantization::Fp16,
            base_bytes: 100,
            sha256: "abc".into(),
            installed: false,
        };
        assert!(!spec.is_ready());
        spec.installed = true;
        assert!(spec.is_ready());
        spec.sha256 = String::new();
        assert!(!spec.is_ready(), "无哈希不可信");
    }

    #[test]
    fn benchmark_interactive() {
        let good = OnnxBenchmark {
            model_id: "m".into(),
            ttft_ms: 500.0,
            tokens_per_sec: 20.0,
            peak_memory_mb: 500.0,
        };
        assert!(good.is_interactive());
        assert!(good.summary().contains("500ms"));

        let slow = OnnxBenchmark {
            ttft_ms: 2000.0,
            ..good.clone()
        };
        assert!(!slow.is_interactive(), "首 token 太慢");

        let slow_tps = OnnxBenchmark {
            tokens_per_sec: 5.0,
            ..good
        };
        assert!(!slow_tps.is_interactive());
    }

    // ── F68 ──

    #[test]
    fn media_kind_extensions() {
        assert_eq!(MediaKind::Image.extension(), "png");
        assert_eq!(MediaKind::Tts.extension(), "mp3");
        assert_eq!(MediaKind::Video.extension(), "mp4");
    }

    #[test]
    fn media_kind_long_running() {
        assert!(MediaKind::Music.is_long_running());
        assert!(MediaKind::Video.is_long_running());
        assert!(!MediaKind::Image.is_long_running());
        assert!(!MediaKind::Tts.is_long_running());
    }

    #[test]
    fn media_request_artifact_path() {
        let req = MediaGenRequest::new(MediaKind::Image, "a cat", "openai-key");
        let path = req.artifact_path("/workspace/proj", "abcdef0123456789extra");
        assert!(
            path.starts_with("/workspace/proj/.zhuye/artifacts/"),
            "{path}"
        );
        assert!(path.contains("image-"), "{path}");
        assert!(path.ends_with(".png"), "{path}");
        assert!(!path.contains("extra"), "指纹截断到 16 位:{path}");
    }

    #[test]
    fn media_request_validate() {
        let good = MediaGenRequest::new(MediaKind::Image, "a cat", "key");
        assert!(good.validate().is_ok());

        let no_prompt = MediaGenRequest::new(MediaKind::Image, "   ", "key");
        assert!(no_prompt.validate().is_err());

        let no_cred = MediaGenRequest::new(MediaKind::Image, "cat", "  ");
        assert!(no_cred.validate().is_err());
    }

    #[test]
    fn media_result_ready() {
        let done = MediaGenResult {
            kind: MediaKind::Image,
            path: "/a.png".into(),
            bytes: 1000,
            pending: false,
        };
        assert!(done.is_ready());
        assert!(done.summary().contains("已生成"));

        let pending = MediaGenResult {
            pending: true,
            ..done
        };
        assert!(!pending.is_ready());
        assert!(pending.summary().contains("生成中"));
    }

    #[test]
    fn media_provider_registry() {
        let mut reg = MediaProviderRegistry::new();
        reg.register(MediaKind::Image, "openai");
        reg.register(MediaKind::Tts, "elevenlabs");
        assert!(reg.supports(MediaKind::Image));
        assert!(!reg.supports(MediaKind::Video));
        assert_eq!(reg.providers_for(MediaKind::Image), &["openai".to_string()]);
        assert_eq!(reg.supported_kinds().len(), 2);
    }

    #[test]
    fn media_request_serializes() {
        let req = MediaGenRequest::new(MediaKind::Tts, "hello", "key").with_param("voice", "alloy");
        let json = serde_json::to_string(&req).expect("serialize");
        let back: MediaGenRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, req);
    }
}
