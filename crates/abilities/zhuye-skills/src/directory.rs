//! skills.sh 目录(sitemap 索引 + 本地搜索)—— 市场发现层。
//!
//! [skills.sh](https://www.skills.sh) 是 Vercel 运营的 Agent Skills 目录
//! (约两千+技能,源头全部是 GitHub `owner/repo`)。其搜索 API 需 Vercel
//! OIDC 认证(第三方桌面/TUI 无法持有 token),但 **sitemap 全量公开**:
//! `sitemap.xml` 是分片索引,`sitemap-skills-*.xml` 的每个 `<loc>` 即
//! `https://www.skills.sh/{owner}/{repo}/{slug}`。本模块据此把全量清单
//! 拉到本地缓存,搜索在缓存上做(owner/repo/slug 字符串匹配打分)——
//! 无认证、无第三方运行时依赖;代价是没有语义搜索与安装量排序。
//!
//! 安装动作由 [`crate::marketplace::install_skill_from_market`] 承担
//! (`owner/repo/slug` → GitHub tarball);本模块只管「发现」。
//!
//! 缓存格式(`cache_path`,JSON):`{ version, fetched_at, entries }`,
//! TTL 24 小时 —— 过期或 `force` 才重新拉取;分片拉取失败时降级用
//! 过期缓存(搜索可用性优先于数据新鲜度)。

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::model::SkillError;

/// 索引缓存 TTL(24h):sitemap 更新频率低,全量拉取对站点不友好。
const INDEX_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// 单个 sitemap 响应的读取上限(5 MiB)—— 分片实际约数百 KiB。
const MAX_SITEMAP_BYTES: usize = 5 * 1024 * 1024;
/// sitemap 入口与分片域名。
const SKILLS_SH_BASE: &str = "https://www.skills.sh";

/// 官方(第一方厂商)owner 清单 —— 与 skills.sh `/official` 页对齐,
/// 搜索结果据此打「官方」来源标签。维护口径:新增厂商上官方页即补。
pub const OFFICIAL_OWNERS: &[&str] = &[
    "anthropics",
    "microsoft",
    "google",
    "vercel-labs",
    "supabase",
    "prisma",
    "remotion-dev",
    "larksuite",
    "inference-sh",
    "mintlify",
    "mattpocock",
    "obr",
];

/// 目录里的一条技能(GitHub 源三段式标识)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDirEntry {
    /// GitHub 用户/组织名。
    pub owner: String,
    /// 仓库名。
    pub repo: String,
    /// 仓库内的 skill 子目录名(skills.sh 的 slug)。
    pub slug: String,
}

impl SkillDirEntry {
    /// 稳定 id:`owner/repo/slug`(与 skills.sh API 的 id 同形)。
    pub fn id(&self) -> String {
        format!("{}/{}/{}", self.owner, self.repo, self.slug)
    }

    /// 来源仓库:`owner/repo`。
    pub fn source(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// 是否官方(第一方厂商)来源。
    pub fn is_official(&self) -> bool {
        OFFICIAL_OWNERS.contains(&self.owner.as_str())
    }
}

/// 一条搜索命中(带评分与来源标签,供 UI 直接消费)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSearchHit {
    /// 稳定 id:`owner/repo/slug`。
    pub id: String,
    /// 来源仓库 `owner/repo`。
    pub source: String,
    /// skill slug(安装名候选)。
    pub slug: String,
    /// 官方(第一方厂商)来源标签。
    pub official: bool,
    /// 匹配评分(仅排序用,越大越相关)。
    pub score: i64,
}

/// 索引缓存文件结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedIndex {
    version: u32,
    /// 拉取时刻(unix 秒)。
    fetched_at: i64,
    entries: Vec<SkillDirEntry>,
}

