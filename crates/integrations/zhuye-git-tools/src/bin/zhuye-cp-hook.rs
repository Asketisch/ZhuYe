//! `zhuye-cp-hook` — Stop hook 用自动 checkpoint(WP17)。
//!
//! 读 stdin 的 hook 事件 JSON(见 `zhuye_hooks::shell` 的派发协议),在
//! 事件里的 workspace(缺省取进程 cwd)打 git checkpoint。
//!
//! ## 为什么必须永远退出 0
//!
//! 插件 / 外部 hook 是 **fail-closed**(spawn 失败 / 超时 / 非零退出 →
//! `HookDecision::Deny`),而 `Stop` hook 的 Deny 语义是**否决完成、强制
//! 续跑** —— checkpoint 失败若返回非零,agent 会被无限续跑直到 turn 上限。
//! 本 hook 是纯信息性的:git 不可用 / 非 repo / 无改动一律退出 0(失败
//! 详情写 stderr,进 hook 日志)。

use std::io::{self, Read};
use std::path::PathBuf;

fn main() {
    // 永远退出 0(理由见模块 doc)。
    let mut input = String::new();
    let _ = io::stdin().read_to_string(&mut input);

    let workspace = serde_json::from_str::<serde_json::Value>(&input)
        .ok()
        .and_then(|v| {
            v.get("workspace")
                .and_then(|w| w.as_str())
                .map(PathBuf::from)
        })
        .or_else(|| std::env::current_dir().ok());

    let Some(cwd) = workspace else {
        eprintln!("[zhuye-cp-hook] no workspace in event and no cwd; skip");
        return;
    };
    match zhuye_git::checkpoint::create(&cwd, "auto (stop hook)") {
        Ok(Some(cp)) => eprintln!(
            "[zhuye-cp-hook] checkpoint created @ {} ({})",
            cp.sha, cp.subject
        ),
        Ok(None) => eprintln!("[zhuye-cp-hook] no changes; no snapshot"),
        Err(e) => eprintln!("[zhuye-cp-hook] checkpoint failed: {e}"),
    }
    // 显式 exit 0:即便上面任何路径意外出错,也不能触发 Stop 的 fail-closed Deny。
    std::process::exit(0);
}
