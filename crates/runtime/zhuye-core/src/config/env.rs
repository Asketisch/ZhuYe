//! 基于环境变量的配置解析(compaction 触发、session token 预算、最大迭代数)。
//!
//! 每个 resolver 在调用时读取 env,以便测试在用例间切换;生产调用方
//! (`zhuye-exec::bootstrap`)在启动时拍下快照。

use zhuye_compact::CompactorConfig;

/// 覆盖默认 compaction 触发阈值的环境变量。
pub const AUTO_COMPACT_INPUT_TOKENS_ENV: &str = "ZHUYE_AUTO_COMPACT_INPUT_TOKENS";

/// v1.2 P1-12:设置 session 级 token 硬预算的环境变量。当累计 session 用量
/// (input + output)达到该值时,turn 以 `TurnStatus::TokenBudgetExceeded` 结束。
/// 优先级:env `ZHUYE_TOKEN_BUDGET` > TOML `[token_budget].session_total_tokens` > None。
pub const TOKEN_BUDGET_ENV: &str = "ZHUYE_TOKEN_BUDGET";

/// v1.x:覆盖全局 agent-loop 迭代上限的环境变量。
/// 优先级:env `ZHUYE_MAX_ITERATIONS` > TOML `active.max_iterations`
/// > [`DEFAULT_MAX_ITERATIONS`] (32)。
pub const MAX_ITERATIONS_ENV: &str = "ZHUYE_MAX_ITERATIONS";

/// 当 env 与 TOML 都未配置时,每轮 `model_call` 迭代上限的默认值。镜像历史硬编码值。
pub const DEFAULT_MAX_ITERATIONS: u32 = 32;

/// 按优先级解析 session token 预算:env `ZHUYE_TOKEN_BUDGET` > `toml_budget` > `None`。
///
/// 调用时读取 env,以便测试在用例间切换。生产调用方(`zhuye-exec::bootstrap`)
/// 在启动时拍快照。与 [`trigger_tokens_from_env`](compaction)形态一致,但返回
/// `Option<u64>`,因为「无预算」也是合法默认值(仅靠 `max_iterations` 限制循环)。
pub fn token_budget_from_env(toml_budget: Option<u64>) -> Option<u64> {
    if let Some(env_val) = std::env::var(TOKEN_BUDGET_ENV)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    {
        return Some(env_val);
    }
    toml_budget
}

/// 按优先级解析全局 agent-loop 迭代上限:env `ZHUYE_MAX_ITERATIONS` >
/// `toml_max_iterations` > [`DEFAULT_MAX_ITERATIONS`] (32)。
///
/// 调用时读取 env,以便测试在用例间切换。生产调用方(`zhuye-exec::bootstrap`)
/// 在启动时拍快照。与 [`token_budget_from_env`] 形态一致,总是返回具体的
/// `u32`(缺失时回退到文档化默认值,而非禁用上限 —— 无界 agent loop 永远不可取)。
pub fn max_iterations_from_env(toml_max_iterations: Option<u32>) -> u32 {
    if let Some(env_val) = std::env::var(MAX_ITERATIONS_ENV)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
    {
        return env_val;
    }
    toml_max_iterations.unwrap_or(DEFAULT_MAX_ITERATIONS)
}

/// 默认触发阈值。调用时读取环境变量,以便测试在用例间切换;
/// 生产调用方(`zhuye-exec::bootstrap_m5`)在启动时拍快照。
pub fn trigger_tokens_from_env() -> u32 {
    std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000)
}