/// 获取目录索引:缓存未过期直接返回;否则拉 sitemap 全量重建并写缓存。
///
/// `force = true` 跳过 TTL 强制刷新。网络失败时若存在(哪怕过期的)
/// 本地缓存,降级返回缓存并记 warn —— 搜索可用性优先于新鲜度。
pub async fn fetch_index(cache_path: &Path, force: bool) -> Result<Vec<SkillDirEntry>, SkillError> {
    let cached = read_cache(cache_path);
    let fresh = |c: &CachedIndex| {
        !force
            && c.version == 1
            && SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|now| now.as_secs() as i64 - c.fetched_at)
                .is_ok_and(|age| age >= 0 && Duration::from_secs(age.max(0) as u64) < INDEX_TTL)
    };
    if let Some(c) = cached.as_ref().filter(|c| fresh(c)) {
        return Ok(c.entries.clone());
    }
    match fetch_sitemap().await {
        Ok(entries) => {
            let index = CachedIndex {
                version: 1,
                fetched_at: now_unix(),
                entries,
            };
            if let Some(parent) = cache_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match serde_json::to_string(&index) {
                Ok(json) => {
                    if let Err(e) = std::fs::write(cache_path, json) {
                        tracing::warn!(path = %cache_path.display(), error = %e, "目录索引缓存写入失败(搜索不受影响)");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "目录索引缓存序列化失败"),
            }
            Ok(index.entries)
        }
        Err(e) => match cached {
            // 降级:用过期缓存兜底,只剩错误上抛。
            Some(c) => {
                tracing::warn!(error = %e, "sitemap 拉取失败,降级使用本地缓存(可能过期)");
                Ok(c.entries)
            }
            None => Err(e),
        },
    }
}

/// 全量拉取 sitemap:入口 index → `sitemap-skills-*.xml` 分片 → `<loc>`
/// 解析为 [`SkillDirEntry`]。
async fn fetch_sitemap() -> Result<Vec<SkillDirEntry>, SkillError> {
    let index_xml = http_get_bounded(&format!("{SKILLS_SH_BASE}/sitemap.xml")).await?;
    let shards = extract_locs(&index_xml)
        .into_iter()
        .filter(|loc| {
            // 只要 skills 分片(owners/misc 与本模块无关);分片文件名
            // 形如 sitemap-skills-1.xml —— 未来分片数变化天然兼容。
            loc.rsplit('/')
                .next()
                .is_some_and(|f| f.starts_with("sitemap-skills"))
        })
        .collect::<Vec<_>>();
    if shards.is_empty() {
        return Err(SkillError::Fetch(
            "sitemap.xml 中未找到 sitemap-skills-* 分片(站点结构可能已变化)".to_string(),
        ));
    }
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for shard in shards {
        let xml = http_get_bounded(&shard).await?;
        for loc in extract_locs(&xml) {
            let Some(entry) = parse_skill_loc(&loc) else {
                continue;
            };
            // 分片间或站点异常导致的重复条目,按 id 去重。
            if seen.insert(entry.id()) {
                entries.push(entry);
            }
        }
    }
    if entries.is_empty() {
        return Err(SkillError::Fetch(
            "sitemap-skills 分片解析结果为空".to_string(),
        ));
    }
    Ok(entries)
}

/// 提取 XML 里全部 `<loc>...</loc>` 文本(sitemap 的条目 URL;
/// 字符串扫描即可,不引 XML 解析依赖)。
fn extract_locs(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<loc>") {
        let Some(end) = rest[start + 5..].find("</loc>") else {
            break;
        };
        out.push(rest[start + 5..start + 5 + end].trim().to_string());
        rest = &rest[start + 5 + end + 6..];
    }
    out
}

/// 把 skills.sh 技能页 URL 解析为条目:`/{owner}/{repo}/{slug}` 三段。
/// `site/...` well-known 源(非 GitHub)与非常规路径跳过。
fn parse_skill_loc(loc: &str) -> Option<SkillDirEntry> {
    let path = loc
        .strip_prefix("https://www.skills.sh/")
        .or_else(|| loc.strip_prefix("https://skills.sh/"))?;
    let parts: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) || parts[0] == "site" {
        return None;
    }
    Some(SkillDirEntry {
        owner: parts[0].to_string(),
        repo: parts[1].to_string(),
        slug: parts[2].to_string(),
    })
}

