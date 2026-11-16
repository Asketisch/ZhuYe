//! # 浏览器与桌面自动化的元素寻址(F39 / F40)
//!
//! F39(浏览器)与 F40(computer-use)的共同核心是**寻址方式**:
//!
//! - 旧方式:截图 + 坐标 `(x=512, y=380)` —— 分辨率/DPI 一变就失效,
//!   模型也看不出点到了什么;
//! - 新方式:可及性(a11y)树 + **元素引用** `@e12` —— 按语义定位,
//!   跨分辨率稳定,且能验证"点对了没"。
//!
//! ## 三块
//!
//! 1. **a11y 树快照**([`A11ySnapshot`])—— 把界面/页面拍成扁平元素列表,
//!    每个元素有稳定 ref(`@eN`)、角色、名称、可交互性。
//! 2. **快照绑定**([`SnapshotBinding`])—— ref 只在**某次快照**内有效。
//!    界面变了旧 ref 必须失效 —— 否则会点到错的地方(比坐标更危险,
//!    因为模型"以为"自己点对了)。
//! 3. **动作后验**([`verify_action`])—— 动作后重新快照,比对预期变化
//!    ("点了提交,应该出现成功提示")。
//!
//! ## 为什么 ref 必须绑定快照
//!
//! 坐标失效最多是"点歪了"(看得见)。ref 失效是"点到了另一个同名按钮"
//! (看不见) —— 后者更危险。绑定快照 + 版本号让误用变成**显式错误**。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 元素角色(a11y role 的常用子集)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElementRole {
    Button,
    Link,
    TextField,
    Checkbox,
    Radio,
    Combobox,
    MenuItem,
    Tab,
    Heading,
    Text,
    Image,
    Dialog,
    Other,
}

impl ElementRole {
    pub fn as_str(self) -> &'static str {
        match self {
            ElementRole::Button => "button",
            ElementRole::Link => "link",
            ElementRole::TextField => "text_field",
            ElementRole::Checkbox => "checkbox",
            ElementRole::Radio => "radio",
            ElementRole::Combobox => "combobox",
            ElementRole::MenuItem => "menu_item",
            ElementRole::Tab => "tab",
            ElementRole::Heading => "heading",
            ElementRole::Text => "text",
            ElementRole::Image => "image",
            ElementRole::Dialog => "dialog",
            ElementRole::Other => "other",
        }
    }

    /// 该角色是否可交互(可点击 / 可输入)。
    pub fn is_interactive(self) -> bool {
        matches!(
            self,
            ElementRole::Button
                | ElementRole::Link
                | ElementRole::TextField
                | ElementRole::Checkbox
                | ElementRole::Radio
                | ElementRole::Combobox
                | ElementRole::MenuItem
                | ElementRole::Tab
        )
    }
}

/// 一个 a11y 元素。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct A11yElement {
    /// 稳定引用(`@e12`)。
    pub reference: String,
    pub role: ElementRole,
    /// 可读名称(按钮文案 / 输入框 label)。
    pub name: String,
    /// 是否禁用。
    #[serde(default)]
    pub disabled: bool,
    /// 是否可见。
    #[serde(default = "default_true")]
    pub visible: bool,
    /// 当前值(输入框文本 / 选中状态)。
    #[serde(default)]
    pub value: Option<String>,
    /// 层级深度(用于渲染缩进)。
    #[serde(default)]
    pub depth: usize,
}

fn default_true() -> bool {
    true
}

impl A11yElement {
    /// 是否可操作(可见 + 未禁用 + 角色可交互)。
    pub fn is_actionable(&self) -> bool {
        self.visible && !self.disabled && self.role.is_interactive()
    }

    /// 渲染成一行(供模型读)。
    pub fn render_line(&self) -> String {
        let indent = "  ".repeat(self.depth);
        let mut out = format!(
            "{indent}{} {} {:?}",
            self.reference,
            self.role.as_str(),
            self.name
        );
        if let Some(value) = &self.value {
            out.push_str(&format!(" = {value:?}"));
        }
        if self.disabled {
            out.push_str(" [disabled]");
        }
        out
    }
}

/// a11y 快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct A11ySnapshot {
    /// 快照版本(单调递增)。ref 只在本版本内有效。
    pub version: u64,
    /// 页面 / 窗口标题。
    pub title: String,
    pub elements: Vec<A11yElement>,
}

impl A11ySnapshot {
    pub fn new(version: u64, title: impl Into<String>, elements: Vec<A11yElement>) -> A11ySnapshot {
        A11ySnapshot {
            version,
            title: title.into(),
            elements,
        }
    }

