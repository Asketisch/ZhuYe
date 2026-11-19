//! v1.11 桌面#1:Skill 市场 —— 从 GitHub 仓库一键安装技能。
//!
//! 两级安装语义:
//!
//! - **单仓形态**([`install_from_github`],v1.11):一个 GitHub 仓库 = 一个
//!   技能目录(根含 `SKILL.md`)。安装 = 拉取 `SKILL.md` 原文,按 frontmatter
//!   `name:`(缺省用仓库名)落盘到 `<out_dir>/<name>/SKILL.md` —— 之后既有
//!   扫描链(`scan_skills_dirs`)自然发现,无需注册表。附属文件(scripts /
//!   references)不随 SKILL.md 自动下载 —— 渐进披露的第二级由用户按需手动
//!   补,避免盲目拉取可执行内容。
//! - **目录生态形态**([`install_skill_from_market`],skills.sh 生态):
//!   `owner/repo[/slug]` 三段式定位仓库内的子目录 skill(`skills/<slug>/`
//!   布局),走 codeload tarball 一次拉全仓、**整目录提取**(SKILL.md +
//!   附属 scripts/references)。这里「整目录」就是 skill 的完整单元 ——
//!   与官方 skills CLI 的安装语义一致;安全边界换到提取器:防路径穿越
//!   (zip-slip)、跳过符号链接、跳过 `metadata.json`,并在落盘前过
//!   [`crate::supply_chain::scan_skill`] 安装前扫描(block 级发现即拒装)。
//!
//! 来源约束:仅接受 `owner/repo[/slug]`、`https://github.com/owner/repo`
//! 或 `https://skills.sh/owner/repo/slug` 形态(防 `file://` / 本地路径
//! 注入);单仓形态拉取走 GitHub contents API(`Accept:
//! application/vnd.github.raw` 直取原文),目录生态形态走 codeload
//! tarball(匿名可用,不受 contents API 60 次/小时限额约束)。
//!
//! 市场发现(搜索/排行榜)由 [`crate::directory`](skills.sh sitemap 索引)
//! 承担;插件市场发现另见 zhuye-plugin `marketplace discover`。

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::loader::parse_skill_str;
use crate::model::SkillError;

/// 技能目录名的合法字符(`[a-z0-9_-]`,长度 1~64)。
pub fn validate_skill_dir_name(name: &str) -> Result<(), SkillError> {
    let ok = !name.is_empty()
        && name.chars().count() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(SkillError::Invalid(
            "skill directory name",
            format!("invalid skill name `{name}`: only [a-z0-9_-], 1..64 chars"),
        ))
    }
}

/// 把 `owner/repo` 或 GitHub URL 归一为 `owner/repo`;非法形态报错。
pub fn normalize_repo(repo: &str) -> Result<String, SkillError> {
    let trimmed = repo.trim();
    let stripped = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .unwrap_or(trimmed);
    let stripped = stripped.trim_end_matches('/');
    let stripped = stripped.strip_suffix(".git").unwrap_or(stripped);
    let parts: Vec<&str> = stripped.split('/').collect();
    // 允许 `owner/repo` 与 `owner/repo/<subpath>`;至少两段。
    if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(SkillError::Invalid(
            "github repo",
            format!("expected `owner/repo`, got `{repo}`"),
        ));
    }
    Ok(format!("{}/{}", parts[0], parts[1]))
}