/// 在索引上做本地搜索:大小写不敏感,slug 全等 > 前缀 > 子串 >
/// repo/owner 匹配,按评分降序、同分按 id 字典序稳定排序。
///
/// query 少于 2 个字符返回空(与 skills.sh 搜索端点同阈值)。
pub fn search_entries(entries: &[SkillDirEntry], query: &str, limit: usize) -> Vec<SkillSearchHit> {
    let q = query.trim().to_lowercase();
    if q.chars().count() < 2 || limit == 0 {
        return Vec::new();
    }
    let mut hits: Vec<SkillSearchHit> = entries
        .iter()
        .filter_map(|e| {
            let slug = e.slug.to_lowercase();
            let repo = e.repo.to_lowercase();
            let owner = e.owner.to_lowercase();
            let score = if slug == q {
                100
            } else if slug.starts_with(&q) {
                80
            } else if slug.contains(&q) {
                60
            } else if repo.contains(&q) {
                25
            } else if owner.contains(&q) {
                20
            } else {
                return None;
            };
            Some(SkillSearchHit {
                id: e.id(),
                source: e.source(),
                slug: e.slug.clone(),
                official: e.is_official(),
                score,
            })
        })
        .collect();
    hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    hits.truncate(limit);
    hits
}

/// 读缓存文件(损坏/缺失返回 `None`,不阻断搜索)。
fn read_cache(cache_path: &Path) -> Option<CachedIndex> {
    let raw = std::fs::read_to_string(cache_path).ok()?;
    match serde_json::from_str(&raw) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(path = %cache_path.display(), error = %e, "目录索引缓存损坏,将重建");
            None
        }
    }
}

