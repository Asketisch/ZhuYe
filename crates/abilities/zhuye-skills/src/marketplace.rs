//! v1.11 桌面#1:Skill 市场 —— 从 GitHub 仓库一键安装技能。
//!
//! 语义:一个 GitHub 仓库 = 一个技能目录(根含 `SKILL.md` 与可选附属
//! 文件)。安装 = 拉取 `SKILL.md` 原文,按 frontmatter `name:`(缺省用
//! 仓库名)落盘到 `<out_dir>/<name>/SKILL.md` —— 之后既有扫描链
//! (`scan_skills_dirs`)自然发现,无需注册表。
//!
//! 来源约束:仅接受 `owner/repo` 或完整 `https://github.com/owner/repo`
//! 两种形态(防 `file://` / 本地路径注入);拉取走 GitHub contents API
//! (`Accept: application/vnd.github.raw` 直取原文)。附属文件
//! (scripts / references)不随 SKILL.md 自动下载 —— 渐进披露的第二级
//! 由用户按需手动补,避免盲目拉取可执行内容。
//!
//! 市场发现(`discover` 列表)由插件市场承担(zhuye-plugin
//! `marketplace discover`),技能安装专注单仓安装这一个动作。

use std::path::{Path, PathBuf};

use crate::loader::parse_skill_str;
use crate::model::SkillError;

/// 技能目录名的合法字符(`[a-z0-9_-]`,长度 1~64)。
fn validate_skill_dir_name(name: &str) -> Result<(), SkillError> {
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
}