/// 从 GitHub 仓库安装一个技能到 `out_dir`(通常为 `~/.zhuye/skills`)。
/// 返回安装后的技能目录路径(含已解析的技能名)。
pub async fn install_from_github(repo: &str, out_dir: &Path) -> Result<PathBuf, SkillError> {
    let repo = normalize_repo(repo)?;
    let url = format!("https://api.github.com/repos/{repo}/contents/SKILL.md");
    let mut builder = reqwest::Client::builder()
        .build()
        .map_err(|e| SkillError::Fetch(format!("build reqwest client 失败: {e}")))?
        .get(&url)
        .header("User-Agent", "zhuye-skills-marketplace")
        .header("Accept", "application/vnd.github.raw");
    // 可选 token 提升匿名配额。
    if let Ok(token) = std::env::var("ZHUYE_GITHUB_TOKEN")
        && !token.trim().is_empty()
    {
        builder = builder.bearer_auth(token.trim());
    }
    let resp = builder
        .send()
        .await
        .map_err(|e| SkillError::Fetch(format!("GET {url} 失败: {e}")))?;
    if !resp.status().is_success() {
        return Err(SkillError::Fetch(format!(
            "GET {url} 返回 {}(仓库不存在或根目录无 SKILL.md)",
            resp.status()
        )));
    }
    let raw = resp
        .text()
        .await
        .map_err(|e| SkillError::Fetch(format!("读响应失败: {e}")))?;
    // 解析 frontmatter 拿 name(顺带校验它真的是一个 SKILL.md)。
    let meta = parse_skill_str(&raw).map_err(|e| {
        SkillError::Fetch(format!(
            "仓库 {repo} 的 SKILL.md 不是合法技能(frontmatter 缺失或非法): {e}"
        ))
    })?;
    let name = if meta.name.trim().is_empty() {
        repo.split('/').next_back().unwrap_or("skill").to_string()
    } else {
        meta.name.trim().to_string()
    };
    validate_skill_dir_name(&name)?;
    let dir = out_dir.join(&name);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("SKILL.md");
    std::fs::write(&path, raw.as_bytes())?;
    tracing::info!(repo = %repo, name = %name, path = %path.display(), "skill installed from github");
    Ok(dir)
}

// ── 目录生态形态(skills.sh):tarball 整目录安装 ─────────────────

/// tarball 下载上限(20 MiB)—— skill 仓库通常几百 KiB,超限视为异常。
const MAX_TARBALL_BYTES: usize = 20 * 1024 * 1024;
/// 单个解包文件上限(8 MiB)。
const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
/// 单个 skill 目录的文件数上限。
const MAX_FILES: usize = 400;

/// 提取产物:skill 根下的相对路径 → 文件内容。
type SkillFiles = Vec<(String, Vec<u8>)>;

/// 目录生态的市场安装源:GitHub `owner/repo` + 可选 `slug`(仓库内的
/// skill 子目录名)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketSource {
    /// GitHub 用户/组织名。
    pub owner: String,
    /// 仓库名。
    pub repo: String,
    /// skill 子目录名(skills.sh 生态的 `owner/repo/slug` 第三段;缺省
    /// 表示仓库根的单 skill 形态)。
    pub slug: Option<String>,
}

/// 把市场来源归一为 [`MarketSource`]。接受形态:
///
/// - `owner/repo` / `owner/repo/slug`(裸形态)
/// - `https://github.com/owner/repo`(两段语义,忽略更深子路径 —— 与
///   [`normalize_repo`] 行为一致)
/// - `https://skills.sh/owner/repo/slug`(技能页链接)
///
/// 段字符限 `[A-Za-z0-9._-]`(防 `..` / `file://` / 查询串注入);
/// `site/...` well-known 源(非 GitHub)不支持,显式报错。
pub fn normalize_market_source(source: &str) -> Result<MarketSource, SkillError> {
    let s = source.trim();
    // github URL:两段语义,子路径截断(与 normalize_repo 同行为)。
    if s.starts_with("https://github.com/") || s.starts_with("http://github.com/") {
        let repo = normalize_repo(s)?;
        let (owner, name) = repo
            .split_once('/')
            .expect("normalize_repo 保证 `owner/repo` 两段");
        return Ok(MarketSource {
            owner: owner.to_string(),
            repo: name.to_string(),
            slug: None,
        });
    }
    // skills.sh 技能页链接 → 剥域名,取路径三段。
    let s = s
        .strip_prefix("https://www.skills.sh/")
        .or_else(|| s.strip_prefix("https://skills.sh/"))
        .unwrap_or(s);
    let s = s.trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return Err(SkillError::Invalid(
            "market source",
            format!("expected `owner/repo[/slug]`, got `{source}`"),
        ));
    }
    if parts[0] == "site" {
        return Err(SkillError::Invalid(
            "market source",
            "well-known(`site/...`)源不是 GitHub 仓库,暂不支持安装".to_string(),
        ));
    }
    // 深于 3 段不支持 —— 粘贴 skills.sh 技能页链接或用 `owner/repo/slug`。
    if parts.len() > 3 {
        return Err(SkillError::Invalid(
            "market source",
            format!("来源段过多(`{source}`):请用 `owner/repo/slug` 三段式或 skills.sh 技能页链接"),
        ));
    }
    for (i, p) in parts.iter().enumerate() {
        let ok = !p.is_empty()
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
        if !ok {
            return Err(SkillError::Invalid(
                "market source",
                format!("来源第 {} 段 `{p}` 含非法字符", i + 1),
            ));
        }
    }
    Ok(MarketSource {
        owner: parts[0].to_string(),
        repo: parts[1].to_string(),
        slug: parts.get(2).map(|s| s.to_string()),
    })
}

