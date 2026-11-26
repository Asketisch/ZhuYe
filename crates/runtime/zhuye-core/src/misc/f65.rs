//! # F65 工程纪律 —— 架构决策记录、错误人话化与行为差异明示
//!
//! - [`AdrEntry`] —— 架构决策记录(ADR)；
//! - [`UserFacingError`] —— 错误信息人话化；
//! - [`ParityDisclosure`] —— CLI↔GUI 行为差异明示。

use serde::{Deserialize, Serialize};

/// ADR 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdrStatus {
    Proposed,
    Accepted,
    Deprecated,
    Superseded,
}

impl AdrStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AdrStatus::Proposed => "proposed",
            AdrStatus::Accepted => "accepted",
            AdrStatus::Deprecated => "deprecated",
            AdrStatus::Superseded => "superseded",
        }
    }
}

/// 一条架构决策记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdrEntry {
    /// 编号(`"0001"`)。
    pub number: String,
    pub title: String,
    pub status: AdrStatus,
    /// 背景(为什么需要决策)。
    pub context: String,
    /// 决策内容。
    pub decision: String,
    /// 后果(正面 + 负面)。
    #[serde(default)]
    pub consequences: Vec<String>,
    /// 被谁取代。
    #[serde(default)]
    pub superseded_by: Option<String>,
}

impl AdrEntry {
    /// 渲染成 Markdown(标准 ADR 格式)。
    pub fn render(&self) -> String {
        let mut out = format!("# {}. {}\n\n", self.number, self.title);
        out.push_str(&format!("- 状态:{}\n\n", self.status.as_str()));
        out.push_str(&format!("## 背景\n\n{}\n\n", self.context));
        out.push_str(&format!("## 决策\n\n{}\n\n", self.decision));
        if !self.consequences.is_empty() {
            out.push_str("## 后果\n\n");
            for c in &self.consequences {
                out.push_str(&format!("- {c}\n"));
            }
            out.push('\n');
        }
        if let Some(by) = &self.superseded_by {
            out.push_str(&format!("> 已被 ADR-{by} 取代\n"));
        }
        out
    }
}

/// 人话错误。
///
/// 三层:用户看到的(人话)、开发者看的(技术细节)、下一步(可操作建议)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserFacingError {
    /// 用户可读的一句话。
    pub message: String,
    /// 技术细节(折叠显示)。
    #[serde(default)]
    pub technical: Option<String>,
    /// 建议动作。
    #[serde(default)]
    pub next_steps: Vec<String>,
}

impl UserFacingError {
    pub fn new(message: impl Into<String>) -> UserFacingError {
        UserFacingError {
            message: message.into(),
            technical: None,
            next_steps: Vec::new(),
        }
    }

    pub fn with_technical(mut self, detail: impl Into<String>) -> UserFacingError {
        self.technical = Some(detail.into());
        self
    }

    pub fn with_next_step(mut self, step: impl Into<String>) -> UserFacingError {
        self.next_steps.push(step.into());
        self
    }

    /// 渲染(CLI 用)。
    pub fn render(&self) -> String {
        let mut out = format!("错误:{}\n", self.message);
        if let Some(t) = &self.technical {
            out.push_str(&format!("详情:{t}\n"));
        }
        if !self.next_steps.is_empty() {
            out.push_str("可以试试:\n");
            for s in &self.next_steps {
                out.push_str(&format!("  - {s}\n"));
            }
        }
        out
    }

    /// 是否含技术细节(UI 决定是否折叠)。
    pub fn has_technical(&self) -> bool {
        self.technical.is_some()
    }
}

/// CLI↔GUI 行为差异明示。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParityDisclosure {
    /// 功能名。
    pub feature: String,
    /// CLI 行为。
    pub cli_behavior: String,
    /// GUI 行为。
    pub gui_behavior: String,
    /// 差异原因(为什么不能一致)。
    #[serde(default)]
    pub reason: Option<String>,
}

impl ParityDisclosure {
    /// 是否有差异。
    pub fn differs(&self) -> bool {
        self.cli_behavior != self.gui_behavior
    }

    /// 渲染一行说明。
    pub fn render(&self) -> String {
        if !self.differs() {
            return format!("{}:两端行为一致({})", self.feature, self.cli_behavior);
        }
        let reason = self.reason.as_deref().unwrap_or("未说明");
        format!(
            "{}:CLI {} · GUI {}(原因:{})",
            self.feature, self.cli_behavior, self.gui_behavior, reason
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adr_render() {
        let adr = AdrEntry {
            number: "0001".into(),
            title: "采用 tree-sitter 而非正则做符号编辑".into(),
            status: AdrStatus::Accepted,
            context: "正则改符号会误伤注释".into(),
            decision: "用 tree-sitter".into(),
            consequences: vec!["依赖变重".into(), "准确性大幅提升".into()],
            superseded_by: None,
        };
        let md = adr.render();
        assert!(md.contains("# 0001."), "{md}");
        assert!(md.contains("状态:accepted"), "{md}");
        assert!(md.contains("## 背景"), "{md}");
        assert!(md.contains("## 决策"), "{md}");
        assert!(md.contains("依赖变重"), "{md}");
    }

    #[test]
    fn adr_superseded_note() {
        let adr = AdrEntry {
            number: "0002".into(),
            title: "t".into(),
            status: AdrStatus::Superseded,
            context: "c".into(),
            decision: "d".into(),
            consequences: vec![],
            superseded_by: Some("0003".into()),
        };
        assert!(adr.render().contains("已被 ADR-0003 取代"));
    }

    #[test]
    fn user_facing_error_render() {
        let err = UserFacingError::new("无法连接到模型服务")
            .with_technical("reqwest: connection refused (os error 61)")
            .with_next_step("检查网络连接")
            .with_next_step("确认 API key 有效");
        let rendered = err.render();
        assert!(rendered.contains("无法连接到模型服务"), "{rendered}");
        assert!(rendered.contains("connection refused"), "{rendered}");
        assert!(rendered.contains("检查网络连接"), "{rendered}");
        assert!(rendered.contains("确认 API key"), "{rendered}");
        assert!(err.has_technical());
    }

    #[test]
    fn user_facing_error_minimal() {
        let err = UserFacingError::new("出错了");
        assert!(!err.has_technical());
        assert!(err.render().contains("出错了"));
    }

    #[test]
    fn parity_disclosure_differs() {
        let same = ParityDisclosure {
            feature: "x".into(),
            cli_behavior: "y".into(),
            gui_behavior: "y".into(),
            reason: None,
        };
        assert!(!same.differs());
        assert!(same.render().contains("行为一致"));

        let diff = ParityDisclosure {
            feature: "审批".into(),
            cli_behavior: "键盘 y/n".into(),
            gui_behavior: "点击按钮".into(),
            reason: Some("交互形态不同".into()),
        };
        assert!(diff.differs());
        let rendered = diff.render();
        assert!(rendered.contains("键盘 y/n"), "{rendered}");
        assert!(rendered.contains("点击按钮"), "{rendered}");
        assert!(rendered.contains("交互形态不同"), "{rendered}");
    }

    #[test]
    fn adr_serializes() {
        let adr = AdrEntry {
            number: "1".into(),
            title: "t".into(),
            status: AdrStatus::Proposed,
            context: "c".into(),
            decision: "d".into(),
            consequences: vec![],
            superseded_by: None,
        };
        let json = serde_json::to_string(&adr).expect("serialize");
        let back: AdrEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, adr);
    }
}
