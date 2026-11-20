//! KMS 名称与现有路径组件校验。

use super::KmsError;
use std::path::Path;

pub(super) fn validate_name(value: &str) -> Result<(), KmsError> {
    if value.is_empty()
        || value.len() > 255
        || value.starts_with('.')
        || value.contains('/')
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || invalid_platform_name(value)
    {
        return Err(KmsError::IoError {
            message: format!("invalid KMS name: {value:?}"),
        });
    }
    Ok(())
}

#[cfg(not(windows))]
fn invalid_platform_name(_value: &str) -> bool {
    false
}

#[cfg(windows)]
fn invalid_platform_name(value: &str) -> bool {
    invalid_windows_name(value)
}

#[cfg(any(windows, test))]
fn invalid_windows_name(value: &str) -> bool {
    if value
        .chars()
        .any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        || value.ends_with(' ')
        || value.ends_with('.')
    {
        return true;
    }
    let stem = value
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    let suffix = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"));
    matches!(
        suffix,
        Some("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
    )
}

fn is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    let is_symlink = metadata.file_type().is_symlink();
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        is_symlink || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        is_symlink
    }
}

/// 拒绝根目录以下已有的符号链接；根目录本身可由用户配置为链接。
pub(super) fn reject_links(root: &Path, relative: &Path) -> Result<(), KmsError> {
    let mut path = root.to_path_buf();
    for part in relative.components() {
        match part {
            std::path::Component::Normal(component) => path.push(component),
            _ => {
                return Err(KmsError::IoError {
                    message: "invalid KMS path".into(),
                });
            }
        }
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if is_link_or_reparse(&metadata) => {
                return Err(KmsError::IoError {
                    message: format!("KMS symbolic link is not allowed: {}", path.display()),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::invalid_windows_name;

    #[test]
    fn windows_device_names_streams_and_ambiguous_names_are_rejected() {
        for name in [
            "CON",
            "nul.txt",
            "COM1",
            "lpt².log",
            "CONOUT$",
            "page:stream",
            "name.",
            "name ",
            "bad?name",
        ] {
            assert!(invalid_windows_name(name), "{name}");
        }
        for name in ["conference", "com10", "lpt0", "normal-page.md"] {
            assert!(!invalid_windows_name(name), "{name}");
        }
    }
}
