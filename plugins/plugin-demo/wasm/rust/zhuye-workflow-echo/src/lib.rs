//! 工作流 WASM 任务节点示例。

wit_bindgen::generate!({
    path: "../../../../../wit",
    world: "zhuye:plugin/zhuye-workflow-node@0.1.0",
});

struct Echo;
impl Guest for Echo {
    fn describe() -> String {
        r#"{"name":"workflow_echo"}"#.into()
    }

    fn invoke(input: String) -> Result<String, String> {
        let input: serde_json::Value = serde_json::from_str(&input).map_err(|e| e.to_string())?;
        match input.get("config").and_then(|v| v.get("mode")).and_then(|v| v.as_str()) {
            Some("error") => return Err("示例组件拒绝请求".into()),
            Some("invalid") => return Ok("不是 JSON".into()),
            Some("loop") => loop { std::hint::spin_loop(); },
            _ => {}
        }
        let value = input.get("variables").and_then(|v| v.get("message")).cloned().unwrap_or_default();
        Ok(serde_json::json!({
            "status": "success",
            "output": {"echo": value},
            "updates": {"echoed": value},
            "action": {"type": "continue"}
        }).to_string())
    }
}
export!(Echo);
