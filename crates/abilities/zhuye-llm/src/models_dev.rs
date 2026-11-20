//! `models_dev` — models.dev 社区模型目录(窗口 / 输出上限 / 视觉 / 思考档位)。
//!
//! ## 为什么需要它
//!
//! provider 清单端点(尤其 Anthropic 官方)披露的能力信息很有限,内置
//! 静态表只覆盖主线模型族,新模型 / 私端模型在「上下文窗口、输出上限、
//! 视觉输入、思考档位」四个维度上长期是 `None`(引擎只能退回保守默认)。
//! models.dev 是 CC0 社区目录,按模型聚合了各家厂商的上述数据;本模块
//! 借鉴 magpie 的做法把它做成引擎的**第二数据源**:
//!
//! - **跨厂商投票**:同一 bare id 出现在多个 provider 名下时,窗口 /
//!   输出取众数(平票取字典序最小,确定性),vision / reasoning 按多数
//!   表决,档位集合按完整相等计数取众数;
//! - **窗口口径**:`min(limit.input, limit.context)`(input 优先、context
//!   封顶),小于 [`LEAST_WINDOW`] 视为未知(防个别数据源的假窗口);
//! - **兜底链**:归一化精确命中 → 变体拼写(点 / 横线互换,两侧归一到
//!   只留字母数字)→ 名字后缀启发(`-128k` / `-1m` / ollama `name:tag`)。
//!
//! ## 数据来源与级联位置
//!
//! 进程内全局目录懒加载:磁盘缓存(`$ZHUYE_HOME > $HOME/.zhuye` 下的
//! `cache/models-dev.json`)与**内嵌快照**(构建期 `scripts/fetch-models-dev.sh`
//! 产出)取 `fetched_at` 较新者;[`refresh_if_stale`] 由调用方(桌面清单
//! 命令 / CLI)显式触发,**引擎启动不主动联网**。
//!
//! 消费级联(用户显式配置永远压过目录):
//! - 窗口:`context_window_for` = override → **本目录** → 静态表;
//! - 输出上限:`output_budget::model_output_limit` = **本目录** → 精确表;
//! - 思考档位:`reasoning_effort::max_effort_for_model` = 静态表 →
//!   **本目录档位映射** → 保守默认(静态表编码了本引擎 wire 词表决策,
//!   权威优先;目录只补未知模型);
//! - provider 清单条目:`enrich_model_info` 用目录补 `None` 字段
//!   (live 披露优先,目录只填空缺,与 magpie Decorate 同语义)。

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::model_catalog::ModelInfo;
use crate::providers::normalize_model_spec;

/// models.dev 目录 API 地址。
pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";
/// 磁盘缓存新鲜期(秒):窗口内不重复刷新(24h,与 magpie staleAfter 同口径)。
pub const CACHE_TTL_SECS: u64 = 24 * 60 * 60;
/// 窗口低于此值视为未知(个别数据源会给出 512 这类假窗口)。
pub const LEAST_WINDOW: u32 = 2_048;
/// 单次下载体积上限。
const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;
/// 刷新请求超时(秒)。
const FETCH_TIMEOUT_SECS: u64 = 30;

/// 内嵌快照(原始 api.json 的 gzip)。口径与运行时拉取完全一致,聚合
/// 都在加载期完成 —— 快照与缓存共用 [`DevCatalog::parse_api`] 一条
/// 解析路径,不存在格式漂移。
const SNAPSHOT_GZ: &[u8] = include_bytes!("../assets/api.json.gz");

/// 快照抓取时刻(unix 秒;meta 缺失 / 解析失败 → 0)。
fn snapshot_at() -> i64 {
    static AT: OnceLock<i64> = OnceLock::new();
    *AT.get_or_init(|| {
        include_str!("../assets/api.json.gz.meta")
            .trim()
            .parse()
            .unwrap_or(0)
    })
}

/// 一个模型在四个能力维度上的目录结论(全部可选:目录没说就是 `None`)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelAbilities {
    /// 上下文窗口(token)。
    pub context_window: Option<u32>,
    /// 最大输出 token。
    pub max_output_tokens: Option<u32>,
    /// 是否接受图片输入。
    pub supports_vision: Option<bool>,
    /// 是否具备思考能力(models.dev `reasoning` 字段投票)。
    pub supports_reasoning: Option<bool>,
    /// 思考档位菜单(models.dev `reasoning_options` type=effort 的 values 投票,
    /// 原样保留厂商词表,如 `["low","medium","high"]`)。
    pub effort_levels: Option<Vec<String>>,
}

