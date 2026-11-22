//! Strict schema 折叠 —— 把任意 JSON Schema 折进 provider 的 strict 子集。
//!
//! 修复背景(借鉴同类 agent 的 strict-tool-schema 折衷哲学):strict /
//! 结构化输出只支持 JSON Schema 的一个子集;此前不支持的关键字(pattern、
//! minimum、format……)只有两条路 —— 要么原样发送(provider 拒绝或静默
//! 忽略,约束丢失),要么整个放弃 strict。折衷:**支持的关键字保留在
//! schema 里;不支持的关键字从 schema 中移除,但把约束以自然语言写进
//! 补充说明** —— "模型仍读得到,引擎侧校验器仍强制"。
//!
//! 纯函数,零 I/O。折不掉的形态(根节点非 object、任意位置的
//! anyOf/oneOf/not)整体返回 `None`,调用方回退为原样发送(诚实降级,
//! 不编造)。

use serde_json::{Map, Value};

/// 折叠产出:safe schema + 需要以自然语言补充给模型的约束说明。
#[derive(Debug, Clone, PartialEq)]
pub struct FoldedSchema {
    /// 折叠后的 schema(只含 strict 子集关键字;`additionalProperties`
    /// 已按 strict 语义收紧)。
    pub schema: Value,
    /// 被移除约束的自然语言说明(逐条;调用方拼进工具 description /
    /// system 补充段)。
    pub folded_notes: Vec<String>,
}

/// 折叠为 strict 子集。根节点必须是 object 且全程不含 anyOf/oneOf/not,
/// 否则 `None`(调用方回退原样发送)。
pub fn fold_to_strict(schema: &Value) -> Option<FoldedSchema> {
    // 结构化输出的 strict 形态要求根节点是 object(非 object 根折叠无
    // 意义 —— 直接诚实降级)。
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return None;
    }
    let mut notes = Vec::new();
    let folded = fold_node("", schema, &mut notes, 0)?;
    Some(FoldedSchema {
        schema: folded,
        folded_notes: notes,
    })
}

/// 递归折叠单个节点。`path` 为 JSON Pointer 风格字段路径(_notes 用)。
fn fold_node(path: &str, node: &Value, notes: &mut Vec<String>, depth: usize) -> Option<Value> {
    if depth > 16 {
        // 防御性深度上限(对抗构造性深嵌套;超出视为折不掉)。
        return None;
    }
    let obj = node.as_object()?;
    // 组合子与否定:strict 子集不支持,整体放弃(诚实降级)。
    if obj.contains_key("anyOf") || obj.contains_key("oneOf") || obj.contains_key("not") {
        return None;
    }
    let schema_type = obj.get("type").and_then(Value::as_str)?;
    let mut out = Map::new();
    out.insert("type".into(), Value::String(schema_type.to_string()));
    if let Some(desc) = obj.get("description").and_then(Value::as_str) {
        out.insert("description".into(), Value::String(desc.to_string()));
    }
    match schema_type {
        "object" => {
            let props = obj.get("properties").and_then(Value::as_object)?;
            let mut new_props = Map::new();
            for (key, sub) in props {
                let sub_path = format!("{path}/{key}");
                new_props.insert(key.clone(), fold_node(&sub_path, sub, notes, depth + 1)?);
            }
            // strict 语义:additionalProperties=false + required 覆盖全部
            // 属性(strict 模式下部分 required 不被接受)。
            out.insert("properties".into(), Value::Object(new_props));
            out.insert("additionalProperties".into(), Value::Bool(false));
            let required: Vec<Value> = props.keys().map(|k| Value::String(k.clone())).collect();
            out.insert("required".into(), Value::Array(required));
        }
        "array" => {
            let items = obj.get("items")?;
            out.insert("items".into(), fold_node(path, items, notes, depth + 1)?);
        }
        "string" | "number" | "integer" | "boolean" | "null" => {}
        _ => return None,
    }
    // 枚举受支持:直接保留。
    if let Some(en) = obj.get("enum") {
        out.insert("enum".into(), en.clone());
    }
    if let Some(c) = obj.get("const") {
        out.insert("const".into(), c.clone());
    }
    // 其余关键字:strict 不支持 → 记入说明并移除。白名单之外的都在此列
    // (pattern / format / minimum / maximum / minLength / maxLength /
    // minItems / maxItems / uniqueItems / default / examples ……)。
    const SUPPORTED: &[&str] = &[
        "type",
        "description",
        "properties",
        "additionalProperties",
        "required",
        "items",
        "enum",
        "const",
    ];
    for (key, v) in obj {
        if SUPPORTED.contains(&key.as_str()) {
            continue;
        }
        notes.push(format!(
            "`{}`(路径 `{}`):{} = {}",
            pretty_keyword(key),
            if path.is_empty() { "(root)" } else { path },
            keyword_constraint_text(key),
            compact_value(v)
        ));
    }
    Some(Value::Object(out))
}

