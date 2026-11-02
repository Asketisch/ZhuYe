//! 数据目录解析 —— 全框架唯一的 home 口径（单一事实源）。
//!
//! 历史上 `default_config_path`（本 crate）、`resolve_notes_home`
//! （zhuye-notes）、`FileTaskStore::with_default_home`（zhuye-task）、
//! `zhuye_home`（zhuye-prompt / zhuye-cli）各自解析一遍
//! `$ZHUYE_HOME` > `$HOME/.zhuye`，口径已漂移多次（空串算不算设置、
//! 要不要 getpwuid 兜底、错误信息各写各的）。数据目录（会话 / 记忆 /
//! 任务 / 配置……）与配置文件必须落进同一个根，否则同一台机器上的
//! 两个客户端会各看一套数据。
//!
//! 本模块只**读**环境变量，不写任何变量 —— 改环境是启动方的事。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::DEFAULT_CONFIG_DIR;

/// 用户级数据目录：`$ZHUYE_HOME` > `$HOME/.zhuye`。
///
/// `ZHUYE_HOME` 指向的**就是**数据目录本身（其下直接放 `config.toml`），
/// 与 `default_config_path` 同语义；空值按未设置处理。拿不到任何 home
/// 时返回 `None`，调用方按各自的降级策略处理（不要 panic）。
pub fn data_dir() -> Option<PathBuf> {
    resolve_data_dir(std::env::var_os("ZHUYE_HOME"), std::env::var_os("HOME"))
}

/// [`data_dir`] 的纯函数版（不碰进程 env，便于确定性测试）。
///
/// `ZHUYE_HOME` 原样采用（允许相对路径，历史上即如此）；为空串时
/// 退回 `HOME`，再退回 `dirs::home_dir()`（getpwuid 兜底，覆盖
/// launchd 等不设 `HOME` 的启动场景）。
pub fn resolve_data_dir(zhuye_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    if let Some(dir) = zhuye_home.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    home.map(PathBuf::from)
        .or_else(dirs::home_dir)
        .map(|dir| dir.join(DEFAULT_CONFIG_DIR))
}

/// 用户级数据目录下的子路径，例如 `data_path(&["kms"])` → `~/.zhuye/kms`。
pub fn data_path(parts: &[&str]) -> Option<PathBuf> {
    let mut path = data_dir()?;
    for part in parts {
        path.push(part);
    }
    Some(path)
}

/// 同 [`data_path`]，但取不到 home 时回落到相对路径 `.zhuye/<parts>`。
///
/// 用于「进程仍要能启动，具体命令自己报 IO 错」的场景。
pub fn data_path_or_relative(parts: &[&str]) -> PathBuf {
    data_path(parts).unwrap_or_else(|| {
        let mut path = PathBuf::from(DEFAULT_CONFIG_DIR);
        for part in parts {
            path.push(part);
        }
        path
    })
}

/// 工作区级数据目录：`<workspace>/.zhuye`（与项目级 memory / skills 同源）。
pub fn project_data_dir(workspace: &Path) -> PathBuf {
    workspace.join(DEFAULT_CONFIG_DIR)
}

/// 工作区级数据目录下的子路径。
pub fn project_data_path(workspace: &Path, parts: &[&str]) -> PathBuf {
    let mut path = project_data_dir(workspace);
    for part in parts {
        path.push(part);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zhuye_home_points_at_the_data_dir_itself() {
        // `ZHUYE_HOME` 就是数据目录本身：其下直接放 config.toml，不再拼 `.zhuye`。
        let resolved =
            resolve_data_dir(Some("/srv/instance".into()), Some("/home/u".into())).unwrap();
        assert_eq!(resolved, PathBuf::from("/srv/instance"));
    }

    #[test]
    fn empty_zhuye_home_is_treated_as_unset() {
        let resolved = resolve_data_dir(Some("".into()), Some("/home/u".into())).unwrap();
        assert_eq!(resolved, PathBuf::from("/home/u").join(DEFAULT_CONFIG_DIR));
    }

    #[test]
    fn falls_back_to_home_dot_zhuye() {
        let resolved = resolve_data_dir(None, Some("/home/u".into())).unwrap();
        assert_eq!(resolved, PathBuf::from("/home/u/.zhuye"));
    }

    #[test]
    fn data_path_appends_subdirectories() {
        // 只验拼装规则；进程 env 不改，避免与其他并发测试相互污染。
        let mut expected = data_dir().unwrap();
        expected.push("sessions-archive");
        assert_eq!(data_path(&["sessions-archive"]), Some(expected));
    }

    #[test]
    fn relative_fallback_keeps_the_same_dir_name() {
        // 取不到 home 时的回落必须是相对 `.zhuye`;`ZHUYE_HOME` 为相对路径时
        // 原样采用(历史上即如此)。env 版 `data_path_or_relative` 的回落分支
        // 依赖 HOME 缺失,无法在测试进程内确定性构造,这里只锁纯函数口径。
        assert_eq!(
            resolve_data_dir(Some("relative/home".into()), None).unwrap(),
            PathBuf::from("relative/home")
        );
        assert_eq!(
            resolve_data_dir(None, None).map(|d| d.join("tasks")),
            dirs::home_dir().map(|h| h.join(".zhuye/tasks"))
        );
    }

    #[test]
    fn project_dir_matches_workspace_naming() {
        assert_eq!(
            project_data_path(Path::new("/ws"), &["agent-memory"]),
            PathBuf::from("/ws/.zhuye/agent-memory")
        );
    }
}
