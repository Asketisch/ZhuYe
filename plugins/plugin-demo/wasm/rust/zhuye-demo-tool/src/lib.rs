//! 社区样例:Rust 实现 `zhuye:plugin/zhuye-tool` 世界(echo 工具)。

wit_bindgen::generate!({
    path: "../../../../../wit",
    world: "zhuye:plugin/zhuye-tool@0.1.0",
});

struct DemoEcho;

impl Guest for DemoEcho {
    fn describe() -> String {
        r#"{
  "name": "demo_echo",
  "description": "WASM 进程内 echo 工具样例(plugin-demo)",
  "parameters": {
    "type": "object",
    "properties": {
      "text": { "type": "string", "description": "回显文本" }
    },
    "required": ["text"]
  }
}"#
        .to_string()
    }

    fn invoke(args: String) -> Result<String, String> {
        // 输出 JSON = 协议层 ToolOutput 的 wire 形态(字段必须齐全:
        // content / is_error / metadata / elapsed_ms)。
        let text = serde_json_pathless(&args);
        Ok(format!(
            r#"{{"content":[{{"type":"text","text":"echo(wasm): {text}"}}],"is_error":false,"metadata":{{}},"elapsed_ms":0}}"#
        ))
    }
}

/// 极简 args["text"] 提取(样例不引 serde_json,免依赖展示最小形态)。
fn serde_json_pathless(args: &str) -> String {
    // 找 "text":"..." 的值;找不到原样截断回显。
    let marker = "\"text\":\"";
    match args.find(marker) {
        Some(i) => {
            let rest = &args[i + marker.len()..];
            rest.split('"').next().unwrap_or(rest).to_string()
        }
        None => args.chars().take(120).collect(),
    }
}

export!(DemoEcho);