/// 关键字的人类可读约束语义(拼说明用)。
fn keyword_constraint_text(key: &str) -> &'static str {
    match key {
        "pattern" => "必须匹配正则",
        "format" => "必须是格式",
        "minimum" | "exclusiveMinimum" => "最小值",
        "maximum" | "exclusiveMaximum" => "最大值",
        "minLength" => "最小长度",
        "maxLength" => "最大长度",
        "minItems" => "最少元素数",
        "maxItems" => "最多元素数",
        "uniqueItems" => "元素必须唯一",
        "multipleOf" => "必须是倍数",
        _ => "约束",
    }
}

/// 关键字的展示名(驼峰转下划线式语义词,便于中文说明混排)。
fn pretty_keyword(key: &str) -> &str {
    key
}

/// 值的紧凑序列化(说明用;超 80 字符截断)。
fn compact_value(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 80 {
        let head: String = s.chars().take(80).collect();
        format!("{head}…")
    } else {
        s
    }
}

/// 把折叠说明拼成一段可附在工具 description 末尾的文本。
pub fn notes_to_description(notes: &[String]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n\n补充约束(strict 折叠自 JSON Schema,必须遵守):\n");
    for n in notes {
        out.push_str(&format!("- {n}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 保留支持的关键字_移除并记录约束() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "minLength": 3},
                "port": {"type": "integer", "minimum": 1, "maximum": 65535},
                "role": {"type": "string", "enum": ["a", "b"]}
            },
            "required": ["name"]
        });
        let folded = fold_to_strict(&schema).expect("应可折叠");
        // additionalProperties 收紧 + required 覆盖全部属性(strict 语义)。
        assert_eq!(folded.schema["additionalProperties"], json!(false));
        assert_eq!(folded.schema["required"], json!(["name", "port", "role"]));
        // 不支持的关键字已移除。
        assert!(
            folded.schema["properties"]["name"]
                .get("minLength")
                .is_none()
        );
        assert!(folded.schema["properties"]["port"].get("minimum").is_none());
        // 枚举保留。
        assert_eq!(
            folded.schema["properties"]["role"]["enum"],
            json!(["a", "b"])
        );
        // 说明逐条覆盖被移除的约束。
        let joined = folded.folded_notes.join("\n");
        assert!(joined.contains("minLength"), "{joined}");
        assert!(joined.contains("minimum"), "{joined}");
        assert!(joined.contains("maximum"), "{joined}");
        // 说明可拼成 description 附加段。
        let desc = notes_to_description(&folded.folded_notes);
        assert!(desc.contains("补充约束"));
    }

    #[test]
    fn 根节点非object返回none() {
        assert!(fold_to_strict(&json!({"type": "string"})).is_none());
        assert!(fold_to_strict(&json!({"type": "array", "items": {"type": "string"}})).is_none());
    }

    #[test]
    fn 组合子整体放弃() {
        let schema = json!({
            "type": "object",
            "properties": {"x": {"anyOf": [{"type": "string"}, {"type": "number"}]}}
        });
        assert!(fold_to_strict(&schema).is_none());
        let not_schema = json!({"type": "object", "not": {"type": "string"}});
        assert!(fold_to_strict(&not_schema).is_none());
    }

    #[test]
    fn 嵌套object与array递归折叠() {
        let schema = json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {"type": "string", "pattern": "^a"},
                    "maxItems": 10
                }
            }
        });
        let folded = fold_to_strict(&schema).expect("应可折叠");
        let item_schema = &folded.schema["properties"]["items"]["items"];
        assert!(item_schema.get("pattern").is_none(), "嵌套层约束同样移除");
        assert!(
            folded.schema["properties"]["items"]
                .get("maxItems")
                .is_none()
        );
        assert!(folded.folded_notes.len() >= 2);
    }
}
