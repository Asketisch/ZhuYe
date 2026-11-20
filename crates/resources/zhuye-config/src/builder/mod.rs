//! `ReflectConfig → ModelRegistry` 转换。
//!
//! ## 模型组是唯一的接入单元
//!
//! v2 起配置里**只有模型组**(`[[model_group]]`):每个组自带协议端点
//! (`endpoint`)、凭据与默认模型,组 id 直接作为 `register_pool` 的池名
//! —— 模型 spec 前缀就是组 id(`<组id>/<模型>`)。
//!
//! ## 活跃组解析(`active_group`)
//!
//! 顺序:
//!
//! 1. env `ZHUYE_PROVIDER` —— 显式覆盖(v1 兼容面,保留);
//! 2. env `ZHUYE_MODEL` 的 mock 特判(`mock` / `mock/...`);
//! 3. TOML `[active].group`;
//! 4. env `ZHUYE_MODEL` 的前缀(是**已知组 id** 时);
//! 5. 第一个有凭据的组;
//! 6. `None`。
//!
//! ## model 解析(`resolve_model`)
//!
//! env `ZHUYE_MODEL` > 被钉住条目的 `model` > 组级 `model` > `None`。
//! 未显式配置时返回 `None` 而非编造内置默认 —— 请求打向第三方兼容端点
//! (stepfun / MiniMax 等)时,编造的官方模型名只会产生更难诊断的远端错误。

pub(crate) mod pools;
pub(crate) mod shapes;

use std::collections::HashSet;
use std::sync::Arc;

use zhuye_llm::{CredentialPool, MockClient, ModelRegistry, PoolEntry};

use crate::error::ConfigError;
use crate::schema::{ModelGroupSection, ReflectConfig, SpecSlotConfig};

pub(crate) use pools::{
    build_group_pool, build_subagent_anthropic_pool, build_subagent_ollama_pool,
    build_subagent_openai_pool, group_has_credential,
};
#[cfg(test)]
pub(crate) use shapes::DEFAULT_MCP_TIMEOUT_MS;
pub use shapes::{LspServerConfigShape, McpServerConfigShape};

pub(crate) const ENV_ZHUYE_PROVIDER: &str = "ZHUYE_PROVIDER";
pub(crate) const ENV_ZHUYE_MODEL: &str = "ZHUYE_MODEL";

impl ReflectConfig {
    /// 构造一个新的 `ModelRegistry`,把本配置中所有可用模型组注册进去。
    pub fn to_registry(&self) -> Result<ModelRegistry, ConfigError> {
        self.to_registry_with_env(
            std::env::var(ENV_ZHUYE_PROVIDER).ok().as_deref(),
            std::env::var(ENV_ZHUYE_MODEL).ok().as_deref(),
        )
    }

    /// [`to_registry`](Self::to_registry) 的 env 注入版(单测用;理由同
    /// [`Self::apply_to_registry_with_env`])。
    pub fn to_registry_with_env(
        &self,
        env_provider: Option<&str>,
        env_model: Option<&str>,
    ) -> Result<ModelRegistry, ConfigError> {
        let r = ModelRegistry::new();
        self.apply_to_registry_with_env(&r, env_provider, env_model)?;
        Ok(r)
    }

    /// 把本配置的模型组注册到已有 `ModelRegistry`(用于热重载)。
    /// 注意:`apply_to_registry` 不清空已有注册 —— 调用方负责决定何时 unregister。
    ///
    /// 池名 = 组 id;组内 `credentials` 数组非空 → 展开为 `CredentialPool`,
    /// 否则组级单值 `api_key` / `base_url` wrap 成 `label = "default"` /
    /// `weight = 1` 的单 entry pool。
    pub fn apply_to_registry(&self, registry: &ModelRegistry) -> Result<(), ConfigError> {
        self.apply_to_registry_with_env(
            registry,
            std::env::var(ENV_ZHUYE_PROVIDER).ok().as_deref(),
            std::env::var(ENV_ZHUYE_MODEL).ok().as_deref(),
        )
    }

