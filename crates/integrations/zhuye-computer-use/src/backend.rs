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
        action.validate()?;
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
                    enigo
                        .move_mouse(*x, *y, Coordinate::Abs)
                        .map_err(input_err)?;
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
                        last_err = enigo.scroll(*dy, Axis::Vertical).err().map(input_err);
                    }
                    if *dx != 0 {
                        last_err = enigo
                            .scroll(*dx, Axis::Horizontal)
                            .err()
                            .map(input_err)
                            .or(last_err);
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

/// 组合键:依次按住修饰键、点击主键,再逆序释放成功按下的修饰键。
#[cfg(feature = "real")]
fn press_combo(enigo: &mut enigo::Enigo, keys: &str) -> Result<(), MediaError> {
    use enigo::{Key, Keyboard as _};
    let parsed: Vec<Key> = keys.split('+').map(|key| parse_key(key.trim())).collect();
    press_parsed_combo(&parsed, |key, direction| {
        enigo.key(key, direction).map_err(input_err)
    })
}

#[cfg(feature = "real")]
fn press_parsed_combo(
    keys: &[enigo::Key],
    mut send: impl FnMut(enigo::Key, enigo::Direction) -> Result<(), MediaError>,
) -> Result<(), MediaError> {
    use enigo::Direction;
    let Some((&last, modifiers)) = keys.split_last() else {
        return Err(MediaError::Other {
            message: "空组合键".into(),
        });
    };
    let mut pressed = Vec::new();
    let mut first_error = None;
    for &key in modifiers {
        match send(key, Direction::Press) {
            Ok(()) => pressed.push(key),
            Err(error) => {
                first_error = Some(error);
                break;
            }
        }
    }
    if first_error.is_none() {
        first_error = send(last, Direction::Click).err();
    }
    for key in pressed.into_iter().rev() {
        first_error = first_error.or(send(key, Direction::Release).err());
    }
    first_error.map_or(Ok(()), Err)
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
        _ => match name.chars().next() {
            Some(c) => Key::Unicode(c),
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
            assert!(
                matches!(err, MediaError::Unavailable { capability, .. } if capability == "screenshot")
            );
            let err = backend
                .execute(&ComputerUseAction::MouseClick {
                    x: 1,
                    y: 1,
                    button: "left".into(),
                })
                .unwrap_err();
            assert!(
                matches!(err, MediaError::Unavailable { capability, .. } if capability == "mouse_input")
            );
        }
    }

    #[cfg(feature = "real")]
    #[test]
    fn failed_combo_press_releases_only_successfully_pressed_modifiers() {
        use enigo::{Direction, Key};
        let mut sent = Vec::new();
        let result = press_parsed_combo(
            &[Key::Control, Key::Shift, Key::Unicode('a')],
            |key, direction| {
                sent.push((key, direction));
                if key == Key::Shift && direction == Direction::Press {
                    Err(MediaError::Other {
                        message: "按键失败".into(),
                    })
                } else {
                    Ok(())
                }
            },
        );
        assert!(result.is_err());
        assert_eq!(
            sent,
            [
                (Key::Control, Direction::Press),
                (Key::Shift, Direction::Press),
                (Key::Control, Direction::Release)
            ]
        );
    }

    #[cfg(feature = "real")]
    #[test]
    fn combo_release_failure_is_reported_after_remaining_releases() {
        use enigo::{Direction, Key};
        let mut sent = Vec::new();
        let result = press_parsed_combo(
            &[Key::Control, Key::Shift, Key::Unicode('a')],
            |key, direction| {
                sent.push((key, direction));
                if key == Key::Shift && direction == Direction::Release {
                    Err(MediaError::Other {
                        message: "释放失败".into(),
                    })
                } else {
                    Ok(())
                }
            },
        );
        assert!(result.is_err());
        assert_eq!(sent.last(), Some(&(Key::Control, Direction::Release)));
    }

    #[cfg(feature = "real")]
    #[test]
    fn single_character_key_preserves_original_unicode() {
        assert_eq!(parse_key("a"), enigo::Key::Unicode('a'));
        assert_eq!(parse_key("A"), enigo::Key::Unicode('A'));
        assert_eq!(parse_key("ß"), enigo::Key::Unicode('ß'));
        assert_eq!(parse_key("CTRL"), enigo::Key::Control);
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