/// 按优先级解析触发阈值,构造 `CompactorConfig`:env `ZHUYE_AUTO_COMPACT_INPUT_TOKENS`
/// > TOML `trigger_tokens` > `CompactorConfig::default()` (= 10 000)。
///
/// `toml_trigger_tokens` 取自 `~/.zhuye/config.toml` 中 `ZhuyeConfig.compact.trigger_tokens`。
/// 若该 section 缺失则传 `None`。`CompactorConfig::default()` 是其他可调参数
/// (`microcompact_ratio`、`keep_recent_*`、`target_ratio`、`summarize_after`)
/// 的单一事实源;此处仅覆盖 `trigger_tokens`。
///
/// 函数在调用时读取 env,以便测试在用例间翻转 `ZHUYE_AUTO_COMPACT_INPUT_TOKENS`
/// (M7 parity:`std::env::set_var` 的竞态由测试套件中的 per-module env lock 串行化)。
pub fn compactor_config_from_env_and_toml(toml_trigger_tokens: Option<u32>) -> CompactorConfig {
    let mut cfg = CompactorConfig::default();
    if let Some(env_val) = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
    {
        cfg.trigger_tokens = env_val;
    } else if let Some(toml_val) = toml_trigger_tokens {
        cfg.trigger_tokens = toml_val;
    }
    cfg
}

/// v1.x:压缩触发比例默认值 —— 触发阈值 = 模型上下文窗口 × 0.85,留
/// 15% 余量给输出与框架 token。可用 `[compact].trigger_ratio` 覆盖。
pub const DEFAULT_TRIGGER_RATIO: f32 = 0.85;

/// v1.x:完全未知模型(显式配置 / 运行时覆盖表 / 静态表 / models.dev
/// 都查不到窗口)的兜底窗口。按主流基线 128K 推导触发点,取代历史
/// 一刀切的 10K 默认(10K 对现代模型低得离谱,会让 1M 窗口模型在
/// 12K token 就开始压缩,是桌面 sess_77cd626f 幽灵压缩死循环的温床)。
pub const FALLBACK_COMPACT_WINDOW_TOKENS: u32 = 128_000;

/// 触发阈值下限:窗口 × 比例再小也至少保留 1K 量级的触发点,避免 0。
const MIN_TRIGGER_TOKENS: u32 = 1_024;

/// 归一化触发比例:非有限 / ≤0 回落默认;>1 钳到 1(阈值不得超过窗口)。
fn sanitize_trigger_ratio(raw: Option<f32>) -> f32 {
    match raw {
        Some(r) if r.is_finite() && r > 0.0 => r.min(1.0),
        _ => DEFAULT_TRIGGER_RATIO,
    }
}

/// 保留阈值来源，以便每次决策按当前模型窗口重新推导。
pub fn compact_trigger_policy(
    toml_tokens: Option<u32>,
    ratio: Option<f32>,
) -> zhuye_compact::TriggerPolicy {
    if let Some(value) = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .or(toml_tokens)
    {
        zhuye_compact::TriggerPolicy::Fixed(value)
    } else {
        zhuye_compact::TriggerPolicy::WindowRatio(sanitize_trigger_ratio(ratio))
    }
}