    /// [`apply_to_registry`](Self::apply_to_registry) 的 env 注入版(单测
    /// 用:runner 门禁会设 `ZHUYE_MODEL=mock`,直接调 env 版的断言在该
    /// 环境下必假失败 —— 这些测试验证的是 TOML 配置行为而非 env 行为)。
    pub fn apply_to_registry_with_env(
        &self,
        registry: &ModelRegistry,
        env_provider: Option<&str>,
        env_model: Option<&str>,
    ) -> Result<(), ConfigError> {
        // 模型组池即将重建(可能换了 base_url / 凭据)→ 旧的模型清单缓存
        // 必须失效,否则新端点的清单会被旧端点的条目顶掉(TTL 内)。
        crate::discovery::clear_catalog_cache();
        // 模型组逐条注册,池名 = 组 id。非法 id(空 / 含 '/' / 保留名)
        // 与组间重复 id 都跳过并 warn,不让一个写错的组阻断其余配置
        // (坏值宽松吞掉的既有口径)。
        let mut seen_group_ids: HashSet<String> = HashSet::new();
        for group in &self.model_group {
            let id = group.id.trim();
            if id.is_empty() {
                tracing::warn!("模型组缺少 id,跳过该组");
                continue;
            }
            if !ModelGroupSection::is_valid_id(id) {
                tracing::warn!(group = %id, "模型组 id 非法(含 '/' 或与保留名冲突),跳过该组");
                continue;
            }
            if !seen_group_ids.insert(id.to_string()) {
                tracing::warn!(group = %id, "模型组 id 重复,跳过后出现的同名组");
                continue;
            }
            if let Some(pool) = build_group_pool(group)? {
                registry.register_pool(id, pool);
            }
        }
        // 离线 mock 组 —— `ZHUYE_PROVIDER=mock` 或 `ZHUYE_MODEL` 形如
        // `mock` / `mock/...` 时注册(免 key、零网络)。显式 opt-in,
        // 不作为无 key 时的静默兜底。
        if self.active_group_inner(env_provider, env_model).as_deref() == Some("mock") {
            registry.register_pool(
                "mock",
                CredentialPool {
                    entries: vec![PoolEntry {
                        client: Arc::new(MockClient::from_env()),
                        label: "default".into(),
                        weight: 1,
                    }],
                },
            );
        }
        // `[active].credential` 钉住 —— active 组池内同名条目健康则始终
        // 优先派位,其余条目只在该条目 cooldown 时作 failover。label 未
        // 命中池内条目时 registry 侧静默忽略(全池 round-robin),不让一个
        // 手写错的 label 阻断启动。
        if let (Some(group), Some(label)) = (
            self.active_group_inner(env_provider, env_model),
            self.active_credential(),
        ) {
            registry.set_preferred(&group, &label);
        }
        // 窗口覆盖表同步到 zhuye-llm 进程级全局表 —— bootstrap 与热重载
        // 共用本 choke point,让 `output_budget`(自适应输出预算)等
        // provider 侧消费方读到与引擎 `SessionConfigured` 一致的窗口口径。
        // 键在 llm 侧归一化。
        //
        // 用**合并表**(`[context_windows]` + `[model].context_window`)。
        // 模型 spec 用静态解析;`model = "free"` 这类运行时动态值由
        // headless 在 `resolve_model_spec` 后再同步一次(见 bootstrap_common)。
        let static_spec = self.resolved_model_spec().unwrap_or_default();
        zhuye_llm::set_context_window_overrides(
            &self.effective_context_window_overrides(&static_spec),
        );
        Ok(())
    }

    /// 选定的活跃模型组 id(= registry 池名);`None` 表示未配置任何可用组。
    pub fn active_group(&self) -> Option<String> {
        self.active_group_inner(
            std::env::var(ENV_ZHUYE_PROVIDER).ok().as_deref(),
            std::env::var(ENV_ZHUYE_MODEL).ok().as_deref(),
        )
    }

