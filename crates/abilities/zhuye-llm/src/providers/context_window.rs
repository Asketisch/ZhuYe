//! `context_window` — 每模型上下文窗口大小(token)查询。
//!
//! 两级来源:
//! 1. **运行时 override 表**(v1.10 功能3):进程级全局 map,由
//!    `zhuye-config` 在 bootstrap / 热重载时灌入 config.toml 的
//!    `[context_windows]` per-model 覆盖表(+ `[model].context_window`
//!    合并结果)。让私端 / 新模型无需改代码即可配窗口,且
//!    **所有**消费方(引擎 `SessionConfigured`、`output_budget`
//!    自适应输出预算、TUI 用量条)读到同一口径。
//! 2. **静态回退表**:一张 `const` 表 + 前缀匹配(仿 [`pricing.rs`] 模式)。
//!
//! 供 `SessionConfiguredEvent.context_window_size`(引擎报告)+ TUI 上下文
//! 用量条(分子 / 分母)+ `output_budget` 复用,避免与 LLM 层漂移。
//!
//! ## 归一化
//!
//! 输入 `model_spec` 可能带 `provider/` 前缀、ollama tag(`:7b`/`:latest`)、
//! 大小写差异。归一化三步:
//! 1. `rsplit_once('/')` 去 provider 前缀(`anthropic/claude-...` → `claude-...`)。
//! 2. `split(':')` 去 ollama tag(`llama3.2:7b` → `llama3.2`)。
//! 3. 小写化,`starts_with` 匹配版本后缀。
//!
//! 未知模型返回 `None`(TUI 优雅省略上下文条,比显示错数字更好)。

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// 静态模型 → 上下文窗口(token)表。覆盖 Zhuye 支持的主线模型族。
/// **更新节奏**:跟 provider 官方文档同步;新模型上线时补条目。
const CONTEXT_WINDOW_TABLE: &[(&str, u32)] = &[
    // ── Anthropic Claude(200k 标准) ──────────────────────────────────
    ("claude-opus-4", 200_000),
    ("claude-sonnet-4", 200_000),
    ("claude-3-5-sonnet", 200_000),
    ("claude-3-5-haiku", 200_000),
    ("claude-3-opus", 200_000),
    ("claude-3-haiku", 200_000),
    // ── OpenAI(128k 标准;o3-mini 200k) ─────────────────────────────
    ("gpt-4o", 128_000),
    ("gpt-4o-mini", 128_000),
    ("gpt-4-turbo", 128_000),
    ("o3-mini", 200_000),
    // ── Ollama 本地模型(128k 通用;以主流 config 为准) ────────────────
    ("llama3.3", 128_000),
    ("llama3.2", 128_000),
    ("llama3.1", 128_000),
    ("qwen2.5", 128_000),
    ("mistral-nemo", 128_000),
    ("gemma2", 8_000),
    // ── MiniMax(M3/M4 系列官方 1M 上下文,MSA 稀疏注意力) ──────────────
    // `minimax-m` 前缀覆盖 M1/M3/M4 等;`abab` 是老命名(abab6.5 等)。
    ("minimax-m", 1_000_000),
    ("abab", 245_000),
];

/// 运行时 override 表(进程级单例)。键为**归一化后**的模型名,查询走
/// 「归一化精确 → 前缀」两级匹配(同 `ContextWindowsSection::lookup`)。
///
/// 为什么是全局单例而非逐 client 字段:窗口的消费方分散在引擎
/// (`SessionConfigured`)、provider 层(`output_budget`)与 TUI 投影,
/// 而 client 构造期不知道请求会带哪个 model(`ChatRequest.model`
/// 逐请求变化);一个进程级表 + bootstrap / 热重载统一灌入是唯一
/// 让所有读者口径一致的挂点。
static OVERRIDE_TABLE: OnceLock<RwLock<HashMap<String, u32>>> = OnceLock::new();