/// v1.x:窗口感知的压缩触发阈值。优先级:
/// env `ZHUYE_AUTO_COMPACT_INPUT_TOKENS` > TOML `[compact].trigger_tokens` >
/// **模型上下文窗口 × `[compact].trigger_ratio`(默认 0.85)**。
///
/// 窗口解析:`explicit_window`(调用方从 `[context_windows]` /
/// `[model].context_window` 解析的显式配置)优先,其次
/// `zhuye_llm::context_window_for`(运行时覆盖表 → 静态表 → models.dev);
/// 全部落空 → [`FALLBACK_COMPACT_WINDOW_TOKENS`] 兜底。每个模型按自己的
/// 窗口与统一压缩比推导触发点 —— 长窗口模型(1M)不再被历史一刀切的
/// 10k 误触发,未知私端模型也有量级合理的默认。
///
/// `toml_trigger_tokens` / `toml_trigger_ratio` 取自 config.toml
/// `ZhuyeConfig.compact`;`model_spec` 是当前会话活跃模型。exec 与
/// 库门面(m4_bootstrap)两条装配路径都必须走本函数,禁止只走
/// [`compactor_config_from_env_and_toml`](那会让窗口推导在桌面静默失效,
/// v1.12 的教训)。
pub fn compactor_config_with_window(
    toml_trigger_tokens: Option<u32>,
    toml_trigger_ratio: Option<f32>,
    explicit_window: Option<u32>,
    model_spec: &str,
) -> CompactorConfig {
    let mut cfg = compactor_config_from_env_and_toml(toml_trigger_tokens);
    // 显式值(env 解析成功或 TOML 有值)永远最高优先;env 解析失败
    // (malformed)不算显式设置,照旧走窗口推导。
    let explicit_set = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .is_some()
        || toml_trigger_tokens.is_some();
    if explicit_set {
        return cfg;
    }
    let ratio = sanitize_trigger_ratio(toml_trigger_ratio);
    let window = explicit_window
        .or_else(|| zhuye_llm::context_window_for(model_spec))
        .unwrap_or(FALLBACK_COMPACT_WINDOW_TOKENS);
    // f64 乘法避免 f32 精度毛刺;向下取整 + 下限保底。
    cfg.trigger_tokens = (((window as f64) * (ratio as f64)) as u32).max(MIN_TRIGGER_TOKENS);
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// `std::env::set_var` 在 Rust 2024 中是 `unsafe`(与并发 `env::var`
    /// 读取非线程安全)。本模块所有测试经这把锁串行,避免并行测试
    /// 执行期间的数据竞争。
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn trigger_tokens_from_env_default() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV).ok();
        unsafe {
            std::env::remove_var(AUTO_COMPACT_INPUT_TOKENS_ENV);
        }
        assert_eq!(trigger_tokens_from_env(), 10_000);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, p);
            }
        }
    }

    #[test]
    fn trigger_tokens_from_env_override() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV).ok();
        unsafe {
            std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, "12345");
        }
        assert_eq!(trigger_tokens_from_env(), 12_345);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, p);
            }
        } else {
            unsafe {
                std::env::remove_var(AUTO_COMPACT_INPUT_TOKENS_ENV);
            }
        }
    }

    #[test]
    fn trigger_tokens_from_env_malformed_returns_default() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV).ok();
        unsafe {
            std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, "not-a-number");
        }
        assert_eq!(trigger_tokens_from_env(), 10_000);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, p);
            }
        } else {
            unsafe {
                std::env::remove_var(AUTO_COMPACT_INPUT_TOKENS_ENV);
            }
        }
    }

    // ── v1.x:窗口感知压缩触发阈值(窗口 × 压缩比)──────────────

    #[test]
    fn window_derived_trigger_when_no_explicit_value() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV).ok();
        unsafe {
            std::env::remove_var(AUTO_COMPACT_INPUT_TOKENS_ENV);
        }
        // 显式窗口 200k × 默认比例 0.85 = 170k。
        let cfg = compactor_config_with_window(None, None, Some(200_000), "any-model");
        assert_eq!(cfg.trigger_tokens, 170_000);
        // 自定义比例 0.5 → 100k。
        let half = compactor_config_with_window(None, Some(0.5), Some(200_000), "any-model");
        assert_eq!(half.trigger_tokens, 100_000);
        // 比例 >1 钳到 1(阈值不得超过窗口);≤0 / 非有限回落默认。
        let over = compactor_config_with_window(None, Some(2.0), Some(200_000), "any-model");
        assert_eq!(over.trigger_tokens, 200_000);
        let zero = compactor_config_with_window(None, Some(0.0), Some(200_000), "any-model");
        assert_eq!(zero.trigger_tokens, 170_000);
        let nan = compactor_config_with_window(None, Some(f32::NAN), Some(200_000), "any-model");
        assert_eq!(nan.trigger_tokens, 170_000);
        // 极小窗口保底 1024。
        let tiny = compactor_config_with_window(None, None, Some(1_000), "any-model");
        assert_eq!(tiny.trigger_tokens, 1_024);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, p);
            }
        }
    }

    #[test]
    fn explicit_values_beat_window() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV).ok();
        // TOML 显式值 > 窗口联动。
        let cfg = compactor_config_with_window(Some(7_000), None, Some(200_000), "any-model");
        assert_eq!(cfg.trigger_tokens, 7_000);
        // env 显式值 > 一切。
        unsafe {
            std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, "5555");
        }
        let cfg2 = compactor_config_with_window(Some(7_000), None, Some(200_000), "any-model");
        assert_eq!(cfg2.trigger_tokens, 5_555);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, p);
            }
        } else {
            unsafe {
                std::env::remove_var(AUTO_COMPACT_INPUT_TOKENS_ENV);
            }
        }
    }

    #[test]
    fn static_table_window_hits_ratio() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV).ok();
        unsafe {
            std::env::remove_var(AUTO_COMPACT_INPUT_TOKENS_ENV);
        }
        // 无显式配置时走静态表:gpt-4o = 128k → 128k × 0.85 = 108_800。
        let cfg = compactor_config_with_window(None, None, None, "gpt-4o");
        assert_eq!(cfg.trigger_tokens, 108_800);
        // MiniMax M3 系列 1M 窗口 → 850k(桌面 sess_77cd626f 的模型)。
        let minimax = compactor_config_with_window(None, None, None, "MiniMax-M3.1-Flash");
        assert_eq!(minimax.trigger_tokens, 850_000);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, p);
            }
        }
    }

    #[test]
    fn unknown_model_falls_back_to_128k_window() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV).ok();
        unsafe {
            std::env::remove_var(AUTO_COMPACT_INPUT_TOKENS_ENV);
        }
        // 全链路未知的私端模型 → 128K 兜底窗口 × 0.85 = 108_800,
        // 不再回落一刀切 10k。
        let cfg = compactor_config_with_window(None, None, None, "totally-unknown-xyz");
        assert_eq!(cfg.trigger_tokens, 108_800);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, p);
            }
        }
    }

    // ── M8 P0b:压缩器配置(环境变量 + TOML)──────────────────

    #[test]
    fn compactor_config_default_when_no_env_no_toml() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV).ok();
        unsafe {
            std::env::remove_var(AUTO_COMPACT_INPUT_TOKENS_ENV);
        }
        let cfg = compactor_config_from_env_and_toml(None);
        assert_eq!(cfg.trigger_tokens, 10_000);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, p);
            }
        }
    }

    #[test]
    fn compactor_config_uses_toml_when_env_absent() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV).ok();
        unsafe {
            std::env::remove_var(AUTO_COMPACT_INPUT_TOKENS_ENV);
        }
        let cfg = compactor_config_from_env_and_toml(Some(5_000));
        assert_eq!(cfg.trigger_tokens, 5_000);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, p);
            }
        }
    }

    #[test]
    fn compactor_config_env_wins_over_toml() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV).ok();
        unsafe {
            std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, "200");
        }
        let cfg = compactor_config_from_env_and_toml(Some(99_999));
        assert_eq!(cfg.trigger_tokens, 200, "env must override TOML");
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, p);
            }
        } else {
            unsafe {
                std::env::remove_var(AUTO_COMPACT_INPUT_TOKENS_ENV);
            }
        }
    }

    #[test]
    fn compactor_config_env_malformed_falls_through_to_toml() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(AUTO_COMPACT_INPUT_TOKENS_ENV).ok();
        unsafe {
            std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, "not-a-number");
        }
        let cfg = compactor_config_from_env_and_toml(Some(7_777));
        assert_eq!(
            cfg.trigger_tokens, 7_777,
            "malformed env should fall through to TOML"
        );
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(AUTO_COMPACT_INPUT_TOKENS_ENV, p);
            }
        } else {
            unsafe {
                std::env::remove_var(AUTO_COMPACT_INPUT_TOKENS_ENV);
            }
        }
    }

    // ── v1.2 P1-12:token 预算 ─────────────────────────────────────

    /// `token_budget_from_env`:无 env 无 toml → `None`(默认不设预算)。
    #[test]
    fn token_budget_from_env_default_none() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(TOKEN_BUDGET_ENV).ok();
        unsafe {
            std::env::remove_var(TOKEN_BUDGET_ENV);
        }
        assert_eq!(token_budget_from_env(None), None);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(TOKEN_BUDGET_ENV, p);
            }
        }
    }

    /// `token_budget_from_env`:toml 有值、env 无 → 用 toml 值。
    #[test]
    fn token_budget_from_env_uses_toml_when_env_absent() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(TOKEN_BUDGET_ENV).ok();
        unsafe {
            std::env::remove_var(TOKEN_BUDGET_ENV);
        }
        assert_eq!(token_budget_from_env(Some(500_000)), Some(500_000));
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(TOKEN_BUDGET_ENV, p);
            }
        }
    }

    /// `token_budget_from_env`:env 优先于 toml。
    #[test]
    fn token_budget_from_env_wins_over_toml() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(TOKEN_BUDGET_ENV).ok();
        unsafe {
            std::env::set_var(TOKEN_BUDGET_ENV, "999");
        }
        assert_eq!(
            token_budget_from_env(Some(500_000)),
            Some(999),
            "env must override TOML"
        );
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(TOKEN_BUDGET_ENV, p);
            }
        } else {
            unsafe {
                std::env::remove_var(TOKEN_BUDGET_ENV);
            }
        }
    }

    /// `token_budget_from_env`:malformed env → 回退 toml(不报错)。
    #[test]
    fn token_budget_from_env_malformed_falls_through_to_toml() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(TOKEN_BUDGET_ENV).ok();
        unsafe {
            std::env::set_var(TOKEN_BUDGET_ENV, "not-a-number");
        }
        assert_eq!(token_budget_from_env(Some(7_777)), Some(7_777));
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(TOKEN_BUDGET_ENV, p);
            }
        } else {
            unsafe {
                std::env::remove_var(TOKEN_BUDGET_ENV);
            }
        }
    }

    // ── v1.x:最大迭代数(env 与 TOML 合成)─────────────────────

    /// `max_iterations_from_env`:无 env 无 toml → 默认 32。
    #[test]
    fn max_iterations_from_env_default() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(MAX_ITERATIONS_ENV).ok();
        unsafe {
            std::env::remove_var(MAX_ITERATIONS_ENV);
        }
        assert_eq!(max_iterations_from_env(None), DEFAULT_MAX_ITERATIONS);
        assert_eq!(max_iterations_from_env(None), 32);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(MAX_ITERATIONS_ENV, p);
            }
        }
    }

    /// `max_iterations_from_env`:toml 有值、env 无 → 用 toml 值。
    #[test]
    fn max_iterations_from_env_uses_toml_when_env_absent() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(MAX_ITERATIONS_ENV).ok();
        unsafe {
            std::env::remove_var(MAX_ITERATIONS_ENV);
        }
        assert_eq!(max_iterations_from_env(Some(8)), 8);
        assert_eq!(max_iterations_from_env(Some(100)), 100);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(MAX_ITERATIONS_ENV, p);
            }
        }
    }

    /// `max_iterations_from_env`:env 优先于 toml。
    #[test]
    fn max_iterations_from_env_wins_over_toml() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(MAX_ITERATIONS_ENV).ok();
        unsafe {
            std::env::set_var(MAX_ITERATIONS_ENV, "7");
        }
        assert_eq!(max_iterations_from_env(Some(99)), 7);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(MAX_ITERATIONS_ENV, p);
            }
        } else {
            unsafe {
                std::env::remove_var(MAX_ITERATIONS_ENV);
            }
        }
    }

    /// `max_iterations_from_env`:malformed env → 回退 toml(不报错)。
    #[test]
    fn max_iterations_from_env_malformed_falls_through_to_toml() {
        let _g = env_lock().lock().unwrap();
        let prior = std::env::var(MAX_ITERATIONS_ENV).ok();
        unsafe {
            std::env::set_var(MAX_ITERATIONS_ENV, "not-a-number");
        }
        assert_eq!(max_iterations_from_env(Some(42)), 42);
        if let Some(p) = prior {
            unsafe {
                std::env::set_var(MAX_ITERATIONS_ENV, p);
            }
        } else {
            unsafe {
                std::env::remove_var(MAX_ITERATIONS_ENV);
            }
        }
    }
}