/// models.dev 目录的可查询形态:归一化 bare id → 能力(含变体拼写别名索引)。
#[derive(Debug, Clone, Default)]
pub struct DevCatalog {
    entries: HashMap<String, ModelAbilities>,
    /// 变体拼写别名(只留字母数字的归一形态 → 规范 key),如
    /// `gemini-2-5-pro` → `gemini-2.5-pro`。仅兜底用,冲突取首见(确定性)。
    aliases: HashMap<String, String>,
}

/// api.json 中单个模型对象的形状(字段全可选,容错解析)。
#[derive(Deserialize)]
struct ApiModel {
    limit: Option<ApiLimit>,
    modalities: Option<ApiModalities>,
    reasoning: Option<bool>,
    #[serde(default, rename = "reasoning_options")]
    reasoning_options: Option<Vec<ApiReasoningOption>>,
}

#[derive(Deserialize)]
struct ApiLimit {
    context: Option<u64>,
    input: Option<u64>,
    output: Option<u64>,
}

#[derive(Deserialize)]
struct ApiModalities {
    input: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct ApiReasoningOption {
    #[serde(rename = "type")]
    kind: Option<String>,
    values: Option<Vec<String>>,
}

/// 同一 bare id 的多源投票累积器。
#[derive(Default)]
struct Vote {
    windows: Vec<u32>,
    outputs: Vec<u32>,
    vision_true: usize,
    vision_false: usize,
    thinks_true: usize,
    thinks_false: usize,
    /// 完整档位集合(按源收集,取众数)。
    level_sets: Vec<Vec<String>>,
}

impl DevCatalog {
    /// 解析原始 api.json(与内嵌快照、磁盘缓存、运行时拉取同一路径)。
    pub fn parse_api(bytes: &[u8]) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| format!("api.json parse failed: {e}"))?;
        Self::parse_api_value(&value)
    }

    /// 从已解析的 JSON 值构建目录(磁盘缓存 envelope 复用)。
    pub fn parse_api_value(value: &serde_json::Value) -> Result<Self, String> {
        let root = value
            .as_object()
            .ok_or_else(|| "api.json root is not an object".to_string())?;
        let mut votes: HashMap<String, Vote> = HashMap::new();
        for (_provider, provider_value) in root {
            // 容错:顶层值不是 provider 形状(无 models 对象)就跳过。
            let Ok(provider) = serde_json::from_value::<HashMap<String, serde_json::Value>>(
                provider_value.clone(),
            ) else {
                continue;
            };
            let Some(models) = provider.get("models").cloned() else {
                continue;
            };
            let Ok(models) = serde_json::from_value::<HashMap<String, ApiModel>>(models) else {
                continue;
            };
            for (_id, m) in models {
                let key = normalize_model_spec(&_id);
                if key.is_empty() {
                    continue;
                }
                let vote = votes.entry(key).or_default();
                if let Some(w) = api_window(&m) {
                    vote.windows.push(w);
                }
                if let Some(o) = m.limit.as_ref().and_then(|l| l.output) {
                    if o > 0 && o <= u32::MAX as u64 {
                        vote.outputs.push(o as u32);
                    }
                }
                match api_has_image(&m) {
                    Some(true) => vote.vision_true += 1,
                    Some(false) => vote.vision_false += 1,
                    None => {}
                }
                match m.reasoning {
                    Some(true) => vote.thinks_true += 1,
                    Some(false) => vote.thinks_false += 1,
                    None => {}
                }
                if let Some(levels) = api_effort_levels(&m) {
                    vote.level_sets.push(levels);
                }
            }
        }
        let entries: HashMap<String, ModelAbilities> = votes
            .into_iter()
            .map(|(key, v)| (key, v.reduce()))
            .collect();
        Ok(Self::from_entries(entries))
    }

    /// 由已聚合条目构建(构建别名索引;测试与装配入口复用)。
    pub fn from_entries(entries: impl IntoIterator<Item = (String, ModelAbilities)>) -> Self {
        let mut catalog = Self {
            entries: entries.into_iter().collect(),
            aliases: HashMap::new(),
        };
        let keys: Vec<String> = catalog.entries.keys().cloned().collect();
        for key in keys {
            let alias: String = alnum_key(&key);
            if alias != key && !alias.is_empty() {
                catalog.aliases.entry(alias).or_insert(key);
            }
        }
        catalog
    }

    /// 条目数(空目录退化为「全部未知」,行为与无目录时一致)。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 目录是否为空。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 兜底链查询:归一化精确 → 变体拼写别名 → 名字后缀启发。
    pub fn get(&self, model_spec: &str) -> ModelAbilities {
        let key = normalize_model_spec(model_spec);
        if let Some(a) = self.entries.get(&key) {
            return a.clone();
        }
        let alias = alnum_key(&key);
        if !alias.is_empty() {
            if let Some(canon) = self.aliases.get(&alias) {
                if let Some(a) = self.entries.get(canon) {
                    return a.clone();
                }
            }
        }
        match suffix_window(model_spec) {
            Some(w) => ModelAbilities {
                context_window: Some(w),
                ..Default::default()
            },
            None => ModelAbilities::default(),
        }
    }
}

