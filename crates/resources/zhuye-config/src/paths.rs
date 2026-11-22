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

/// 进程 env(`ZHUYE_HOME` / `HOME`)的测试互斥锁。
///
/// `data_dir` / `data_path` 在一次调用里要读两个变量,读与读之间若另一
/// 测试并发改写 env(load.rs 的 `zhuye_home_redirects_config_load`),
/// 断言两边就会各拿到一个值 —— CI macos 实证过这个 flake。读 env 的
/// 测试与改 env 的测试都必须持同一把锁串行化。仅测试消费,运行时无读者。
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 用户级数据目录：`$ZHUYE_HOME` > `$HOME/.zhuye`。
///
/// `ZHUYE_HOME` 指向的**就是**数据目录本身（其下直接放 `config.toml`），
/// 与 `default_config_path` 同语义；空值按未设置处理。拿不到任何 home
/// 时返回 `None`，调用方按各自的降级策略处理（不要 panic）。
pub fn data_dir() -> Option<PathBuf> {
    zhuye_fs::data_dir()
}

/// [`data_dir`] 的纯函数版（不碰进程 env，便于确定性测试）。
///
/// `ZHUYE_HOME` 原样采用（允许相对路径，历史上即如此）；为空串时
/// 退回 `HOME`，再退回 `dirs::home_dir()`（getpwuid 兜底，覆盖
/// launchd 等不设 `HOME` 的启动场景）。
pub fn resolve_data_dir(zhuye_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    zhuye_fs::resolve_data_dir(zhuye_home, home)
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

/// 用户级通用 agent skill 目录：`~/.agents/skills`。
///
/// 业界通用约定（skills CLI 的 "universal" 目标等十余个 agent 共用），
/// **非烛夜私有数据目录**，故不走 `ZHUYE_HOME` 重定向、直接取 `$HOME`。
/// 取不到 home 时返回 `None`，调用方跳过该目录即可（不要 panic）。
pub fn user_agents_skills_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".agents").join("skills"))
}

/// 工作区级通用 agent skill 目录：`<workspace>/.agents/skills`。
///
/// 与 [`user_agents_skills_dir`] 同约定，作用域为当前工作区；随仓库
/// 分发（可入 VCS），供烛夜与其他 agent 共享技能。
pub fn project_agents_skills_dir(workspace: &Path) -> PathBuf {
    workspace.join(".agents").join("skills")
}

/// 默认 skill 扫描目录清单（优先级从高到低，同名 skill 先者胜）：
///
/// 1. `<workspace>/.zhuye/skills` —— 烛夜工作区级；
/// 2. `<workspace>/.agents/skills` —— 业界通用 agent 目录（跨 agent 互操作）；
/// 3. `~/.zhuye/skills` —— 烛夜用户级（`ZHUYE_HOME` 可重定向）；
/// 4. `~/.agents/skills` —— 业界通用 agent 目录（用户级）。
///
/// bootstrap（exec / m4 两处）与 GUI 列表命令都必须以此清单为唯一
/// 来源，否则运行时 catalog 与 GUI 各看一套目录。
pub fn default_skill_dirs(workspace: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![
        project_data_path(workspace, &["skills"]),
        project_agents_skills_dir(workspace),
        // 取不到 home 时回落相对 `.zhuye/skills`，与 data_path_or_relative 口径一致。
        data_path_or_relative(&["skills"]),
    ];
    if let Some(d) = user_agents_skills_dir() {
        dirs.push(d);
    }
    dirs
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
        // 只验拼装规则;进程 env 不改,避免与其他并发测试相互污染。
        // 但本测试要**读**两次 env(`data_dir` + `data_path` 内部各一次),
        // 必须与改 env 的测试(load.rs)持同一把锁串行,否则两次读取可能
        // 落在并发改写的前后两侧 —— CI macos 曾因此间歇失败。
        let _guard = ENV_LOCK.lock().unwrap();
        let mut expected = data_dir().unwrap();
        expected.push("sessions-archive");
        assert_eq!(data_path(&["sessions-archive"]), Some(expected));
    }

    #[test]
    fn relative_fallback_keeps_the_same_dir_name() {
        // 取不到 home 时的回落必须是相对 `.zhuye`;`ZHUYE_HOME` 为相对路径时
        // 原样采用(历史上即如此)。env 版 `data_path_or_relative` 的回落分支
        // 依赖 HOME 缺失,无法在测试进程内确定性构造,这里只锁纯函数口径。
        // 最后一个断言两侧都依赖 `$HOME`(`dirs::home_dir` 首选读它),
        // 同样要持 ENV_LOCK 与改 env 的测试串行。
        let _guard = ENV_LOCK.lock().unwrap();
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

    /// skill 目录清单:四目录固定优先级,`.zhuye` 在前、`.agents` 在后。
    /// `default_skill_dirs` 读进程 env(data_path_or_relative 内部),
    /// 必须持 ENV_LOCK 与改 env 的测试串行。
    #[test]
    fn default_skill_dirs_priority_order() {
        let _guard = ENV_LOCK.lock().unwrap();
        let ws = Path::new("/ws");
        let dirs = default_skill_dirs(ws);
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/ws/.zhuye/skills"),
                PathBuf::from("/ws/.agents/skills"),
                data_path_or_relative(&["skills"]),
                user_agents_skills_dir().unwrap(),
            ],
            "顺序即同名优先级:工作区 .zhuye > 工作区 .agents > 用户 .zhuye > 用户 .agents"
        );
        // 通用目录与烛夜目录同名并存的场景由调用方(扫描链)按序去重,
        // 这里只锁清单本身。
        assert_eq!(
            project_agents_skills_dir(ws),
            PathBuf::from("/ws/.agents/skills")
        );
    }

    /// 锁纪律的**结构门禁**(确定性,不靠调度采样)。
    ///
    /// 竞态回归无法用单条行为用例锁定(依赖线程调度,压测只能概率放大),
    /// 这里机械校验锁纪律本身:改 env 的测试(load.rs)与读 env 的测试
    /// (paths.rs 两条)必须都引用 crate 级 `ENV_LOCK`。谁把锁改回函数
    /// 局部、或新增/删掉持锁点,本测试立即失败。
    #[test]
    fn env_lock_discipline_is_structurally_enforced() {
        let load_src = include_str!("load/tests.rs");
        assert!(
            load_src.contains("crate::paths::ENV_LOCK.lock()"),
            "load/tests.rs 的 env 改写测试必须持 crate 级 paths::ENV_LOCK \
             (函数局部锁锁不住其他模块的 env 读者,见 ENV_LOCK 文档)"
        );
        let paths_src = include_str!("paths.rs");
        // 搜索词必须用 concat! 拼接:门禁自身就在 paths.rs 里,直接写字面量
        // 会被 include_str 读进来形成自匹配(把计数撑爆,第一次落地即翻车)。
        let guard = concat!("let _guard = ", "ENV_LOCK.lock()");
        assert_eq!(
            paths_src.matches(guard).count(),
            3,
            "paths.rs 恰好三个测试读进程 env(data_path_appends_subdirectories \
             与 relative_fallback_keeps_the_same_dir_name 与 \
             default_skill_dirs_priority_order),都必须持 ENV_LOCK;\
             新增读 env 测试时同步补锁并更新此断言"
        );
    }
}
