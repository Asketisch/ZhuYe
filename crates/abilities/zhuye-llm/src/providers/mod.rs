//! Provider implementations.

pub mod anthropic;
pub mod context_window;
pub mod mock;
pub mod ollama;
pub mod openai;
pub mod pricing;
pub mod quota;
pub mod responses;

use std::time::Duration;

use crate::error::LlmError;
use crate::request::ChatRequest;

// ── provider 共享的端点拼接与 SSE 响应校验 ─────────────────────────

/// 建连超时上限:即使 provider 段配了更长的 `timeout_secs`,建连阶段
/// 也不该等几分钟 —— 端点不可达(丢包 / 防火墙静默丢弃)时尽快失败,
/// 让自动重试与用户反馈都能及时到来。LLM 端点(含内网 vLLM)正常建连
/// 远快于 10s,超过即视为路径不通。
const CONNECT_TIMEOUT_CAP: Duration = Duration::from_secs(10);

/// 构建流式 provider 共用的 HTTP client(建连 + 读空闲双层超时)。
///
/// **不要**在这里挂 `ClientBuilder::timeout` —— reqwest 的 total timeout
/// 覆盖「建连 → 响应体读完」的全程,而流式 LLM 响应(长规划输出、
/// 推理模型思考)单次跑几分钟完全正常;total deadline 会把合法长流
/// 在固定时长处腰斩,`bytes_stream` 以 Decode 错误终止,表现为
/// `SSE_PARSE_ERROR: Transport error: error decoding response body`
/// (实战:内网 vLLM 端点 60s 整点断流,短请求全部成功)。
///
/// 正确语义是两层:
/// - `connect_timeout`:仅 TCP/TLS 建连阶段,封顶 [`CONNECT_TIMEOUT_CAP`],
///   防端点不可达时久挂;
/// - `read_timeout`:**读空闲**超时 —— 每次读到数据即重置计时,流只要
///   还在吐字节就永不超时,只在连接真正挂死(无字节窗口超时)才断。
///   默认 300s 覆盖推理模型 prefill/思考期的无字节窗口(对齐
///   OpenAI/Anthropic 官方 SDK 600s 量级的包容度)。
pub(crate) fn stream_http_client(timeout: Duration) -> Result<reqwest::Client, LlmError> {
    reqwest::Client::builder()
        .connect_timeout(timeout.min(CONNECT_TIMEOUT_CAP))
        .read_timeout(timeout)
        .build()
        .map_err(LlmError::from)
}

/// 把 base_url 与 API path 拼成完整端点。
///
/// base 的最后一段已是版本段(形如 `v1` / `v4`,仅含数字后缀)时
/// **不再追加 `/v1`**,直接接 path;否则补 `/v1`。三种实测形态:
/// - `https://api.openai.com` → `…/v1/chat/completions`(原行为);
/// - `https://gw.example.com/v1` → `…/v1/chat/completions`(修复:原来
///   拼成 `/v1/v1/…` 直接 404);
/// - `https://open.bigmodel.cn/api/paas/v4` → `…/v4/chat/completions`
///   (修复:版本段不是 v1 的网关原来无法表达)。
pub(crate) fn join_versioned_endpoint(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let last = base.rsplit('/').next().unwrap_or("");
    let is_version_segment =
        last.len() >= 2 && last.starts_with('v') && last[1..].chars().all(|c| c.is_ascii_digit());
    if is_version_segment {
        format!("{base}{path}")
    } else {
        format!("{base}/v1{path}")
    }
}

/// 校验「流式请求」的成功响应确实是 `text/event-stream`。
///
/// 引擎以 `stream=true` 发请求,但部分网关对不支持流式的模型会返回
/// 普通 JSON(HTTP 200)。此前该响应交给 SSE 解析器,一条事件也解析
/// 不出来 → 流以「成功但零内容」收场,`prompt()` 返回空串、无任何
/// 错误信号 —— 协议层故障被伪装成「模型没说话」。现在显式返回
/// `LlmError::Provider`(GiveUp 路径,不无谓重试),错误消息带
/// content-type 与响应体预览,引擎发 `EventMsg::Error`、SDK 升为异常。
pub(crate) async fn ensure_sse_response(
    response: reqwest::Response,
    endpoint: &str,
) -> Result<reqwest::Response, LlmError> {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if content_type.contains("text/event-stream") {
        return Ok(response);
    }
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    let preview: String = body.chars().take(512).collect();
    Err(LlmError::Provider {
        status,
        message: format!(
            "流式请求收到非 SSE 响应(content-type: '{}');端点 {endpoint} 可能不支持 \
             stream=true,或把错误载荷当 200 返回了。响应体预览: {preview}",
            if content_type.is_empty() {
                "缺失"
            } else {
                &content_type
            }
        ),
    })
}

/// 把 provider 段配置的 temperature / max_tokens 作为**默认值**
/// 合入请求(请求侧显式值优先)。`ChatRequest` 是各 provider 共用的
/// 入参形态,在 `stream()` 入口统一做一次默认值回填,比在各
/// `XxxRequest::from` 里分散处理更不易漏。
/// v1.9 C8 文档见函数体;`apply_request_param_defaults` 在「请求侧与
/// provider 段配置都未给 max_tokens」时按模型上下文窗口自适应推
/// 输出预算(见 `crate::output_budget`);模型窗口或最大输出未知时维持
/// `None`,沿用 provider 原有默认行为。
pub(crate) fn apply_request_param_defaults(
    request: &mut ChatRequest,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
) {
    if request.temperature.is_none() {
        request.temperature = temperature;
    }
    if request.max_tokens.is_none() {
        request.max_tokens = max_tokens;
    }
    // v1.9 C8:两级默认都缺席 → 自适应预算兜底。
    if request.max_tokens.is_none() {
        request.max_tokens = crate::output_budget::adaptive_max_tokens(request);
    }
}

pub use self::anthropic::{AnthropicClient, AnthropicConfig};
pub use self::context_window::{
    context_window_for, context_window_override_for, normalize_model_spec,
    set_context_window_overrides,
};
pub use self::mock::{
    DEFAULT_MOCK_MODEL, DEFAULT_MOCK_REPLY, ENV_MOCK_SCRIPT, MockClient, MockReply,
};
pub use self::ollama::{OllamaClient, OllamaConfig};
pub use self::openai::{OpenAIClient, OpenAIConfig};
pub use self::pricing::{
    ModelPricing, RateCard, cumulative_cost_usd, is_priced, price, pricing_for, pricing_table,
    rate_card, rate_cards,
};
pub use self::quota::{
    KimiQuotaProvider, MinimaxQuotaProvider, QuotaError, QuotaProvider, QuotaSnapshot,
    ZenmuxQuotaProvider, ZhipuQuotaProvider,
};
pub use self::responses::{OpenAIResponsesClient, OpenAIResponsesConfig};