/// 窗口口径:input 优先、context 封顶,低于 [`LEAST_WINDOW`] 视为未知。
fn api_window(m: &ApiModel) -> Option<u32> {
    let limit = m.limit.as_ref()?;
    let w = match (limit.input, limit.context) {
        (Some(i), Some(c)) => i.min(c),
        (Some(i), None) => i,
        (None, Some(c)) => c,
        (None, None) => return None,
    };
    (w >= LEAST_WINDOW as u64 && w <= u32::MAX as u64).then_some(w as u32)
}

/// 输入模态含 `image` → 具备视觉(大小写不敏感)。
fn api_has_image(m: &ApiModel) -> Option<bool> {
    let inputs = m.modalities.as_ref()?.input.as_ref()?;
    Some(inputs.iter().any(|s| s.eq_ignore_ascii_case("image")))
}

/// 思考档位菜单:`reasoning_options` 中 type=effort 的 values(取首个)。
fn api_effort_levels(m: &ApiModel) -> Option<Vec<String>> {
    let levels = m
        .reasoning_options
        .as_ref()?
        .iter()
        .filter(|o| o.kind.as_deref() == Some("effort"))
        .filter_map(|o| o.values.clone())
        .find(|v| !v.is_empty())?;
    Some(levels)
}

/// 只留字母数字的归一形态(变体拼写比对的公共语言)。
fn alnum_key(key: &str) -> String {
    key.chars().filter(|c| c.is_alphanumeric()).collect()
}

/// 数值众数,平票取字典序最小(排序后先见者胜,确定性且保守)。
fn mode_min_tie(mut values: Vec<u32>) -> Option<u32> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let mut best = (values[0], 0usize);
    let mut cur = (values[0], 0usize);
    for &v in &values {
        if v == cur.0 {
            cur.1 += 1;
        } else {
            cur = (v, 1);
        }
        if cur.1 > best.1 {
            best = cur;
        }
    }
    Some(best.0)
}

/// 布尔多数表决(平票 / 零票 → `None`)。
fn vote_bool(true_n: usize, false_n: usize) -> Option<bool> {
    match true_n.cmp(&false_n) {
        std::cmp::Ordering::Greater => Some(true),
        std::cmp::Ordering::Less => Some(false),
        std::cmp::Ordering::Equal => None,
    }
}

/// 档位集合众数(按完整相等计数,平票取字典序最小)。
fn mode_set(mut sets: Vec<Vec<String>>) -> Option<Vec<String>> {
    if sets.is_empty() {
        return None;
    }
    sets.sort();
    let mut best = 0usize;
    let mut best_set = sets[0].clone();
    let mut cur = 0usize;
    for (i, s) in sets.iter().enumerate() {
        if i > 0 && s == &sets[i - 1] {
            cur += 1;
        } else {
            cur = 1;
        }
        if cur > best {
            best = cur;
            best_set = s.clone();
        }
    }
    Some(best_set)
}

impl Vote {
    /// 多源投票出最终能力结论。
    fn reduce(self) -> ModelAbilities {
        ModelAbilities {
            context_window: mode_min_tie(self.windows),
            max_output_tokens: mode_min_tie(self.outputs),
            supports_vision: vote_bool(self.vision_true, self.vision_false),
            supports_reasoning: vote_bool(self.thinks_true, self.thinks_false),
            effort_levels: mode_set(self.level_sets),
        }
    }
}