    /// `active_group` 的纯函数核心(env 值由调用方注入,便于单测并行
    /// 运行而不必改写进程 env)。
    fn active_group_inner(
        &self,
        env_provider: Option<&str>,
        env_model: Option<&str>,
    ) -> Option<String> {
        // 1. env `ZHUYE_PROVIDER` —— 显式覆盖(v1 兼容面):既认固定别名
        //    (claude / gpt / local),也直接认模型组 id。
        if let Some(p) = env_provider.map(str::trim).filter(|p| !p.is_empty()) {
            if let Some(name) = canonical_group(p) {
                return Some(name.to_string());
            }
            if self.group_id_exists(p) {
                return Some(p.to_string());
            }
        }
        // 2. `ZHUYE_MODEL` 以完整 spec 形式(`mock/mock-1`)或裸 `mock`
        //    显式指定 mock 时优先选中 —— 优先级在 TOML `[active]` 之前,
        //    让「用户明确要 mock」总能赢过配置文件里的真实组。
        if let Some(m) = env_model.map(str::trim) {
            if m == "mock" || m.starts_with("mock/") {
                return Some("mock".to_string());
            }
        }
        // 3. TOML `[active].group` —— 固定别名(claude / gpt / local)直接
        //    归一,自定义 id 必须是已声明的组;两者都不命中则继续往下兜底。
        if let Some(g) = self.active.group.as_deref().map(str::trim) {
            if let Some(name) = canonical_group(g) {
                return Some(name.to_string());
            }
            if self.group_id_exists(g) {
                return Some(g.to_string());
            }
        }
        // 4. env `ZHUYE_MODEL` 的前缀 —— 是**已知组 id** 时选中该组。
        if let Some(m) = env_model.map(str::trim) {
            if let Some((prefix, _)) = m.split_once('/')
                && self.group_id_exists(prefix)
            {
                return Some(prefix.to_string());
            }
        }
        // 5. 首个有可用凭据(id 合法)的组。
        self.model_group
            .iter()
            .find(|g| ModelGroupSection::is_valid_id(g.id.trim()) && group_has_credential(g))
            .map(|g| g.id.trim().to_string())
    }

    /// 给定名字是否是已声明的模型组 id(trim 后精确匹配)。
    pub(crate) fn group_id_exists(&self, raw: &str) -> bool {
        let trimmed = raw.trim();
        !trimmed.is_empty() && self.model_group.iter().any(|g| g.id.trim() == trimmed)
    }