fn override_table() -> &'static RwLock<HashMap<String, u32>> {
    OVERRIDE_TABLE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 测试辅助:override 表是进程级全局,涉及 set/clear 的单测(本模块与
/// `output_budget` 模块)用这把锁互斥,避免并行用例互相覆盖。
#[cfg(test)]
pub(crate) fn override_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// v1.10 功能3:全量替换运行时 override 表(键会先归一化再存入)。
///
/// 由 `zhuye-config::ReflectConfig::apply_to_registry`(bootstrap 与
/// 热重载的公共 choke point)调用;传空 map 即清空(回到纯静态表)。
pub fn set_context_window_overrides(entries: &HashMap<String, u32>) {
    let normalized: HashMap<String, u32> = entries
        .iter()
        .map(|(k, v)| (normalize_model_spec(k), *v))
        .filter(|(k, _)| !k.is_empty())
        .collect();
    if let Ok(mut guard) = override_table().write() {
        *guard = normalized;
    }
}

/// 查运行时 override 表(归一化精确匹配 → 前缀匹配)。
/// 不查静态表 —— 调用方(`context_window_for` / output_budget)用它
/// 区分「窗口来自用户配置」与「窗口来自内置回退表」。
pub fn context_window_override_for(model_spec: &str) -> Option<u32> {
    let key = normalize_model_spec(model_spec);
    if key.is_empty() {
        return None;
    }
    let guard = override_table().read().ok()?;
    // 精确匹配(归一化后)。
    if let Some(v) = guard.get(&key) {
        return Some(*v);
    }
    // 前缀匹配(同静态表策略:配置 "qwen36" 覆盖 "qwen36-1m")。
    guard
        .iter()
        .find(|(k, _)| k.starts_with(&key) || key.starts_with(k.as_str()))
        .map(|(_, v)| *v)
}

/// 模型名归一化:去 provider 前缀 / ollama tag、小写化 trim。
/// 与 `zhuye_config::schema::normalize_model_key` 保持同一口径。
pub fn normalize_model_spec(model_spec: &str) -> String {
    let no_provider = model_spec
        .rsplit_once('/')
        .map(|(_, m)| m)
        .unwrap_or(model_spec);
    let no_tag = no_provider.split(':').next().unwrap_or(no_provider);
    no_tag.trim().to_ascii_lowercase()
}

/// 查 `model_spec` 的上下文窗口大小（token）。归一化 provider 前缀 /
/// ollama tag / 大小写后用 `starts_with` 匹配版本后缀。
///
/// 级联:**运行时 override 表(用户配置,优先)** → 内置静态回退表 →
/// `None`。返回 `None` 表示未知模型（TUI 此时省略上下文条）。
///
/// # 示例
///
/// 已知模型（带 / 不带 `provider/` 前缀、带 ollama tag）均返回命中值，
/// 未知模型返回 `None`：
///
/// ```
/// use zhuye_llm::context_window_for;
/// assert_eq!(context_window_for("anthropic/claude-sonnet-4-latest"), Some(200_000));
/// assert_eq!(context_window_for("openai/gpt-4o"), Some(128_000));
/// assert_eq!(context_window_for("llama3.2:7b"), Some(128_000));
/// assert_eq!(context_window_for("unknown-model"), None);
/// ```
pub fn context_window_for(model_spec: &str) -> Option<u32> {
    // 0. 运行时 override 表(用户在 config.toml 配的窗口)优先。
    if let Some(v) = context_window_override_for(model_spec) {
        return Some(v);
    }
    // 1. 去 provider 前缀(`rsplit_once('/')` 取最后一段,无 '/' 时整串)。
    let no_provider = model_spec
        .rsplit_once('/')
        .map(|(_, m)| m)
        .unwrap_or(model_spec);
    // 2. 去 ollama tag(`:7b` / `:latest` / `:instruct`)。
    let no_tag = no_provider.split(':').next().unwrap_or(no_provider);
    // 3. 小写化匹配。
    let needle = no_tag.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return None;
    }
    if matches!(needle.as_str(), "gpt-4" | "gpt-4-0314" | "gpt-4-0613") {
        return Some(8_192);
    }
    CONTEXT_WINDOW_TABLE
        .iter()
        .find(|(prefix, _)| needle.starts_with(prefix))
        .map(|(_, size)| *size)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── v1.10 功能3:运行时 override 表 ─────────────────────────────
    // 用全局唯一的模型名前缀,避免与并行执行的其他单测(查静态表)串扰;
    // 每个用例结束时清空 override 表,失败时泄漏的名字也不会命中别的用例。

    #[test]
    fn override_takes_precedence_over_static_table() {
        let _guard = override_test_lock().lock().unwrap();
        let mut entries = std::collections::HashMap::new();
        // 静态表里 claude-sonnet-4 = 200_000;override 改写为 1M。
        entries.insert("claude-sonnet-4-latest".to_string(), 1_000_000u32);
        set_context_window_overrides(&entries);
        assert_eq!(
            context_window_for("claude-sonnet-4-latest"),
            Some(1_000_000)
        );
        assert_eq!(
            context_window_override_for("claude-sonnet-4-latest"),
            Some(1_000_000)
        );
        set_context_window_overrides(&std::collections::HashMap::new());
        // 清空后回到静态表。
        assert_eq!(context_window_for("claude-sonnet-4-latest"), Some(200_000));
    }

    #[test]
    fn override_supports_custom_and_prefix_models() {
        let _guard = override_test_lock().lock().unwrap();
        let mut entries = std::collections::HashMap::new();
        // 私端新模型(静态表未知)+ 前缀式条目。
        entries.insert("zzov-custom-model-x".to_string(), 512_000u32);
        entries.insert("zzovfam".to_string(), 256_000u32);
        set_context_window_overrides(&entries);
        // 归一化精确命中(带 provider 前缀 / tag / 大小写)。
        assert_eq!(context_window_for("zzov-custom-model-x"), Some(512_000));
        assert_eq!(
            context_window_for("vendor/ZZOV-Custom-Model-X"),
            Some(512_000)
        );
        assert_eq!(context_window_for("zzov-custom-model-x:8b"), Some(512_000));
        // 前缀命中:配置 "zzovfam" 覆盖 "zzovfam-pro-42b"。
        assert_eq!(context_window_for("zzovfam-pro-42b"), Some(256_000));
        // 反向前缀命中:配置带版本后缀、查询用短名。
        assert_eq!(context_window_override_for("zzovfam"), Some(256_000));
        set_context_window_overrides(&std::collections::HashMap::new());
        // 清空后未知模型回到 None。
        assert_eq!(context_window_for("zzov-custom-model-x"), None);
    }

    #[test]
    fn normalize_matches_config_side() {
        assert_eq!(
            normalize_model_spec("anthropic/Claude-3.5:latest"),
            "claude-3.5"
        );
        assert_eq!(normalize_model_spec("gpt-4o"), "gpt-4o");
        assert_eq!(normalize_model_spec(""), "");
    }

    #[test]
    fn strips_provider_prefix() {
        assert_eq!(
            context_window_for("anthropic/claude-sonnet-4-latest"),
            Some(200_000)
        );
        assert_eq!(context_window_for("openai/gpt-4o"), Some(128_000));
        assert_eq!(context_window_for("ollama/llama3.2"), Some(128_000));
    }

    #[test]
    fn strips_ollama_tag() {
        assert_eq!(context_window_for("llama3.2:7b"), Some(128_000));
        assert_eq!(context_window_for("qwen2.5:latest"), Some(128_000));
        assert_eq!(context_window_for("gpt-4o:instruct"), Some(128_000));
        assert_eq!(context_window_for("ollama/llama3.1:8b"), Some(128_000));
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(context_window_for("Claude-Sonnet-4-Latest"), Some(200_000));
        assert_eq!(context_window_for("GPT-4O"), Some(128_000));
        assert_eq!(context_window_for("O3-MINI"), Some(200_000));
    }

    #[test]
    fn matches_version_suffixes() {
        // `starts_with` 让版本后缀(-latest / -20240229 等)都能命中。
        assert_eq!(
            context_window_for("claude-3-5-sonnet-20241022"),
            Some(200_000)
        );
        assert_eq!(context_window_for("gpt-4o-2024-08-06"), Some(128_000));
    }

    #[test]
    fn unknown_model_returns_none() {
        assert_eq!(context_window_for("future-model-9000"), None);
        assert_eq!(context_window_for("gpt-4.1"), None);
        assert_eq!(context_window_for("gpt-4-32k"), None);
        assert_eq!(context_window_for("gpt-4-0613"), Some(8_192));
        assert_eq!(context_window_for(""), None);
        assert_eq!(context_window_for("/"), None);
    }

    #[test]
    fn bare_model_without_prefix() {
        // 无 provider 前缀也能查。
        assert_eq!(context_window_for("claude-opus-4-latest"), Some(200_000));
        assert_eq!(context_window_for("o3-mini"), Some(200_000));
    }

    #[test]
    fn minimax_m3_1m_context() {
        // MiniMax M3/M4 官方 1M 上下文(带/不带 provider 前缀、大小写)。
        assert_eq!(context_window_for("MiniMax-M3"), Some(1_000_000));
        assert_eq!(context_window_for("minimax/MiniMax-M3"), Some(1_000_000));
        assert_eq!(context_window_for("minimax-m4"), Some(1_000_000));
        // 老命名 abab 系列(abab6.5 = 245k)。
        assert_eq!(context_window_for("abab6.5-chat"), Some(245_000));
    }
}
