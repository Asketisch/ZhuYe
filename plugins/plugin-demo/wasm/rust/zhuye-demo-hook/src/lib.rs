//! 社区样例:Rust 实现 `zhuye:plugin/zhuye-hook` 世界。
//!
//! 决策 JSON 协议与 shell hook 的 stdout 一致(见 wit/plugin.wit 注释)。
//! 本样例对 PostToolUse 一律 Allow,并对 bash 工具注入一条提醒,演示
//! `inject` 决策的进程内形态。

wit_bindgen::generate!({
    path: "../../../../../wit",
    world: "zhuye:plugin/zhuye-hook@0.1.0",
});

struct DemoHook;

impl Guest for DemoHook {
    fn handle(event: String) -> Result<String, String> {
        // 事件 JSON(kind 字段标识事件类型)。样例策略:
        // - PostToolUse 且工具为 bash → allow + inject 提醒;
        // - 其余一律 allow。
        let is_bash_post = event.contains("\"kind\":\"post_tool_use\"") && event.contains("bash");
        if is_bash_post {
            return Ok(
                "{\"action\":\"allow\",\"reason\":\"\"}".to_string().replace(
                    "{\"action\":\"allow\",\"reason\":\"\"}",
                    "{\"action\":\"inject\",\"content\":\"[demo-wasm] bash 输出已由进程内 hook 审阅\"}",
                ),
            );
        }
        Ok("{\"action\":\"allow\"}".to_string())
    }
}

export!(DemoHook);