    /// 按 ref 查元素。
    pub fn find(&self, reference: &str) -> Option<&A11yElement> {
        self.elements.iter().find(|e| e.reference == reference)
    }

    /// 按名称模糊查(大小写不敏感子串)。
    pub fn find_by_name(&self, name: &str) -> Vec<&A11yElement> {
        let needle = name.to_lowercase();
        self.elements
            .iter()
            .filter(|e| e.name.to_lowercase().contains(&needle))
            .collect()
    }

    /// 可交互元素(模型最常需要看的)。
    pub fn actionable(&self) -> Vec<&A11yElement> {
        self.elements.iter().filter(|e| e.is_actionable()).collect()
    }

    /// 渲染成模型可读的树形文本。
    pub fn render(&self) -> String {
        let mut out = format!("[{}] {}\n", self.version, self.title);
        for e in &self.elements {
            out.push_str(&e.render_line());
            out.push('\n');
        }
        out
    }

    /// 元素数。
    pub fn len(&self) -> usize {
        self.elements.len()
    }

    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }
}

/// 快照绑定(防旧 ref 误用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotBinding {
    /// 当前有效的快照版本。
    pub version: u64,
}

impl SnapshotBinding {
    pub fn new(version: u64) -> SnapshotBinding {
        SnapshotBinding { version }
    }

    /// 校验 ref 属于当前快照。
    ///
    /// `ref_version` 是 ref 被记录时的快照版本。
    pub fn check(&self, reference: &str, ref_version: u64) -> Result<(), StaleRefError> {
        if ref_version != self.version {
            return Err(StaleRefError {
                reference: reference.to_string(),
                ref_version,
                current_version: self.version,
            });
        }
        Ok(())
    }
}

/// 陈旧 ref 错误。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "stale element reference {reference}: captured at snapshot v{ref_version}, current is v{current_version}; take a fresh snapshot"
)]
pub struct StaleRefError {
    pub reference: String,
    pub ref_version: u64,
    pub current_version: u64,
}

/// 动作类型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiAction {
    Click { reference: String },
    Fill { reference: String, text: String },
    Select { reference: String, option: String },
    PressKey { key: String },
    Scroll { direction: String, amount: u32 },
}

impl UiAction {
    /// 动作涉及的 ref(键盘/滚动无 ref)。
    pub fn reference(&self) -> Option<&str> {
        match self {
            UiAction::Click { reference }
            | UiAction::Fill { reference, .. }
            | UiAction::Select { reference, .. } => Some(reference),
            UiAction::PressKey { .. } | UiAction::Scroll { .. } => None,
        }
    }

    /// 人读描述。
    pub fn describe(&self) -> String {
        match self {
            UiAction::Click { reference } => format!("点击 {reference}"),
            UiAction::Fill { reference, text } => format!("在 {reference} 输入 {text:?}"),
            UiAction::Select { reference, option } => {
                format!("在 {reference} 选择 {option:?}")
            }
            UiAction::PressKey { key } => format!("按键 {key}"),
            UiAction::Scroll { direction, amount } => format!("向 {direction} 滚动 {amount}"),
        }
    }
}

/// 后验断言。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerifyExpectation {
    /// 期望出现某名称的元素。
    ElementAppears { name: String },
    /// 期望某名称的元素消失。
    ElementDisappears { name: String },
    /// 期望某元素的 value 变成指定值。
    ValueBecomes { reference: String, value: String },
    /// 期望标题包含某文本。
    TitleContains { text: String },
}

/// 后验结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyResult {
    pub passed: bool,
    pub detail: String,
}

