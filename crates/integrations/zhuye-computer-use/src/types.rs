//! 共享类型(WP15 收编)。

use serde::{Deserialize, Serialize};

/// 后端能力等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendCapability {
    /// 无能力(降级态;所有操作返回 Unavailable)。
    None,
    /// 截屏可用(输入不可用,如缺辅助功能权限)。
    ScreenOnly,
    /// 截屏 + 鼠标键盘输入全可用。
    Full,
}

/// Computer Use 动作。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ComputerUseAction {
    /// 鼠标移动到 `(x, y)` 屏幕坐标(绝对)。
    MouseMove { x: i32, y: i32 },
    /// 鼠标点击(默认左键)。
    MouseClick {
        x: i32,
        y: i32,
        /// `"left"` / `"right"` / `"middle"`(默认 left)。
        #[serde(default = "default_button")]
        button: String,
    },
    /// 键入文本(经输入法语义;组合键用 [`ComputerUseAction::KeyCombo`])。
    KeyType { text: String },
    /// 组合键(`"ctrl+c"` / `"cmd+shift+p"`;修饰键按住 → click 主键 →
    /// 逆序释放,主键 click 失败也必须释放修饰键 —— 否则 Ctrl/Cmd 卡在
    /// OS 层直到用户手动再按)。
    KeyCombo { keys: String },
    /// 滚轮(正 = 上 / 右)。
    Scroll { dx: i32, dy: i32 },
}

impl ComputerUseAction {
    /// 在访问桌面前拒绝无法明确解释的鼠标按钮和按键。
    pub fn validate(&self) -> Result<(), MediaError> {
        match self {
            Self::MouseClick { button, .. } => {
                if !matches!(
                    button.to_ascii_lowercase().as_str(),
                    "left" | "l" | "right" | "r" | "middle" | "m"
                ) {
                    return Err(MediaError::Other {
                        message: format!("无效鼠标按钮: {button:?}"),
                    });
                }
            }
            Self::KeyCombo { keys } => {
                for part in keys.split('+') {
                    let key = part.trim();
                    let name = key.to_ascii_lowercase();
                    let named = matches!(
                        name.as_str(),
                        "ctrl"
                            | "control"
                            | "shift"
                            | "alt"
                            | "option"
                            | "cmd"
                            | "command"
                            | "meta"
                            | "super"
                            | "enter"
                            | "return"
                            | "tab"
                            | "esc"
                            | "escape"
                            | "backspace"
                            | "space"
                            | "left"
                            | "right"
                            | "up"
                            | "down"
                            | "home"
                            | "end"
                            | "delete"
                            | "del"
                            | "f1"
                            | "f2"
                            | "f3"
                            | "f4"
                            | "f5"
                            | "f6"
                            | "f7"
                            | "f8"
                            | "f9"
                            | "f10"
                            | "f11"
                            | "f12"
                    );
                    if !named && (key.chars().count() != 1 || key.chars().any(char::is_control)) {
                        return Err(MediaError::Other {
                            message: format!("无效组合键: {keys:?}"),
                        });
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn default_button() -> String {
    "left".into()
}

/// Media 错误。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MediaError {
    /// 后端不可用(headless / 权限缺失 / 缺 platform deps / feature 关)。
    Unavailable {
        /// 缺失能力名:`"screenshot"` / `"mouse_input"` / `"input_permission"`。
        capability: String,
        /// 原因(诊断)。
        reason: String,
    },
    /// 其它运行时错误(截屏失败 / 输入注入失败)。
    Other { message: String },
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaError::Unavailable { capability, reason } => {
                write!(f, "computer-use 后端不可用({capability}): {reason}")
            }
            MediaError::Other { message } => write!(f, "computer-use 错误: {message}"),
        }
    }
}

impl std::error::Error for MediaError {}
