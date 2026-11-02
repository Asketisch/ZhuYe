//! 后端选择:`real` feature 开启 → xcap/enigo 真实后端;关闭 → 统一
//! Unavailable 降级(不 panic,工具返回 isError 文案)。

use crate::types::{BackendCapability, ComputerUseAction, MediaError};

/// Computer Use 后端 trait。
pub trait ComputerBackend: Send + Sync {
    /// 当前能力。
    fn capability(&self) -> BackendCapability;
    /// 截屏:返回 PNG bytes(capability == Full / ScreenOnly)。
    fn screenshot(&self) -> Result<Vec<u8>, MediaError>;
    /// 执行一个动作(capability == Full)。
    fn execute(&self, action: &ComputerUseAction) -> Result<(), MediaError>;
}

/// 进程级共享后端(按 feature 决定真实 / 降级)。
pub fn computer_backend() -> std::sync::Arc<dyn ComputerBackend> {
    std::sync::Arc::new(RealComputerBackend)
}

/// 真实后端。`real` feature 关闭时全部操作返回 `Unavailable`。
pub struct RealComputerBackend;

impl ComputerBackend for RealComputerBackend {
    fn capability(&self) -> BackendCapability {
        #[cfg(feature = "real")]
        {
            BackendCapability::Full
        }
        #[cfg(not(feature = "real"))]
        {
            BackendCapability::None
        }
    }

    fn screenshot(&self) -> Result<Vec<u8>, MediaError> {
        #[cfg(feature = "real")]
        {
            let monitors = xcap::Monitor::all().map_err(|e| MediaError::Unavailable {
                capability: "screenshot".into(),
                reason: format!("xcap enumerate failed: {e}"),
            })?;
            let monitor = monitors.first().ok_or_else(|| MediaError::Unavailable {
                capability: "screenshot".into(),
                reason: "no monitors detected".into(),
            })?;
            let rgba = monitor.capture_image().map_err(|e| MediaError::Other {
                message: format!("capture_image failed: {e}"),
            })?;
            let mut buf: Vec<u8> = Vec::new();
            {
                let mut cursor = std::io::Cursor::new(&mut buf);
                rgba.write_to(&mut cursor, image::ImageFormat::Png)
                    .map_err(|e| MediaError::Other {
                        message: format!("png encode failed: {e}"),
                    })?;
            }
            Ok(buf)
        }
        #[cfg(not(feature = "real"))]
        {
            Err(MediaError::Unavailable {
                capability: "screenshot".into(),
                reason: "built without `real` feature (no xcap); rebuild with --features real"
                    .into(),
            })
        }
    }

    fn execute(&self, action: &ComputerUseAction) -> Result<(), MediaError> {
        #[cfg(feature = "real")]
        {
            use enigo::{Axis, Coordinate, Direction, Keyboard, Mouse};
            match action {
                ComputerUseAction::MouseMove { x, y } => {
                    let mut enigo = make_enigo()?;
                    enigo.move_mouse(*x, *y, Coordinate::Abs).map_err(input_err)
                }
                ComputerUseAction::MouseClick { x, y, button } => {
                    let mut enigo = make_enigo()?;
                    enigo.move_mouse(*x, *y, Coordinate::Abs).map_err(input_err)?;
                    let btn = parse_button(button);
                    enigo.button(btn, Direction::Click).map_err(input_err)
                }
                ComputerUseAction::KeyType { text } => {
                    let mut enigo = make_enigo()?;
                    enigo.text(text).map_err(input_err)
                }
                ComputerUseAction::KeyCombo { keys } => {
                    let mut enigo = make_enigo()?;
                    press_combo(&mut enigo, keys)
                }
                ComputerUseAction::Scroll { dx, dy } => {
                    let mut enigo = make_enigo()?;
                    let mut last_err: Option<MediaError> = None;
                    if *dy != 0 {
                        if let Err(e) = enigo.scroll(*dy, Axis::Vertical) {
                            last_err = Some(input_err(e));
                        }
                    }
                    if *dx != 0 {
                        if let Err(e) = enigo.scroll(*dx, Axis::Horizontal) {
                            last_err = Some(input_err(e));
                        }
                    }
                    match last_err {
                        None => Ok(()),
                        Some(e) => Err(e),
                    }
                }
            }
        }
        #[cfg(not(feature = "real"))]
        {
            let _ = action;
            Err(MediaError::Unavailable {
                capability: "mouse_input".into(),
                reason: "built without `real` feature (no enigo); rebuild with --features real"
                    .into(),
            })
        }
    }
}