/// 名字后缀启发:`xxx-128k` / `xxx-1m` / `xxx-1.5m`,以及 ollama 的
/// `name:128k` tag(在归一化去 tag 之前先看原始 tag)。
fn suffix_window(model_spec: &str) -> Option<u32> {
    if let Some((_, tag)) = model_spec.rsplit_once(':') {
        if let Some(w) = parse_size_token(tag.trim()) {
            return Some(w);
        }
    }
    let key = normalize_model_spec(model_spec);
    let last = key.rsplit('-').next()?;
    parse_size_token(last)
}

/// 解析 `128k` / `1m` / `1.5m` 形态的窗口标注(k/m 均按二进制 1024 进位)。
fn parse_size_token(token: &str) -> Option<u32> {
    let (num, unit) = token.split_at(token.len().checked_sub(1)?);
    match unit {
        "k" | "K" => num
            .parse::<u32>()
            .ok()
            .map(|n| n.saturating_mul(1024))
            .filter(|w| *w >= LEAST_WINDOW),
        "m" | "M" => {
            let f = num.parse::<f64>().ok()?;
            (f.is_finite() && f > 0.0 && f <= 1024.0).then_some((f * 1_048_576.0) as u32)
        }
        _ => None,
    }
}

// ── 进程级全局目录 ───────────────────────────────────────────────────────

/// 磁盘缓存 envelope:`{ fetched_at, api }`(api 为原始 api.json 值)。
#[derive(Serialize, Deserialize)]
struct CacheEnvelope {
    fetched_at: i64,
    api: serde_json::Value,
}

/// 只取 fetched_at 的轻量解析(不必解析整个 api 值)。
#[derive(Deserialize)]
struct CacheStamp {
    #[serde(default)]
    fetched_at: i64,
}

fn cache_file(dir: &Path) -> PathBuf {
    dir.join("cache").join("models-dev.json")
}

fn global_catalog() -> &'static RwLock<DevCatalog> {
    static GLOBAL: OnceLock<RwLock<DevCatalog>> = OnceLock::new();
    GLOBAL.get_or_init(|| RwLock::new(load_initial()))
}

/// 懒加载初值:磁盘缓存与内嵌快照取 `fetched_at` 较新者。
fn load_initial() -> DevCatalog {
    let disk = zhuye_fs::data_dir().and_then(|dir| read_cache(&cache_file(&dir)));
    match disk {
        Some((at, catalog)) if at >= snapshot_at() && !catalog.is_empty() => catalog,
        _ => DevCatalog::parse_api(&decode_snapshot()).unwrap_or_default(),
    }
}

fn decode_snapshot() -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(SNAPSHOT_GZ)
        .read_to_end(&mut out)
        .unwrap_or_default();
    out
}

fn read_cache(path: &Path) -> Option<(i64, DevCatalog)> {
    let bytes = std::fs::read(path).ok()?;
    let env: CacheEnvelope = serde_json::from_slice(&bytes).ok()?;
    let catalog = DevCatalog::parse_api_value(&env.api).ok()?;
    Some((env.fetched_at, catalog))
}

/// 测试钩子:持目录测试锁,把进程内目录恢复为**内嵌快照**,再运行
/// `f`。其他 crate 的单测(reasoning_effort 等)会向这个进程级全局装
/// 极简 fixture 且不恢复 —— 桌面侧断言真实目录的测试若与之并发
/// (Windows 调度实证),会读到 fixture 而误报;持锁后互斥,恢复后
/// 断言与执行顺序无关。
pub fn with_snapshot_catalog<R>(f: impl FnOnce() -> R) -> R {
    let _guard = catalog_test_lock().lock().unwrap();
    *global_catalog().write() = DevCatalog::parse_api(&decode_snapshot()).unwrap_or_default();
    f()
}

/// 测试钩子:持目录测试锁并安装 `catalog`,再运行 `f` —— 与
/// [`with_snapshot_catalog`] 及其他持锁测试互斥,fixture 不再被并发
/// 重置(进程内并发测试互相覆盖的同一竞态,Windows 调度实证)。
pub fn with_test_catalog<R>(catalog: DevCatalog, f: impl FnOnce() -> R) -> R {
    let _guard = catalog_test_lock().lock().unwrap();
    *global_catalog().write() = catalog;
    f()
}