    /// `[active].credential` —— 钉住活跃组凭证池中的条目 label。
    /// `None` = 未钉住(全池加权 round-robin)。组级 `api_key` 隐式条目的
    /// label 固定为 `"default"`,钉它就显式写 `credential = "default"`。
    pub fn active_credential(&self) -> Option<String> {
        self.active
            .credential
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    /// 给定模型组显式解析 model:env `ZHUYE_MODEL` > 被钉住条目的
    /// `model` > 组级 `model`。
    ///
    /// `None` = 未显式配置任何 model。调用方应如实上报缺失(报错 /
    /// UI 显示"未配置"),而非回落内置默认 —— 默认模型名打向第三方
    /// 兼容端点只会得到更难诊断的远端错误。
    pub fn resolve_model(&self, group: &str) -> Option<String> {
        self.resolve_model_inner(
            group,
            std::env::var(ENV_ZHUYE_PROVIDER).ok().as_deref(),
            std::env::var(ENV_ZHUYE_MODEL).ok().as_deref(),
        )
    }

    /// `resolve_model` 的纯函数核心(env 值由调用方注入,便于单测)。
    fn resolve_model_inner(
        &self,
        group: &str,
        env_provider: Option<&str>,
        env_model: Option<&str>,
    ) -> Option<String> {
        // mock 组的 model 取 `mock/` 前缀后的部分(`ZHUYE_MODEL=mock/mock-1`
        // → `mock-1`),裸 `mock` / 未设 → `mock-1`。不能走下面的通用 env
        // 分支,否则会把完整 spec(`mock/mock-1`)原样当 model 名拼出
        // `mock/mock/mock-1`。mock 是显式 opt-in 的离线组,内置 model 名
        // 属于其契约的一部分,不属于"编造默认"。
        if group == "mock" {
            if let Some(m) = env_model.map(str::trim) {
                if let Some(stripped) = m.strip_prefix("mock/") {
                    if !stripped.is_empty() {
                        return Some(stripped.to_string());
                    }
                }
            }
            return Some(zhuye_llm::DEFAULT_MOCK_MODEL.to_string());
        }
        if let Some(m) = env_model.map(str::trim).filter(|m| !m.is_empty()) {
            // env 值带**已知组前缀**(`openai/m1`、`mygroup/claude-x`)时剥掉
            // 前缀,只把 model 段发给网关(多数网关对带前缀的 model 名
            // 404/400)。只剥**配置里声明过的组 id** 与 `mock` ——
            // 用户真实模型名形如 `spark-2/qwen3.8-27b-nvfp4`,前缀不是组名
            // 时必须原样保留。
            return Some(self.strip_group_prefix(m).to_string());
        }
        // 被钉住的凭证条目自带 model 时优先取用:它比组级覆盖更具体
        // (描述的就是当前 plan 本身);钉住只对活跃组生效。
        if self.active_group_inner(env_provider, env_model).as_deref() == Some(group)
            && let Some(label) = self.active_credential()
        {
            let entry_model = self
                .credentials_of(group)
                .and_then(|creds| creds.iter().find(|c| c.label == label))
                .and_then(|c| c.model.clone());
            if let Some(m) = entry_model.filter(|m| !m.trim().is_empty()) {
                return Some(m);
            }
        }
        let group_override = self.group_model_of(group);
        if let Some(m) = group_override.filter(|m| !m.trim().is_empty()) {
            return Some(m);
        }
        None
    }

    /// 模型组的 `credentials` 数组(单凭据组返回空数组,走组级 model 覆盖)。
    fn credentials_of(&self, group: &str) -> Option<&Vec<crate::schema::CredentialConfig>> {
        self.model_group
            .iter()
            .find(|g| g.id.trim() == group)
            .map(|g| &g.credentials)
    }

    /// 模型组的组级 `model` 覆盖。
    fn group_model_of(&self, group: &str) -> Option<String> {
        self.model_group
            .iter()
            .find(|g| g.id.trim() == group)
            .and_then(|g| g.model.clone())
    }

    /// 剥掉 model 值上的**已知组前缀**。
    ///
    /// 只剥配置里声明过的组 id 与 `mock`;其余(`cohere/x:free`、
    /// `spark-2/qwen3.8-27b-nvfp4` 这类厂商命名)原样返回。
    fn strip_group_prefix<'a>(&self, model: &'a str) -> &'a str {
        if let Some((prefix, rest)) = model.split_once('/')
            && !rest.is_empty()
            && (prefix == "mock" || self.group_id_exists(prefix))
        {
            return rest;
        }
        model
    }

    /// 当前活跃组的完整 spec(`"<组id>/<model>"`)。
    /// `None` = 未配置模型组,或组已配置但没有任何显式 model
    /// (env / 被钉住条目 / 组级都没有)—— 调用方据此如实显示"未配置
    /// 模型",而不是展示一个编造出来的默认模型。
    ///
    /// 用途:`zhuye-exec::handle_reload` 拿到 `old_cfg` 与 `new_cfg` 后,
    /// 对比两侧 `resolved_model_spec()` 的差异决定是否更新
    /// `AgentConfig.model`。集中在这里便于单测覆盖 env > 钉住条目 >
    /// 组级优先级和组切换场景。
    pub fn resolved_model_spec(&self) -> Option<String> {
        self.resolved_model_spec_with_env(
            std::env::var(ENV_ZHUYE_PROVIDER).ok().as_deref(),
            std::env::var(ENV_ZHUYE_MODEL).ok().as_deref(),
        )
    }

    /// [`resolved_model_spec`](Self::resolved_model_spec) 的 env 注入版
    /// (单测用;理由同 [`Self::apply_to_registry_with_env`])。
    pub fn resolved_model_spec_with_env(
        &self,
        env_provider: Option<&str>,
        env_model: Option<&str>,
    ) -> Option<String> {
        let group = self.active_group_inner(env_provider, env_model)?;
        let model = self.resolve_model_inner(&group, env_provider, env_model)?;
        Some(format!("{group}/{model}"))
    }

    /// 从 `[routing]` 段构建 `RoutingPolicy`。
    ///
    /// 缺省规则:整个 `[routing]` 段缺省 → `RoutingPolicy::default()`(所有
    /// slot 的 primary 为空,`zhuye-exec::bootstrap_m4` 用
    /// `cfg.resolved_model_spec()` 兜底 main slot)。
    /// 各 slot 的 `primary` 缺省时,优先用该 slot 的 env 变量:
    /// `ZHUYE_COMPACT_MODEL` / `ZHUYE_SUBAGENT_MODEL`,否则兜底
    /// active spec。
    ///
    /// ⚠️ 本方法用的是**同步**的 `resolved_model_spec()`。当配置里的 model
    /// 是 `free` 这类「运行时才知道」的特殊值时,同步解析拿到的仍是占位串
    /// (`"openai/free"`),而 `model_call` 的 spec 优先取 routing 的 main
    /// slot primary —— 占位串会盖掉已解析出的真实 id。**动态 model 场景
    /// 必须用 [`Self::routing_policy_with`] 传入解析后的 spec。**
    pub fn routing_policy(&self) -> zhuye_llm::RoutingPolicy {
        let active_spec = self.resolved_model_spec().unwrap_or_default();
        self.routing_policy_with(&active_spec)
    }

    /// 同 [`Self::routing_policy`],但 active spec 由调用方传入。
    ///
    /// 用于「model 需要运行时解析」的场景:调用方先解析出真实模型 id,再
    /// 用它构建 policy,保证 routing 的 main slot 与实际请求一致。
    pub fn routing_policy_with(&self, active_spec: &str) -> zhuye_llm::RoutingPolicy {
        use zhuye_llm::{Role, RoutingPolicy, SpecSlot};
        let active_spec = active_spec.to_string();

        let Some(section) = &self.routing else {
            return RoutingPolicy {
                main: SpecSlot::with_primary(active_spec),
                ..RoutingPolicy::default()
            };
        };

        fn build_slot(slot: &SpecSlotConfig, env_var: &str, active_spec: &str) -> SpecSlot {
            let primary = slot
                .primary
                .clone()
                .or_else(|| std::env::var(env_var).ok().filter(|s| !s.is_empty()))
                .unwrap_or_else(|| active_spec.to_string());
            // weights 与 fallbacks 长度不匹配时用全 1
            let weights = if slot.weights.len() == slot.fallbacks.len() {
                slot.weights.clone()
            } else {
                vec![1; slot.fallbacks.len()]
            };
            SpecSlot {
                primary,
                fallbacks: slot.fallbacks.clone(),
                weights,
            }
        }

        // 单轮重试上限可配 —— 缺省沿用 `RoutingPolicy::default()` 的 16,
        // `[routing] max_attempts` 覆盖。经 `with_policy` 流入
        // `model_call` 的重试环。
        let max_attempts = section
            .max_attempts
            .unwrap_or(RoutingPolicy::default().max_attempts);
        RoutingPolicy {
            main: build_slot(&section.main, "ZHUYE_MAIN_MODEL", &active_spec),
            compact: build_slot(&section.compact, "ZHUYE_COMPACT_MODEL", &active_spec),
            subagent: build_slot(&section.subagent, "ZHUYE_SUBAGENT_MODEL", &active_spec),
            max_attempts,
            ..RoutingPolicy::default()
        }
        // 抑制 Role 导入的 unused 警告(供 Phase 3 caller 用)
        .with_role(Role::Main)
    }

    /// 从 `[subagent_providers]` 段构建独立的 `ModelRegistry`。
    ///
    /// - 若整个 section 缺省 → 返回 `None`,保持默认行为(父子共享 registry)。
    /// - 若任意子 provider 有可用凭证 → 构建并注册到 child registry,返回 `Some(reg)`。
    /// - 所有子 provider 都无凭证 → 返回 `None`。
    pub fn to_child_registry(&self) -> Option<ModelRegistry> {
        let section = self.subagent_providers.as_ref()?;

        let r = ModelRegistry::new();
        let mut any_registered = false;

        if let Some(anth) = &section.anthropic {
            if let Ok(Some(pool)) = build_subagent_anthropic_pool(anth) {
                r.register_pool("anthropic", pool);
                any_registered = true;
            }
        }
        if let Some(oai) = &section.openai {
            if let Ok(Some(pool)) = build_subagent_openai_pool(oai) {
                r.register_pool("openai", pool);
                any_registered = true;
            }
        }
        if let Some(ollama) = &section.ollama {
            if let Ok(Some(pool)) = build_subagent_ollama_pool(ollama) {
                r.register_pool("ollama", pool);
                any_registered = true;
            }
        }

        if any_registered { Some(r) } else { None }
    }

    /// 把 `[mcp_servers.*]` 段编译为强类型列表。
    ///
    /// 任一 entry 校验失败 → 整函数返回 `Err`,由 caller 决定是否 fail-fast
    /// (`zhuye-exec::handle_reload` 选择 warn + 跳过单 server,
    /// `bootstrap_m6` 选择 warn + 不启动)。
    pub fn mcp_server_configs(&self) -> Result<Vec<McpServerConfigShape>, ConfigError> {
        let mut out = Vec::with_capacity(self.mcp_servers.servers.len());
        for (name, entry) in &self.mcp_servers.servers {
            out.push(McpServerConfigShape::from_entry(name.clone(), entry)?);
        }
        // HashMap 顺序不稳定,按 server 名排序保证 reload diff 的可重现性。
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// 把 `[lsp_servers.*]` 段编译为强类型列表。
    ///
    /// 校验规则:每个 entry 必须 `file_patterns` 非空(否则 server 不知道
    /// 自己要管哪些文件,无意义);`command` 已由 schema 强制必填。
    ///
    /// 错误路径任一 entry 失败 → 整函数 `Err`,caller 决定 warn + 跳过。
    pub fn lsp_server_configs(&self) -> Result<Vec<LspServerConfigShape>, ConfigError> {
        let mut out = Vec::with_capacity(self.lsp_servers.servers.len());
        for (name, entry) in &self.lsp_servers.servers {
            out.push(LspServerConfigShape::from_entry(name.clone(), entry)?);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
}

// v1.0 多 Provider 路由:`RoutingPolicy::with_role` 内部辅助,避免
// 在 `routing_policy()` 末尾孤悬一个 Role import。
trait RoutingPolicyExt {
    fn with_role(self, _r: zhuye_llm::Role) -> Self;
}
impl RoutingPolicyExt for zhuye_llm::RoutingPolicy {
    fn with_role(self, _r: zhuye_llm::Role) -> Self {
        self
    }
}

/// 固定组名的别名归一(v1 的 provider 别名仍然好用)。
///
/// 只用于**已知组名**的判定 —— 别名映射到 `anthropic` / `openai` /
/// `ollama` / `mock`,而这些名字是否是真正的组由调用方用
/// [`ReflectConfig::group_id_exists`] 复核。
fn canonical_group(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => Some("anthropic"),
        "openai" | "gpt" => Some("openai"),
        "ollama" | "local" => Some("ollama"),
        // 离线 mock 组(e2e / SDK 集成测试用)。
        "mock" | "fake" => Some("mock"),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