/// 动作后验:比对动作前后的快照。
///
/// 这是 F40「能验算点对了没」的实现 —— 没有它,模型只能假设动作成功了。
pub fn verify_action(
    before: &A11ySnapshot,
    after: &A11ySnapshot,
    expectation: &VerifyExpectation,
) -> VerifyResult {
    match expectation {
        VerifyExpectation::ElementAppears { name } => {
            let existed = !before.find_by_name(name).is_empty();
            let exists = !after.find_by_name(name).is_empty();
            if exists && !existed {
                VerifyResult {
                    passed: true,
                    detail: format!("元素 {name:?} 已出现"),
                }
            } else if exists {
                VerifyResult {
                    passed: false,
                    detail: format!("元素 {name:?} 动作前就已存在,无法确认是本次动作的效果"),
                }
            } else {
                VerifyResult {
                    passed: false,
                    detail: format!("元素 {name:?} 未出现"),
                }
            }
        }
        VerifyExpectation::ElementDisappears { name } => {
            let exists = !after.find_by_name(name).is_empty();
            VerifyResult {
                passed: !exists,
                detail: if exists {
                    format!("元素 {name:?} 仍然存在")
                } else {
                    format!("元素 {name:?} 已消失")
                },
            }
        }
        VerifyExpectation::ValueBecomes { reference, value } => match after.find(reference) {
            Some(el) if el.value.as_deref() == Some(value.as_str()) => VerifyResult {
                passed: true,
                detail: format!("{reference} 的值已变为 {value:?}"),
            },
            Some(el) => VerifyResult {
                passed: false,
                detail: format!(
                    "{reference} 的值为 {:?},期望 {value:?}",
                    el.value.as_deref().unwrap_or("(无)")
                ),
            },
            None => VerifyResult {
                passed: false,
                detail: format!("{reference} 不存在于新快照"),
            },
        },
        VerifyExpectation::TitleContains { text } => {
            let ok = after.title.contains(text.as_str());
            VerifyResult {
                passed: ok,
                detail: if ok {
                    format!("标题含 {text:?}")
                } else {
                    format!("标题 {:?} 不含 {text:?}", after.title)
                },
            }
        }
    }
}

/// 快照差异摘要(供 UI 高亮变化)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotDiff {
    pub appeared: Vec<String>,
    pub disappeared: Vec<String>,
    pub value_changed: BTreeMap<String, (Option<String>, Option<String>)>,
}

impl SnapshotDiff {
    /// 是否有变化。
    pub fn is_empty(&self) -> bool {
        self.appeared.is_empty() && self.disappeared.is_empty() && self.value_changed.is_empty()
    }

    /// 人读摘要。
    pub fn summary(&self) -> String {
        if self.is_empty() {
            return "界面无变化".to_string();
        }
        let mut parts = Vec::new();
        if !self.appeared.is_empty() {
            parts.push(format!("+{} 元素", self.appeared.len()));
        }
        if !self.disappeared.is_empty() {
            parts.push(format!("-{} 元素", self.disappeared.len()));
        }
        if !self.value_changed.is_empty() {
            parts.push(format!("~{} 值变更", self.value_changed.len()));
        }
        parts.join(" · ")
    }
}

/// 计算两个快照的差异(按名称 + 角色匹配)。
pub fn diff_snapshots(before: &A11ySnapshot, after: &A11ySnapshot) -> SnapshotDiff {
    let key = |e: &A11yElement| format!("{}:{}", e.role.as_str(), e.name);
    let before_map: BTreeMap<String, &A11yElement> =
        before.elements.iter().map(|e| (key(e), e)).collect();
    let after_map: BTreeMap<String, &A11yElement> =
        after.elements.iter().map(|e| (key(e), e)).collect();

    let mut diff = SnapshotDiff::default();
    for (k, after_el) in &after_map {
        match before_map.get(k) {
            None => diff.appeared.push(k.clone()),
            Some(before_el) => {
                if before_el.value != after_el.value {
                    diff.value_changed
                        .insert(k.clone(), (before_el.value.clone(), after_el.value.clone()));
                }
            }
        }
    }
    for k in before_map.keys() {
        if !after_map.contains_key(k) {
            diff.disappeared.push(k.clone());
        }
    }
    diff.appeared.sort();
    diff.disappeared.sort();
    diff
}

/// 浏览器安全约束(F39)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSecurity {
    /// 仅允许单个 target(防误操作多标签)。
    pub single_target: bool,
    /// 是否启用 CDP 鉴权 token。
    pub require_token: bool,
    /// 仅允许回环连接(不连远程调试端口)。
    pub loopback_only: bool,
}

impl Default for BrowserSecurity {
    fn default() -> Self {
        BrowserSecurity {
            single_target: true,
            require_token: true,
            loopback_only: true,
        }
    }
}