/// 从 skills.sh 生态(GitHub 仓库)安装一个技能到 `out_dir`。
///
/// 与 [`install_from_github`](单仓形态)的差异:支持 `owner/repo/slug`
/// 三段式定位仓库内子目录(`skills/<slug>/` 布局),并**整目录提取**
/// (SKILL.md + scripts / references 附属文件)。tarball 经
/// `codeload.github.com` 拉取(匿名可用),落盘前过 [`crate::supply_chain::scan_skill`]
/// 安装前扫描(block 级发现即拒装)。返回安装后的技能目录路径。
pub async fn install_skill_from_market(
    source: &str,
    out_dir: &Path,
) -> Result<PathBuf, SkillError> {
    let src = normalize_market_source(source)?;
    let url = format!(
        "https://codeload.github.com/{}/{}/tar.gz/HEAD",
        src.owner, src.repo
    );
    let tarball = fetch_bounded(&url, MAX_TARBALL_BYTES).await?;
    let (dir_name, files) = extract_skill_dir_from_tarball(&tarball, src.slug.as_deref())?;

    // SKILL.md 必在提取结果里;解析 frontmatter 定技能名并顺带校验。
    let skill_md = files
        .iter()
        .find(|(p, _)| p == "SKILL.md")
        .ok_or_else(|| SkillError::Fetch("提取结果缺少 SKILL.md".to_string()))?;
    let raw = String::from_utf8_lossy(&skill_md.1).into_owned();
    let meta = parse_skill_str(&raw).map_err(|e| {
        SkillError::Fetch(format!(
            "SKILL.md 不是合法技能(frontmatter 缺失或非法): {e}"
        ))
    })?;
    let name = if meta.name.trim().is_empty() {
        src.slug.clone().unwrap_or_else(|| src.repo.clone())
    } else {
        meta.name.trim().to_string()
    };
    validate_skill_dir_name(&name)?;

    // 安装前供应链扫描:block 即拒装,warn 记日志放行(与 TUI 同口径)。
    let report = crate::supply_chain::scan_skill(&meta.body, &meta.tools);
    if report.is_blocked() {
        return Err(SkillError::Fetch(format!(
            "安装前安全扫描阻断({}):{}",
            report.summary(),
            report.findings[0].excerpt
        )));
    }
    if !report.is_clean() {
        tracing::warn!(source = %source, summary = %report.summary(), "skill 安装前扫描发现警告,已放行");
    }

    // 先落 staging 再原子替换:失败不残留半装目录,旧版本可完整回退。
    persist_skill_files(out_dir, &name, &files)?;
    let target = out_dir.join(&name);
    tracing::info!(
        source = %source,
        name = %name,
        path = %target.display(),
        files = files.len(),
        dir_name = %dir_name,
        "skill installed from market tarball"
    );
    Ok(target)
}