#[cfg(feature = "real")]
fn make_enigo() -> Result<enigo::Enigo, MediaError> {
    enigo::Enigo::new(&enigo::Settings::default()).map_err(|e| MediaError::Unavailable {
        capability: "input_permission".into(),
        reason: format!("enigo::Enigo::new failed: {e:?}"),
    })
}

#[cfg(feature = "real")]
fn input_err<E: std::fmt::Display>(e: E) -> MediaError {
    MediaError::Other {
        message: format!("input error: {e}"),
    }
}

#[cfg(feature = "real")]
fn parse_button(s: &str) -> enigo::Button {
    match s.to_lowercase().as_str() {
        "right" | "r" => enigo::Button::Right,
        "middle" | "m" => enigo::Button::Middle,
        _ => enigo::Button::Left,
    }
}

/// 组合键:按住所有修饰键 → click 主键 → **无条件**逆序释放修饰键。
#[cfg(feature = "real")]
fn press_combo(enigo: &mut enigo::Enigo, keys: &str) -> Result<(), MediaError> {
    use enigo::{Direction, Key, Keyboard as _};
    let parts: Vec<&str> = keys
        .split('+')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return Err(MediaError::Other {
            message: format!("empty combo: {keys:?}"),
        });
    }
    let parsed: Vec<Key> = parts.iter().map(|p| parse_key(p)).collect();
    let (modifiers, last) = parsed.split_at(parsed.len() - 1);
    let mut press_error = None;
    for k in modifiers {
        if let Err(e) = enigo.key(*k, Direction::Press) {
            press_error = Some(input_err(e));
            break;
        }
    }
    if press_error.is_none() {
        if let Err(e) = enigo.key(last[0], Direction::Click) {
            press_error = Some(input_err(e));
        }
    }
    for k in modifiers.iter().rev() {
        let _ = enigo.key(*k, Direction::Release);
    }
    press_error.map_or(Ok(()), Err)
}

#[cfg(feature = "real")]
fn parse_key(name: &str) -> enigo::Key {
    use enigo::Key;
    let n = name.to_lowercase();
    match n.as_str() {
        "ctrl" | "control" => Key::Control,
        "shift" => Key::Shift,
        "alt" | "option" => Key::Alt,
        "cmd" | "command" | "meta" | "super" => Key::Meta,
        "enter" | "return" => Key::Return,
        "tab" => Key::Tab,
        "esc" | "escape" => Key::Escape,
        "backspace" => Key::Backspace,
        "space" => Key::Space,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "home" => Key::Home,
        "end" => Key::End,
        "delete" | "del" => Key::Delete,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        // 单字符键:enigo 0.6 用 Unicode 形态(Key::Unicode)。
        other => match other.chars().next() {
            Some(c) => Key::Unicode(c.to_uppercase().next().unwrap_or(c)),
            None => Key::Unicode('?'),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn degraded_backend_reports_unavailable_not_panic() {
        // 无 `real` feature(默认)时:能力 None,操作返回 Unavailable。
        let backend = RealComputerBackend;
        if backend.capability() == BackendCapability::None {
            let err = backend.screenshot().unwrap_err();
            assert!(matches!(err, MediaError::Unavailable { capability, .. } if capability == "screenshot"));
            let err = backend
                .execute(&ComputerUseAction::MouseClick {
                    x: 1,
                    y: 1,
                    button: "left".into(),
                })
                .unwrap_err();
            assert!(matches!(err, MediaError::Unavailable { capability, .. } if capability == "mouse_input"));
        }
    }

    #[test]
    fn media_error_display_is_actionable() {
        let e = MediaError::Unavailable {
            capability: "screenshot".into(),
            reason: "no permission".into(),
        };
        assert!(e.to_string().contains("screenshot"));
        assert!(e.to_string().contains("no permission"));
    }
}