impl BrowserSecurity {
    /// 校验连接参数是否安全。
    pub fn validate(&self, target_count: usize, host: &str, has_token: bool) -> Result<(), String> {
        if self.single_target && target_count > 1 {
            return Err(format!(
                "检测到 {target_count} 个浏览器 target,但配置要求单 target"
            ));
        }
        if self.require_token && !has_token {
            return Err("缺少 CDP 鉴权 token".to_string());
        }
        if self.loopback_only {
            let is_loopback = host == "127.0.0.1" || host == "localhost" || host == "::1";
            if !is_loopback {
                return Err(format!("CDP 目标 {host} 非回环地址,拒绝连接"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(reference: &str, role: ElementRole, name: &str) -> A11yElement {
        A11yElement {
            reference: reference.into(),
            role,
            name: name.into(),
            disabled: false,
            visible: true,
            value: None,
            depth: 0,
        }
    }

    fn snapshot(version: u64, elements: Vec<A11yElement>) -> A11ySnapshot {
        A11ySnapshot::new(version, "Test Page", elements)
    }

    #[test]
    fn role_interactivity() {
        assert!(ElementRole::Button.is_interactive());
        assert!(ElementRole::TextField.is_interactive());
        assert!(!ElementRole::Heading.is_interactive());
        assert!(!ElementRole::Text.is_interactive());
    }

    #[test]
    fn element_actionable_rules() {
        let mut e = el("@e1", ElementRole::Button, "Submit");
        assert!(e.is_actionable());
        e.disabled = true;
        assert!(!e.is_actionable(), "禁用不可操作");
        e.disabled = false;
        e.visible = false;
        assert!(!e.is_actionable(), "不可见不可操作");
    }

    #[test]
    fn snapshot_find_and_find_by_name() {
        let snap = snapshot(
            1,
            vec![
                el("@e1", ElementRole::Button, "Submit"),
                el("@e2", ElementRole::Link, "Cancel"),
                el("@e3", ElementRole::Button, "Submit All"),
            ],
        );
        assert_eq!(snap.find("@e1").unwrap().name, "Submit");
        assert!(snap.find("@e99").is_none());
        assert_eq!(snap.find_by_name("submit").len(), 2, "大小写不敏感子串");
    }

    #[test]
    fn snapshot_actionable_filter() {
        let mut disabled = el("@e2", ElementRole::Button, "Disabled");
        disabled.disabled = true;
        let snap = snapshot(
            1,
            vec![
                el("@e1", ElementRole::Button, "OK"),
                disabled,
                el("@e3", ElementRole::Heading, "Title"),
            ],
        );
        let actionable = snap.actionable();
        assert_eq!(actionable.len(), 1);
        assert_eq!(actionable[0].reference, "@e1");
    }

    #[test]
    fn snapshot_render_has_refs() {
        let snap = snapshot(3, vec![el("@e1", ElementRole::Button, "Submit")]);
        let rendered = snap.render();
        assert!(rendered.contains("[3]"), "{rendered}");
        assert!(rendered.contains("@e1"), "{rendered}");
        assert!(rendered.contains("button"), "{rendered}");
    }

    #[test]
    fn stale_ref_detected() {
        let binding = SnapshotBinding::new(5);
        assert!(binding.check("@e1", 5).is_ok());
        let err = binding.check("@e1", 4).unwrap_err();
        assert_eq!(err.ref_version, 4);
        assert_eq!(err.current_version, 5);
        assert!(err.to_string().contains("stale"), "{err}");
    }

    #[test]
    fn action_reference_extraction() {
        assert_eq!(
            UiAction::Click {
                reference: "@e1".into()
            }
            .reference(),
            Some("@e1")
        );
        assert_eq!(
            UiAction::PressKey {
                key: "Enter".into()
            }
            .reference(),
            None
        );
        assert_eq!(
            UiAction::Scroll {
                direction: "down".into(),
                amount: 3
            }
            .reference(),
            None
        );
    }

    #[test]
    fn action_describe() {
        let a = UiAction::Fill {
            reference: "@e1".into(),
            text: "hello".into(),
        };
        assert!(a.describe().contains("@e1"), "{}", a.describe());
        assert!(a.describe().contains("hello"));
    }

    #[test]
    fn verify_element_appears() {
        let before = snapshot(1, vec![]);
        let after = snapshot(2, vec![el("@e1", ElementRole::Text, "Success!")]);
        let result = verify_action(
            &before,
            &after,
            &VerifyExpectation::ElementAppears {
                name: "Success".into(),
            },
        );
        assert!(result.passed, "{}", result.detail);
    }

    #[test]
    fn verify_element_already_existed_is_inconclusive() {
        let before = snapshot(1, vec![el("@e1", ElementRole::Text, "Success!")]);
        let after = snapshot(2, vec![el("@e1", ElementRole::Text, "Success!")]);
        let result = verify_action(
            &before,
            &after,
            &VerifyExpectation::ElementAppears {
                name: "Success".into(),
            },
        );
        assert!(!result.passed, "动作前已存在 → 无法归因于本次动作");
        assert!(result.detail.contains("已存在"), "{}", result.detail);
    }

    #[test]
    fn verify_element_disappears() {
        let before = snapshot(1, vec![el("@e1", ElementRole::Dialog, "Modal")]);
        let after = snapshot(2, vec![]);
        let result = verify_action(
            &before,
            &after,
            &VerifyExpectation::ElementDisappears {
                name: "Modal".into(),
            },
        );
        assert!(result.passed);
    }

    #[test]
    fn verify_value_becomes() {
        let mut before_el = el("@e1", ElementRole::TextField, "Email");
        before_el.value = Some("".into());
        let mut after_el = el("@e1", ElementRole::TextField, "Email");
        after_el.value = Some("a@b.com".into());
        let before = snapshot(1, vec![before_el]);
        let after = snapshot(2, vec![after_el]);
        let result = verify_action(
            &before,
            &after,
            &VerifyExpectation::ValueBecomes {
                reference: "@e1".into(),
                value: "a@b.com".into(),
            },
        );
        assert!(result.passed, "{}", result.detail);
    }

    #[test]
    fn verify_value_mismatch() {
        let mut el_ = el("@e1", ElementRole::TextField, "Email");
        el_.value = Some("wrong".into());
        let after = snapshot(2, vec![el_]);
        let result = verify_action(
            &snapshot(1, vec![]),
            &after,
            &VerifyExpectation::ValueBecomes {
                reference: "@e1".into(),
                value: "expected".into(),
            },
        );
        assert!(!result.passed);
    }

    #[test]
    fn verify_title_contains() {
        let before = snapshot(1, vec![]);
        let mut after = snapshot(2, vec![]);
        after.title = "Dashboard - Loaded".into();
        let result = verify_action(
            &before,
            &after,
            &VerifyExpectation::TitleContains {
                text: "Loaded".into(),
            },
        );
        assert!(result.passed);
    }

    #[test]
    fn diff_detects_appear_disappear_change() {
        let mut before_el = el("@e1", ElementRole::TextField, "Name");
        before_el.value = Some("old".into());
        let before = snapshot(1, vec![before_el, el("@e2", ElementRole::Button, "Gone")]);

        let mut after_el = el("@e1", ElementRole::TextField, "Name");
        after_el.value = Some("new".into());
        let after = snapshot(2, vec![after_el, el("@e3", ElementRole::Button, "New")]);

        let diff = diff_snapshots(&before, &after);
        assert_eq!(diff.appeared.len(), 1);
        assert!(diff.appeared[0].contains("New"));
        assert_eq!(diff.disappeared.len(), 1);
        assert!(diff.disappeared[0].contains("Gone"));
        assert_eq!(diff.value_changed.len(), 1);
        assert!(!diff.is_empty());
        assert!(diff.summary().contains('+'), "{}", diff.summary());
    }

    #[test]
    fn diff_no_change() {
        let snap = snapshot(1, vec![el("@e1", ElementRole::Button, "X")]);
        let diff = diff_snapshots(&snap, &snap);
        assert!(diff.is_empty());
        assert!(diff.summary().contains("无变化"));
    }

    #[test]
    fn browser_security_defaults_are_strict() {
        let sec = BrowserSecurity::default();
        assert!(sec.single_target);
        assert!(sec.require_token);
        assert!(sec.loopback_only);
    }

    #[test]
    fn browser_security_validation() {
        let sec = BrowserSecurity::default();
        assert!(sec.validate(1, "127.0.0.1", true).is_ok());
        assert!(
            sec.validate(2, "127.0.0.1", true).is_err(),
            "多 target 应拒"
        );
        assert!(
            sec.validate(1, "127.0.0.1", false).is_err(),
            "缺 token 应拒"
        );
        assert!(sec.validate(1, "192.168.1.5", true).is_err(), "非回环应拒");
    }

    #[test]
    fn snapshot_serializes() {
        let snap = snapshot(1, vec![el("@e1", ElementRole::Button, "X")]);
        let json = serde_json::to_string(&snap).expect("serialize");
        let back: A11ySnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, snap);
    }

    #[test]
    fn action_serializes_with_tag() {
        let a = UiAction::Click {
            reference: "@e1".into(),
        };
        let json = serde_json::to_string(&a).expect("serialize");
        assert!(json.contains("\"kind\":\"click\""), "{json}");
        let back: UiAction = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, a);
    }
}