/// 查 `model_spec` 的目录能力(进程级懒加载;空目录 = 全未知)。
pub fn abilities_for(model_spec: &str) -> ModelAbilities {
    global_catalog().read().get(model_spec)
}

/// 装配 / 测试钩子:整体替换进程内目录。常规调用方无需使用。
pub fn install_catalog(catalog: DevCatalog) {
    *global_catalog().write() = catalog;
}

/// 拉取 models.dev → 落盘缓存 → 换进程内目录。
///
/// 落盘失败(目录只读等)只降级为「内存换新 + 警告日志」,不报错 ——
/// 刷新失败不该阻断清单命令。
pub async fn refresh() -> Result<DevCatalog, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let resp = client
        .get(MODELS_DEV_URL)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("network error: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_DOWNLOAD_BYTES)
    {
        return Err("api.json exceeds size cap".into());
    }
    let bytes = resp.bytes().await.map_err(|e| format!("body: {e}"))?;
    if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
        return Err("api.json exceeds size cap".into());
    }
    let catalog = DevCatalog::parse_api(&bytes)?;
    let fetched_at = chrono::Utc::now().timestamp();
    if let Some(dir) = zhuye_fs::data_dir() {
        let path = cache_file(&dir);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|e| e.to_string())
            .and_then(|api| {
                serde_json::to_vec(&CacheEnvelope { fetched_at, api }).map_err(|e| e.to_string())
            }) {
            Ok(payload) => {
                if let Err(e) = zhuye_fs::write_atomic(&path, &payload) {
                    tracing::warn!("models.dev 缓存落盘失败({path:?}):{e}");
                }
            }
            Err(e) => tracing::warn!("models.dev 缓存序列化失败:{e}"),
        }
    }
    install_catalog(catalog.clone());
    Ok(catalog)
}

/// 缓存过期(超过 [`CACHE_TTL_SECS`])才刷新;新鲜返回 `Ok(false)`。
/// 无磁盘缓存时以快照时刻起算。
pub async fn refresh_if_stale() -> Result<bool, String> {
    let disk_at = zhuye_fs::data_dir()
        .and_then(|dir| {
            std::fs::read(cache_file(&dir))
                .ok()
                .and_then(|b| serde_json::from_slice::<CacheStamp>(&b).ok())
        })
        .map_or_else(snapshot_at, |s| s.fetched_at);
    let age = chrono::Utc::now().timestamp() - disk_at;
    if age >= 0 && age < CACHE_TTL_SECS as i64 {
        return Ok(false);
    }
    refresh().await.map(|_| true)
}

/// 用目录补齐 provider 清单条目的空缺字段(live 披露优先,目录只填 `None`)。
pub fn enrich_model_info(info: &mut ModelInfo) {
    let abilities = abilities_for(&info.id);
    if info.context_window.is_none() {
        info.context_window = abilities.context_window;
    }
    if info.max_output_tokens.is_none() {
        info.max_output_tokens = abilities.max_output_tokens;
    }
    if info.supports_vision.is_none() {
        info.supports_vision = abilities.supports_vision;
    }
    if info.efforts.is_none() {
        info.efforts = abilities.effort_levels;
    }
}

