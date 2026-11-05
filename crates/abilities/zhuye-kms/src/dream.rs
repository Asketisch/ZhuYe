//! 会话洞察页面的创建。

use super::*;

impl KnowledgeManager {
    /// 运行 dream 会话 - 从最近的会话中提取洞察。
    pub fn dream(
        &self,
        insights: Vec<String>,
        sessions_analyzed: usize,
    ) -> Result<DreamResult, KmsError> {
        let insights_wiki = "dream-insights";
        if !self.wiki_path(insights_wiki).exists() {
            match self.create_wiki(
                insights_wiki,
                Some("Auto-generated insights from sessions".to_string()),
            ) {
                Ok(_) | Err(KmsError::WikiAlreadyExists { .. }) => {}
                Err(error) => return Err(error),
            }
        }

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let page_name = format!("insights-{}-{}", timestamp, uuid::Uuid::new_v4());

        let mut content = String::from("# Session Insights\n\n");
        for (i, insight) in insights.iter().enumerate() {
            content.push_str(&format!("{}. {}\n\n", i + 1, insight));
        }

        self.save_page_inner(
            insights_wiki,
            &page_name,
            &content,
            Some(format!("Insights {}", timestamp)),
            vec!["dream".to_string()],
            SaveMode::Create,
        )?;

        Ok(DreamResult {
            insights,
            sessions_analyzed,
            wiki: insights_wiki.to_string(),
            page: page_name,
        })
    }
}