/// 有界 GET(文本响应):超过 `MAX_SITEMAP_BYTES` 断流。
async fn http_get_bounded(url: &str) -> Result<String, SkillError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| SkillError::Fetch(format!("build reqwest client 失败: {e}")))?;
    let mut resp = client
        .get(url)
        .header("User-Agent", "zhuye-skills-directory")
        .send()
        .await
        .map_err(|e| SkillError::Fetch(format!("GET {url} 失败: {e}")))?;
    if !resp.status().is_success() {
        return Err(SkillError::Fetch(format!(
            "GET {url} 返回 {}",
            resp.status()
        )));
    }
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| SkillError::Fetch(format!("读响应失败: {e}")))?
    {
        if bytes.len() + chunk.len() > MAX_SITEMAP_BYTES {
            return Err(SkillError::Fetch(format!(
                "响应超过 {} 字节上限,已断流",
                MAX_SITEMAP_BYTES
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|e| SkillError::Fetch(format!("响应非 UTF-8: {e}")))
}

fn now_unix() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sitemap 分片过滤:只取 sitemap-skills-*;owners/misc 分片忽略。
    #[test]
    fn skills_shard_filter() {
        let xml = "<urlset><url><loc>https://www.skills.sh/sitemap-misc.xml</loc></url>\
                   <url><loc>https://www.skills.sh/sitemap-owners.xml</loc></url>\
                   <url><loc>https://www.skills.sh/sitemap-skills-1.xml</loc></url>\
                   <url><loc>https://www.skills.sh/sitemap-skills-2.xml</loc></url></urlset>";
        let locs = extract_locs(xml);
        let shard_count = locs
            .iter()
            .filter(|l| {
                l.rsplit('/')
                    .next()
                    .is_some_and(|f| f.starts_with("sitemap-skills"))
            })
            .count();
        assert_eq!(shard_count, 2);
    }

    /// 技能页 URL 解析:三段正例、site 源/两段/四段负例。
    #[test]
    fn parse_skill_loc_forms() {
        let e = parse_skill_loc("https://www.skills.sh/vercel-labs/skills/find-skills").unwrap();
        assert_eq!(
            (e.owner.as_str(), e.repo.as_str(), e.slug.as_str()),
            ("vercel-labs", "skills", "find-skills")
        );
        assert_eq!(e.id(), "vercel-labs/skills/find-skills");
        assert_eq!(e.source(), "vercel-labs/skills");
        // 官方标签:vercel-labs 在清单内。
        assert!(e.is_official());

        assert!(parse_skill_loc("https://skills.sh/site/open.feishu.cn/lark-doc").is_none());
        assert!(parse_skill_loc("https://www.skills.sh/a/b").is_none());
        assert!(parse_skill_loc("https://www.skills.sh/a/b/c/d").is_none());
        assert!(parse_skill_loc("https://example.com/a/b/c").is_none());
    }

    /// 搜索打分:全等 > 前缀 > 子串 > 仓库/owner 匹配;空查询与短查询返回空。
    #[test]
    fn search_scoring_order() {
        let entries = vec![
            SkillDirEntry {
                owner: "a".into(),
                repo: "react-kit".into(),
                slug: "find-skills".into(),
            },
            SkillDirEntry {
                owner: "anthropics".into(),
                repo: "skills".into(),
                slug: "find-skills-fast".into(),
            },
            SkillDirEntry {
                owner: "o".into(),
                repo: "o".into(),
                slug: "super-find-skills".into(),
            },
            SkillDirEntry {
                owner: "vercel-labs".into(),
                repo: "skills".into(),
                slug: "code-review".into(),
            },
        ];
        let hits = search_entries(&entries, "find-skills", 10);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].slug, "find-skills", "全等最高分");
        assert_eq!(hits[1].slug, "find-skills-fast", "前缀次之");
        assert_eq!(hits[2].slug, "super-find-skills", "子串再次");

        // 仓库名命中(code-review 仓库 repo=skills 不含,owner vercel-labs 不含)。
        let by_repo = search_entries(&entries, "react-kit", 10);
        assert_eq!(by_repo.len(), 1);
        assert_eq!(by_repo[0].slug, "find-skills");
        assert_eq!(by_repo[0].score, 25);

        // 非官方 owner 不带标签。
        assert!(!by_repo[0].official);
        let official_hit = search_entries(&entries, "code-review", 10);
        assert_eq!(official_hit.len(), 1);
        assert!(official_hit[0].official, "vercel-labs 应带官方标签");

        // 短查询 / 空查询。
        assert!(search_entries(&entries, "f", 10).is_empty());
        assert!(search_entries(&entries, "  ", 10).is_empty());
        // limit 截断。
        assert_eq!(search_entries(&entries, "find", 1).len(), 1);
    }

    /// 缓存读写往返 + TTL 判定(不碰网络,直接构造 CachedIndex)。
    #[test]
    fn cache_roundtrip_and_freshness() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("skills-market.json");
        assert!(read_cache(&cache).is_none());

        let index = CachedIndex {
            version: 1,
            fetched_at: now_unix(),
            entries: vec![SkillDirEntry {
                owner: "a".into(),
                repo: "b".into(),
                slug: "c".into(),
            }],
        };
        std::fs::write(&cache, serde_json::to_string(&index).unwrap()).unwrap();
        let back = read_cache(&cache).unwrap();
        assert_eq!(back.entries, index.entries);
        // 刚写完必然「新鲜」;把 fetched_at 挪到 25h 前则过期。
        assert!(back.version == 1);
        let stale = CachedIndex {
            fetched_at: now_unix() - 25 * 3600,
            ..back
        };
        std::fs::write(&cache, serde_json::to_string(&stale).unwrap()).unwrap();
        let stale_back = read_cache(&cache).unwrap();
        let age = now_unix() - stale_back.fetched_at;
        assert!(Duration::from_secs(age.max(0) as u64) >= INDEX_TTL);
    }
}