/// 测试辅助:目录全局是进程级共享,所有 install_catalog 的单测
/// (本模块与 reasoning_effort)用这把锁互斥,避免并行用例互相覆盖
/// (与 `context_window::override_test_lock` 同一模式)。
/// 不再 cfg(test):`with_snapshot_catalog` 测试钩子在依赖构建
/// (桌面端单测)下也要持这把锁。
pub(crate) fn catalog_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(body: &str) -> DevCatalog {
        DevCatalog::parse_api(body.as_bytes()).expect("fixture must parse")
    }

    /// 跨厂商投票:窗口 / 输出取众数,vision / reasoning 多数表决,
    /// 档位集合取众数;窗口口径 input 优先 context 封顶。
    #[test]
    fn votes_across_providers() {
        let catalog = api(r#"{
              "openai": { "models": {
                "m1": { "reasoning": true,
                        "reasoning_options": [ {"type":"effort","values":["low","medium","high"]} ],
                        "modalities": { "input": ["text","image"] },
                        "limit": { "context": 128000, "input": 127999, "output": 16384 } } } },
              "mirror": { "models": {
                "m1": { "reasoning": true,
                        "reasoning_options": [ {"type":"effort","values":["low","medium","high"]} ],
                        "modalities": { "input": ["text"] },
                        "limit": { "context": 100000, "output": 8192 } },
                "m2": { "reasoning": false, "limit": { "context": 512 } } } }
            }"#);
        let m1 = catalog.get("openai/m1");
        // 窗口 100000 一票 vs 127999 一票 → 平票取小;vision 1:1 平票 → None;
        // reasoning 2:0 → true;档位两源一致 → 保留。
        assert_eq!(m1.context_window, Some(100_000));
        assert_eq!(
            m1.max_output_tokens,
            Some(8_192),
            "输出 16384 vs 8192 平票取小"
        );
        assert_eq!(m1.supports_vision, None);
        assert_eq!(m1.supports_reasoning, Some(true));
        assert_eq!(
            m1.effort_levels,
            Some(vec!["low".into(), "medium".into(), "high".into()])
        );
        // 假窗口(< LEAST_WINDOW)不计入 → 全未知。
        let m2 = catalog.get("m2");
        assert_eq!(m2.context_window, None);
        assert_eq!(m2.supports_reasoning, Some(false));
    }

    /// 兜底链:变体拼写(点 / 横线互换)→ 名字后缀启发。
    #[test]
    fn fallback_chain() {
        let catalog = DevCatalog::from_entries([(
            "gemini-2.5-pro".to_string(),
            ModelAbilities {
                context_window: Some(1_048_576),
                supports_vision: Some(true),
                ..Default::default()
            },
        )]);
        // 精确(带 provider 前缀归一)。
        assert_eq!(
            catalog.get("google/gemini-2.5-pro").context_window,
            Some(1_048_576)
        );
        // 变体拼写:厂商 dash 写法命中目录 dot 写法。
        assert_eq!(catalog.get("gemini-2-5-pro").supports_vision, Some(true));
        // 名字后缀启发:未收录模型的 -128k / -1m / ollama tag。
        assert_eq!(catalog.get("some-model-128k").context_window, Some(131_072));
        assert_eq!(catalog.get("some-model-1m").context_window, Some(1_048_576));
        assert_eq!(catalog.get("llama3:128k").context_window, Some(131_072));
        // 什么都匹配不上 → 全未知。
        assert_eq!(catalog.get("mystery"), ModelAbilities::default());
    }

    /// enrich:live 披露优先,目录只补 `None` 字段。
    #[test]
    fn enrich_fills_only_gaps() {
        let _guard = catalog_test_lock().lock().unwrap();
        install_catalog(DevCatalog::from_entries([(
            "m1".to_string(),
            ModelAbilities {
                context_window: Some(200_000),
                max_output_tokens: Some(32_768),
                supports_vision: Some(true),
                supports_reasoning: Some(true),
                effort_levels: Some(vec!["low".into(), "high".into()]),
            },
        )]));
        let mut live = ModelInfo {
            id: "m1".into(),
            context_window: Some(8_000), // live 已披露,保留
            ..Default::default()
        };
        enrich_model_info(&mut live);
        assert_eq!(live.context_window, Some(8_000), "live 优先");
        assert_eq!(live.max_output_tokens, Some(32_768), "目录补空缺");
        assert_eq!(live.supports_vision, Some(true));
        assert_eq!(live.efforts, Some(vec!["low".into(), "high".into()]));
        install_catalog(DevCatalog::default());
    }

    /// envelope 落盘 / 回读 round-trip(read_cache 复用 parse_api_value)。
    /// write_atomic 不自动建父目录(与 refresh 的 create_dir_all 分工),
    /// 测试先建 cache/ 子目录。
    #[test]
    fn cache_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_file(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let payload = br#"{"fetched_at": 1700000000, "api": {"p": {"models": {"m": {"limit": {"context": 64000}}}}}}"#;
        zhuye_fs::write_atomic(&path, payload).unwrap();
        let (at, catalog) = read_cache(&path).expect("cache must round-trip");
        assert_eq!(at, 1_700_000_000);
        assert_eq!(catalog.get("m").context_window, Some(64_000));
    }
}
