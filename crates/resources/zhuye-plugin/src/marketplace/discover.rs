//! v1.11 基建#13:GitHub topic 插件发现。
//!
//! 需求语义:「GitHub 上标注 `zhuye-plugin` topic 的仓库自动发现并进
//! 市场展示」。本模块查 GitHub Search API
//! (`GET /search/repositories?q=topic:<topic>`),返回仓库清单(名字 /
//! 描述 / star / topics / 直链),CLI 以
//! `zhuye plugin marketplace discover [--topic ...]` 暴露 —— 发现结果
//! 每条自带 `marketplace add <name> --from github --repo <owner/repo>`
//! 的现成命令,用户复制即接通既有五源拉取链路(发现 ≠ 自动接入:
//! 接入 marketplace 是写操作,仍由用户显式执行)。
//!
//! 请求头遵守 GitHub API 要求:`User-Agent` + `Accept:
//! application/vnd.github+json`。无 token 时为匿名配额(60 req/h),
//! 够发现场景使用;`ZHUYE_GITHUB_TOKEN` 设置时附加
//! `Authorization: Bearer` 提升配额(5000 req/h)。

use serde::Deserialize;

use crate::errors::{PluginError, Result};

/// 默认发现 topic(需求原文:GitHub topic `zhuye-plugin`)。
pub const DEFAULT_PLUGIN_TOPIC: &str = "zhuye-plugin";

/// 一条发现结果。
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredPlugin {
    /// `owner/repo` 全名(可直接用作 `--from github --repo` 参数)。
    pub full_name: String,
    /// 仓库主页 URL。
    pub html_url: String,
    /// 仓库描述(可能为空)。
    pub description: Option<String>,
    /// star 数(热度参考)。
    pub stars: u64,
    /// 仓库 topics 标签(含被查的 topic)。
    pub topics: Vec<String>,
}

/// GitHub Search API 响应(只取用字段)。
#[derive(Deserialize)]
struct SearchResponse {
    items: Vec<SearchItem>,
}

#[derive(Deserialize)]
struct SearchItem {
    full_name: String,
    html_url: String,
    #[serde(default)]
    description: Option<String>,
    stargazers_count: u64,
    #[serde(default)]
    topics: Vec<String>,
}

/// 按 topic 发现插件仓库(按 GitHub 相关度默认排序,截 `limit` 条)。
pub async fn discover_by_topic(topic: &str, limit: usize) -> Result<Vec<DiscoveredPlugin>> {
    let topic = topic.trim();
    if topic.is_empty() {
        return Err(PluginError::Http("topic 不能为空".into()));
    }
    let limit = limit.clamp(1, 100); // GitHub per_page 上限 100
    let url =
        format!("https://api.github.com/search/repositories?q=topic:{topic}&per_page={limit}");
    let mut builder = reqwest::Client::builder()
        .build()
        .map_err(|e| PluginError::Http(format!("build reqwest client 失败: {e}")))?
        .get(&url)
        .header("User-Agent", "zhuye-plugin-marketplace")
        .header("Accept", "application/vnd.github+json");
    // 可选 token 提升配额(匿名 60 req/h 不够重度发现)。
    if let Ok(token) = std::env::var("ZHUYE_GITHUB_TOKEN")
        && !token.trim().is_empty()
    {
        builder = builder.bearer_auth(token.trim());
    }
    let resp = builder
        .send()
        .await
        .map_err(|e| PluginError::Http(format!("GET {url} 失败: {e}")))?;
    if !resp.status().is_success() {
        return Err(PluginError::Http(format!(
            "GET {url} 返回 {};匿名配额耗尽时可设 ZHUYE_GITHUB_TOKEN",
            resp.status()
        )));
    }
    let body: SearchResponse = resp
        .json()
        .await
        .map_err(|e| PluginError::Http(format!("解析 GitHub 搜索响应失败: {e}")))?;
    Ok(body
        .items
        .into_iter()
        .map(|i| DiscoveredPlugin {
            full_name: i.full_name,
            html_url: i.html_url,
            description: i.description.filter(|d| !d.trim().is_empty()),
            stars: i.stargazers_count,
            topics: i.topics,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// topic 空 / 空白拒绝(不发请求)。
    #[tokio::test]
    async fn blank_topic_rejected() {
        let err = discover_by_topic("   ", 10).await.unwrap_err();
        assert!(matches!(err, PluginError::Http(ref m) if m.contains("topic")));
    }

    /// limit 钳制到 GitHub 合法区间(不发请求前 clamp 生效;这里只验
    /// 超上限不 panic —— 网络调用因非法 topic 校验在前,limit 单测仅
    /// 覆盖纯函数行为不可行,故只验证 1 与 100 之间路径不再校验)。
    #[tokio::test]
    async fn topic_validation_precedes_network() {
        // 空 topic 在网络调用前返回,证明校验路径独立于网络。
        assert!(discover_by_topic("", 500).await.is_err());
    }
}