/// 把提取出的文件集整目录落盘到 `out_dir/<name>`(staging + 原子替换)。
///
/// 旧目录先 rename 成 `.old-<name>` 再整体换新 —— 替换失败时回滚,
/// 成功后清掉 `.old`;同一 skill 的旧文件(新版本已删除的)不会残留。
fn persist_skill_files(
    out_dir: &Path,
    name: &str,
    files: &[(String, Vec<u8>)],
) -> Result<PathBuf, SkillError> {
    std::fs::create_dir_all(out_dir)?;
    let target = out_dir.join(name);
    let staging = out_dir.join(format!(".installing-{name}"));
    let old = out_dir.join(format!(".old-{name}"));
    // 清掉上次失败可能残留的 staging / old。
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_dir_all(&old);
    std::fs::create_dir_all(&staging)?;
    for (rel, bytes) in files {
        // 相对路径已在提取器里归一(仅 Normal 段),这里只负责落盘。
        let dest = staging.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, bytes)?;
    }
    let swapped = std::fs::rename(&target, &old);
    if let Err(e) = std::fs::rename(&staging, &target) {
        // 回滚:把旧版本放回去,staging 清理后报错。
        if swapped.is_ok() {
            let _ = std::fs::rename(&old, &target);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(SkillError::Io(e));
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(target)
}

/// 有界下载:流式读取并在超过 `max` 时立刻断流(tarball 解包前的
/// 第一道体积闸,防恶意仓库拖垮内存)。
async fn fetch_bounded(url: &str, max: usize) -> Result<Vec<u8>, SkillError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| SkillError::Fetch(format!("build reqwest client 失败: {e}")))?;
    let mut builder = client
        .get(url)
        .header("User-Agent", "zhuye-skills-marketplace");
    // 可选 token 提升匿名配额(私有仓库 / 限额场景)。
    if let Ok(token) = std::env::var("ZHUYE_GITHUB_TOKEN")
        && !token.trim().is_empty()
    {
        builder = builder.bearer_auth(token.trim());
    }
    let mut resp = builder
        .send()
        .await
        .map_err(|e| SkillError::Fetch(format!("GET {url} 失败: {e}")))?;
    if !resp.status().is_success() {
        return Err(SkillError::Fetch(format!(
            "GET {url} 返回 {}(仓库不存在或不可访问)",
            resp.status()
        )));
    }
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| SkillError::Fetch(format!("读响应失败: {e}")))?
    {
        if bytes.len() + chunk.len() > max {
            return Err(SkillError::Fetch(format!("响应超过 {max} 字节上限,已断流")));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// tar entry 路径的组件级归一:只保留 `Normal` 段;出现 `..` / 绝对
/// 路径 / 盘符前缀等非常规段时返回 `None`(调用方跳过该条目)——
/// zip-slip 的第一道防线(第二道在 [`persist_skill_files`] 的 join 层)。
fn sanitize_rel_path(raw: &Path) -> Option<PathBuf> {
    let mut norm = PathBuf::new();
    for comp in raw.components() {
        match comp {
            std::path::Component::Normal(c) => norm.push(c),
            _ => return None,
        }
    }
    Some(norm)
}

/// 从 tarball 字节流里定位并提取一个 skill 目录(布局探测 + 安全提取)。
///
/// 布局探测(slug 给定时按优先级):
/// 1. `**/skills/<slug>/SKILL.md`(skills.sh 生态主布局,浅者优先);
/// 2. `**/<slug>/SKILL.md`(repo 根直下同名目录)。
///
/// slug 缺省(`owner/repo` 两段式):找 tarball 根直下的 `SKILL.md`
/// (单 skill 仓库);若仓库是 `skills/<name>/` 集合仓则报错并列出候选。
///
/// 安全边界:每个 entry 的路径先做**组件级归一**(只保留 `Normal` 段,
/// `..` / 绝对路径 / 盘符前缀一律丢弃并记 warn —— zip-slip 防线);
/// 符号链接 / 硬链接 entry 跳过;`metadata.json`(官方 CLI 的安装器
/// 元数据,非 skill 运行所需)不落盘。
///
/// 返回 `(探测命中的目录名, 以 SKILL.md 所在目录为根的相对文件清单)`。
fn extract_skill_dir_from_tarball(
    tarball: &[u8],
    slug: Option<&str>,
) -> Result<(String, SkillFiles), SkillError> {
    let gz = flate2::read::GzDecoder::new(tarball);
    let mut archive = tar::Archive::new(gz);
    // 全量文件清单(组件归一后):路径 → 内容。
    let mut all: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in archive
        .entries()
        .map_err(|e| SkillError::Fetch(format!("tarball 解包失败: {e}")))?
    {
        let mut entry = entry.map_err(|e| SkillError::Fetch(format!("tarball 读条目失败: {e}")))?;
        // 只取普通文件:目录 entry 无内容,链接 entry 是潜在攻击面。
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let raw_path = entry
            .path()
            .map_err(|e| SkillError::Fetch(format!("tarball 条目路径非法: {e}")))?;
        let raw_str = raw_path.to_string_lossy().into_owned();
        // 组件级归一:拒绝 `..` / 绝对路径 / 盘符,只留 Normal 段。
        let Some(norm) = sanitize_rel_path(&raw_path) else {
            tracing::warn!(path = %raw_str, "tarball 条目含非常规路径段,已跳过");
            continue;
        };
        let Some(rel) = norm.to_str().map(str::to_string) else {
            tracing::warn!(path = %raw_str, "tarball 条目路径非 UTF-8,已跳过");
            continue;
        };
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut buf)
            .map_err(|e| SkillError::Fetch(format!("tarball 读文件失败: {e}")))?;
        if buf.len() > MAX_FILE_BYTES {
            return Err(SkillError::Fetch(format!(
                "文件 {rel} 超过 {} 字节上限",
                MAX_FILE_BYTES
            )));
        }
        all.push((rel, buf));
        if all.len() > MAX_FILES {
            return Err(SkillError::Fetch(format!(
                "解包文件数超过 {} 上限",
                MAX_FILES
            )));
        }
    }

    // 所有 SKILL.md 的所在目录(含根形态)。
    let skill_md_dirs: Vec<&str> = all
        .iter()
        .filter_map(|(p, _)| {
            p.strip_suffix("/SKILL.md")
                .or_else(|| (p == "SKILL.md").then_some(""))
        })
        .collect();
    // 布局探测:候选「skill 目录前缀」,浅者优先(同深度按字典序稳定)。
    let pick_shallowest = |pred: &dyn Fn(&str) -> bool| {
        skill_md_dirs
            .iter()
            .copied()
            .filter(|d| pred(d))
            .min_by_key(|d| (d.matches('/').count(), d.to_string()))
    };
    let prefix: Option<String> = if let Some(slug) = slug {
        // 优先 `**/skills/<slug>` 布局,次选任意 `**/<slug>` 目录。
        pick_shallowest(&|d| {
            d.rsplit_once('/')
                .is_some_and(|(parent, last)| last == slug && parent.ends_with("/skills"))
                || *d == format!("skills/{slug}")
        })
        .or_else(|| pick_shallowest(&|d| d.rsplit_once('/').is_some_and(|(_, last)| last == slug)))
        .map(str::to_string)
    } else {
        // 两段式:仅认 tarball 根直下(SKILL.md 深度为 1)的唯一单 skill 形态。
        let mut roots = skill_md_dirs.iter().copied().filter(|d| !d.contains('/'));
        match (roots.next(), roots.next()) {
            (Some(only), None) => Some(only.to_string()),
            _ => None,
        }
    };

    let prefix = prefix.ok_or_else(|| {
        // 报错信息带仓库内可用 slug(BTreeSet 去重 + 字典序稳定)。
        let uniq: std::collections::BTreeSet<&str> = skill_md_dirs
            .iter()
            .map(|d| d.rsplit('/').next().unwrap_or(d))
            .filter(|s| !s.is_empty())
            .collect();
        if uniq.is_empty() {
            SkillError::Fetch("tarball 内未找到任何 SKILL.md".to_string())
        } else {
            SkillError::Fetch(format!(
                "未定位到目标 skill;仓库内可用 slug:{}",
                uniq.iter().take(12).cloned().collect::<Vec<_>>().join(", ")
            ))
        }
    })?;
    let dir_name = prefix.rsplit('/').next().unwrap_or(&prefix).to_string();

    // 提取该前缀下全部文件(相对 skill 根);metadata.json 不落盘。
    let base = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    };
    let mut files = Vec::new();
    for (p, bytes) in &all {
        let Some(rel) = p.strip_prefix(&base) else {
            continue;
        };
        if rel.is_empty() || rel == "metadata.json" {
            continue;
        }
        files.push((rel.to_string(), bytes.clone()));
    }
    if files.is_empty() {
        return Err(SkillError::Fetch("skill 目录提取结果为空".to_string()));
    }
    Ok((dir_name, files))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// repo 归一化:owner/repo、URL、URL + 子路径、.git 后缀。
    #[test]
    fn normalize_repo_forms() {
        assert_eq!(normalize_repo("a/b").unwrap(), "a/b");
        assert_eq!(normalize_repo("https://github.com/a/b").unwrap(), "a/b");
        assert_eq!(normalize_repo("https://github.com/a/b.git").unwrap(), "a/b");
        assert_eq!(
            normalize_repo("https://github.com/a/b/tree/main/skills/").unwrap(),
            "a/b"
        );
        assert!(normalize_repo("just-a-name").is_err());
        assert!(normalize_repo("https://example.com/a/b").is_err());
        assert!(normalize_repo("").is_err());
    }

    /// 目录名校验:小写字母数字连字符下划线。
    #[test]
    fn skill_dir_name_validation() {
        assert!(validate_skill_dir_name("code-review").is_ok());
        assert!(validate_skill_dir_name("Skill_2").is_err());
        assert!(validate_skill_dir_name("").is_err());
        assert!(validate_skill_dir_name("../evil").is_err());
    }

    /// 市场来源归一:裸三段 / skills.sh 链接 / github URL 两段 / 非法形态。
    #[test]
    fn normalize_market_source_forms() {
        assert_eq!(
            normalize_market_source("a/b").unwrap(),
            MarketSource {
                owner: "a".into(),
                repo: "b".into(),
                slug: None
            }
        );
        assert_eq!(
            normalize_market_source("a/b/find-skills").unwrap(),
            MarketSource {
                owner: "a".into(),
                repo: "b".into(),
                slug: Some("find-skills".into())
            }
        );
        assert_eq!(
            normalize_market_source("https://skills.sh/a/b/foo").unwrap(),
            MarketSource {
                owner: "a".into(),
                repo: "b".into(),
                slug: Some("foo".into())
            }
        );
        // github URL:两段语义,子路径截断(与 normalize_repo 一致)。
        assert_eq!(
            normalize_market_source("https://github.com/a/b/tree/main/skills/").unwrap(),
            MarketSource {
                owner: "a".into(),
                repo: "b".into(),
                slug: None
            }
        );
        assert!(normalize_market_source("just-a-name").is_err());
        assert!(normalize_market_source("").is_err());
        // well-known 站点源(非 GitHub)显式拒绝。
        assert!(normalize_market_source("https://skills.sh/site/foo.com/bar").is_err());
        // 段内非法字符(`..` 注入)拒绝。
        assert!(normalize_market_source("a/b/../etc").is_err());
        // 深于三段的裸形态拒绝。
        assert!(normalize_market_source("a/b/c/d").is_err());
    }

    /// 组件级归一:`..` / 绝对路径 / `.` 段一律拒绝,普通路径原样通过。
    #[test]
    fn sanitize_rel_path_rejects_traversal() {
        assert_eq!(
            sanitize_rel_path(Path::new("repo-main/skills/foo/SKILL.md")),
            Some(PathBuf::from("repo-main/skills/foo/SKILL.md"))
        );
        assert_eq!(sanitize_rel_path(Path::new("../evil.sh")), None);
        assert_eq!(sanitize_rel_path(Path::new("a/../../evil.sh")), None);
        assert_eq!(sanitize_rel_path(Path::new("/abs/evil.sh")), None);
        assert_eq!(sanitize_rel_path(Path::new("./cur.sh")), None);
    }

    /// 构造测试 tarball:普通文件 + 可选符号链接 entry。
    fn build_tarball(files: &[(&str, &[u8])], links: &[(&str, &str)]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        {
            let mut b = tar::Builder::new(&mut enc);
            for (path, data) in files {
                let mut h = tar::Header::new_gnu();
                h.set_size(data.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                b.append_data(&mut h, path, *data)
                    .expect("append tar entry");
            }
            for (path, target) in links {
                let mut h = tar::Header::new_gnu();
                h.set_size(0);
                h.set_entry_type(tar::EntryType::Symlink);
                h.set_link_name(target).expect("set link name");
                h.set_cksum();
                b.append_data(&mut h, path, std::io::empty())
                    .expect("append tar symlink");
            }
            b.into_inner().expect("flush tar");
        }
        enc.finish().expect("finish gzip")
    }

    const SKILL_MD: &[u8] = b"---\nname: foo\ndescription: demo skill\n---\n# Foo\nbody\n";

    /// skills.sh 主布局:`**/skills/<slug>/SKILL.md` 优先于同名根直下目录,
    /// 整目录提取(附属 scripts 一并取出,metadata.json 剔除)。
    #[test]
    fn extract_prefers_skills_layout() {
        let tb = build_tarball(
            &[
                ("repo-main/skills/foo/SKILL.md", SKILL_MD),
                ("repo-main/skills/foo/scripts/run.sh", b"echo hi\n"),
                ("repo-main/skills/foo/metadata.json", b"{}"),
                ("repo-main/foo/SKILL.md", b"---\nname: decoy\n---\n"),
            ],
            &[],
        );
        let (dir_name, files) = extract_skill_dir_from_tarball(&tb, Some("foo")).unwrap();
        assert_eq!(dir_name, "foo");
        let mut names: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["SKILL.md", "scripts/run.sh"]);
    }

    /// 无 slug 的两段式:认 tarball 根直下 SKILL.md(单 skill 仓库)。
    #[test]
    fn extract_root_single_skill() {
        let tb = build_tarball(
            &[
                ("repo-main/SKILL.md", SKILL_MD),
                ("repo-main/scripts/x.py", b"print(1)\n"),
                ("repo-main/README.md", b"not part of skill\n"),
            ],
            &[],
        );
        let (_, files) = extract_skill_dir_from_tarball(&tb, None).unwrap();
        let mut names: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["README.md", "SKILL.md", "scripts/x.py"]);
    }

    /// 根直下同名目录布局(`repo/<slug>/SKILL.md`)兜底命中。
    #[test]
    fn extract_bare_dir_layout() {
        let tb = build_tarball(&[("repo-main/foo/SKILL.md", SKILL_MD)], &[]);
        let (dir_name, files) = extract_skill_dir_from_tarball(&tb, Some("foo")).unwrap();
        assert_eq!(dir_name, "foo");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, "SKILL.md");
    }

    /// 符号链接 entry 一律跳过(攻击面),不进提取结果。
    #[test]
    fn extract_skips_symlinks() {
        let tb = build_tarball(
            &[
                ("repo-main/skills/foo/SKILL.md", SKILL_MD),
                ("repo-main/skills/foo/link.sh", b""),
            ],
            &[("repo-main/skills/foo/evil-link", "/etc/passwd")],
        );
        let (_, files) = extract_skill_dir_from_tarball(&tb, Some("foo")).unwrap();
        assert!(files.iter().all(|(p, _)| p != "evil-link"), "{files:?}");
    }

    /// 集合仓(skills/a + skills/b)在无 slug 时报错并列出候选 slug。
    #[test]
    fn extract_collection_repo_lists_slugs() {
        let tb = build_tarball(
            &[
                ("repo-main/skills/alpha/SKILL.md", SKILL_MD),
                ("repo-main/skills/beta/SKILL.md", SKILL_MD),
            ],
            &[],
        );
        let err = extract_skill_dir_from_tarball(&tb, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("alpha") && msg.contains("beta"), "{msg}");
    }

    /// 无任何 SKILL.md 的仓库:显式报错。
    #[test]
    fn extract_no_skill_md_errors() {
        let tb = build_tarball(&[("repo-main/README.md", b"hi\n")], &[]);
        assert!(extract_skill_dir_from_tarball(&tb, Some("foo")).is_err());
    }

    /// 落盘是整目录替换:旧版本的 extra 文件不残留,staging/.old 不残留。
    #[test]
    fn persist_replaces_whole_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path();
        // 预置旧版本(带 extra.txt)。
        std::fs::create_dir_all(out.join("foo")).unwrap();
        std::fs::write(out.join("foo/SKILL.md"), b"old").unwrap();
        std::fs::write(out.join("foo/extra.txt"), b"stale").unwrap();
        let target = persist_skill_files(
            out,
            "foo",
            &[
                ("SKILL.md".to_string(), b"new".to_vec()),
                ("scripts/run.sh".to_string(), b"echo hi\n".to_vec()),
            ],
        )
        .unwrap();
        assert_eq!(target, out.join("foo"));
        assert_eq!(std::fs::read(target.join("SKILL.md")).unwrap(), b"new");
        assert!(target.join("scripts/run.sh").exists());
        assert!(
            !target.join("extra.txt").exists(),
            "旧文件必须随整目录替换消失"
        );
        assert!(!out.join(".installing-foo").exists());
        assert!(!out.join(".old-foo").exists());
    }
}
