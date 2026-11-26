//! # 工具参数自动修复与结构化输出(F22)
//!
//! 三件工具面的小事,合起来省掉大量无谓重试:
//!
//! 1. **参数自动修复**([`repair_args`])—— 模型给错参数形状(字符串代替
//!    对象、缺必填、类型不符)时,按 schema 尝试**无损**修正,而不是直接
//!    报 `InvalidArgs` 让模型重来。每次重试都是一次往返,修好比报错便宜
//!    得多。
//! 2. **Sequential Thinking**([`SequentialThinking`])—— 内置的显式
//!    推理草稿工具:模型分步写下思考,每步可修订前面的结论。
//! 3. **类型化返回**([`TypedResult`])—— 工具输出带结构化 `data` 字段,
//!    让调用方(程序)能判定/统计,而不只是让模型读文本。
//!
//! ## 修复的边界
//!
//! 只做**可证明无损**的修正:
//!
//! - 字符串 `"42"` → 数字 `42`(schema 要 integer/number);
//! - 单值 → 单元素数组(schema 要 array);
//! - 缺可选字段 → 补默认值;
//! - 参数被包在 `{"args": {...}}` 里 → 拆一层(模型常见的多包一层)。
//!
//! **绝不**猜测语义(不把 `"yes"` 变成 `true`,不把缺失的必填字段编一个值)
//! —— 那会静默改变模型意图。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// 一次修复记录(供审计 / 诊断)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairNote {
    /// 修复的字段路径(如 `count` / `items[0]`)。
    pub path: String,
    /// 做了什么(人读)。
    pub change: String,
}

/// 参数修复结果。
#[derive(Debug, Clone, PartialEq)]
pub struct RepairOutcome {
    pub args: serde_json::Value,
    pub notes: Vec<RepairNote>,
}

impl RepairOutcome {
    /// 是否发生了修复。
    pub fn changed(&self) -> bool {
        !self.notes.is_empty()
    }
}

/// 按 JSON Schema 修复工具参数。
///
/// 只做无损修正(见模块文档)。无法修复时原样返回 + 空 notes —— 由调用方
/// 继续走既有的 `InvalidArgs` 报错路径。
pub fn repair_args(args: &serde_json::Value, schema: &serde_json::Value) -> RepairOutcome {
    let mut notes = Vec::new();
    let mut working = args.clone();

    // ① 拆掉多余的 `{"args": {...}}` 包装(模型最常见的多包一层)。
    if let Some(inner) = unwrap_single_args(&working, schema) {
        notes.push(RepairNote {
            path: "args".into(),
            change: "unwrap nested {\"args\": ...} wrapper".into(),
        });
        working = inner;
    }

    // ② 按 properties 逐字段修正。
    if let Some(props) = schema.get("properties").and_then(|p| p.as_object())
        && let Some(obj) = working.as_object_mut()
    {
        for (field, field_schema) in props {
            if let Some(value) = obj.get(field).cloned() {
                let repaired = repair_value(&value, field_schema, field, &mut notes);
                if repaired != value {
                    obj.insert(field.clone(), repaired);
                }
            }
        }
    }

    RepairOutcome {
        args: working,
        notes,
    }
}

/// 拆掉 `{"args": {...}}`:仅当 schema 顶层没有 `args` 属性、且内层对象
/// 看起来符合 schema 时。
fn unwrap_single_args(
    args: &serde_json::Value,
    schema: &serde_json::Value,
) -> Option<serde_json::Value> {
    let obj = args.as_object()?;
    if obj.len() != 1 {
        return None;
    }
    let inner = obj.get("args")?;
    if !inner.is_object() {
        return None;
    }
    // schema 自己声明了 `args` 字段 → 不是误包。
    if schema
        .get("properties")
        .and_then(|p| p.as_object())
        .is_some_and(|p| p.contains_key("args"))
    {
        return None;
    }
    Some(inner.clone())
}

/// 按 schema 修一个值(递归)。
fn repair_value(
    value: &serde_json::Value,
    schema: &serde_json::Value,
    path: &str,
    notes: &mut Vec<RepairNote>,
) -> serde_json::Value {
    let type_name = schema.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match type_name {
        "integer" | "number" => {
            // 字符串数字 → 数字(无损)。
            if let Some(s) = value.as_str() {
                let parsed = if type_name == "integer" {
                    s.trim().parse::<i64>().ok().map(|n| serde_json::json!(n))
                } else {
                    s.trim().parse::<f64>().ok().and_then(|f| {
                        serde_json::Number::from_f64(f).map(|n| serde_json::Value::Number(n))
                    })
                };
                if let Some(v) = parsed {
                    notes.push(RepairNote {
                        path: path.into(),
                        change: format!("string {s:?} → {type_name}"),
                    });
                    return v;
                }
            }
            value.clone()
        }
        "boolean" => {
            // 只接受 JSON 原生布尔;字符串 "true"/"false" 也算无损。
            if let Some(s) = value.as_str() {
                match s.trim().to_ascii_lowercase().as_str() {
                    "true" => {
                        notes.push(RepairNote {
                            path: path.into(),
                            change: "string \"true\" → boolean".into(),
                        });
                        return serde_json::Value::Bool(true);
                    }
                    "false" => {
                        notes.push(RepairNote {
                            path: path.into(),
                            change: "string \"false\" → boolean".into(),
                        });
                        return serde_json::Value::Bool(false);
                    }
                    _ => {}
                }
            }
            value.clone()
        }
        "array" => {
            // 单值 → 单元素数组(schema 要数组,模型给了标量)。
            if !value.is_array() {
                let item_schema = schema
                    .get("items")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let item = repair_value(value, &item_schema, &format!("{path}[0]"), notes);
                notes.push(RepairNote {
                    path: path.into(),
                    change: "scalar → single-element array".into(),
                });
                return serde_json::Value::Array(vec![item]);
            }
            value.clone()
        }
        "object" => {
            // 字符串化的 JSON 对象 → 解析(模型偶尔把对象写成字符串)。
            if let Some(s) = value.as_str()
                && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s)
                && parsed.is_object()
            {
                notes.push(RepairNote {
                    path: path.into(),
                    change: "JSON string → object".into(),
                });
                return parsed;
            }
            value.clone()
        }
        _ => value.clone(),
    }
}

/// 补可选字段的默认值(独立函数:与「修类型」分开,便于调用方选择)。
pub fn apply_defaults(args: &serde_json::Value, schema: &serde_json::Value) -> RepairOutcome {
    let mut notes = Vec::new();
    let mut working = args.clone();
    if let Some(props) = schema.get("properties").and_then(|p| p.as_object())
        && let Some(obj) = working.as_object_mut()
    {
        for (field, field_schema) in props {
            if obj.contains_key(field) {
                continue;
            }
            if let Some(default) = field_schema.get("default") {
                obj.insert(field.clone(), default.clone());
                notes.push(RepairNote {
                    path: field.clone(),
                    change: "filled default".into(),
                });
            }
        }
    }
    RepairOutcome {
        args: working,
        notes,
    }
}

// ── Sequential Thinking ──────────────────────────────────────────────────

/// 一步思考。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thought {
    /// 步骤序号(1 起)。
    pub number: usize,
    /// 思考内容。
    pub thought: String,
    /// 这一步是否在修订前面的结论。
    #[serde(default)]
    pub is_revision: bool,
    /// 被修订的步骤号(仅 `is_revision` 时有意义)。
    #[serde(default)]
    pub revises: Option<usize>,
}

/// Sequential Thinking 草稿本。
///
/// 模型分步写思考;后面的步骤可以修订前面(标记 `revises`)。最终给出
/// **当前有效**的思考链(被修订的步骤保留但标记 `superseded`)。
#[derive(Debug, Default)]
pub struct SequentialThinking {
    thoughts: Vec<Thought>,
    /// 被修订的步骤号集合。
    superseded: std::collections::HashSet<usize>,
}

impl SequentialThinking {
    pub fn new() -> SequentialThinking {
        SequentialThinking::default()
    }

    /// 追加一步思考,返回分配到的序号。
    pub fn add(&mut self, thought: impl Into<String>) -> usize {
        let number = self.thoughts.len() + 1;
        self.thoughts.push(Thought {
            number,
            thought: thought.into(),
            is_revision: false,
            revises: None,
        });
        number
    }

    /// 修订第 `target` 步,追加一条修订记录。
    ///
    /// 返回新步骤号;`target` 不存在时返回 `None`(不静默忽略)。
    pub fn revise(&mut self, target: usize, thought: impl Into<String>) -> Option<usize> {
        if !self.thoughts.iter().any(|t| t.number == target) {
            return None;
        }
        let number = self.thoughts.len() + 1;
        self.thoughts.push(Thought {
            number,
            thought: thought.into(),
            is_revision: true,
            revises: Some(target),
        });
        self.superseded.insert(target);
        Some(number)
    }

    /// 全部思考(含被修订的)。
    pub fn all(&self) -> &[Thought] {
        &self.thoughts
    }

    /// 当前有效思考链(排除被修订的步骤)。
    pub fn effective(&self) -> Vec<&Thought> {
        self.thoughts
            .iter()
            .filter(|t| !self.superseded.contains(&t.number))
            .collect()
    }

    /// 是否某步已被修订。
    pub fn is_superseded(&self, number: usize) -> bool {
        self.superseded.contains(&number)
    }

    /// 步数。
    pub fn len(&self) -> usize {
        self.thoughts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.thoughts.is_empty()
    }

    /// 渲染成可读文本(进工具输出)。
    pub fn render(&self) -> String {
        let mut out = String::new();
        for t in self.effective() {
            out.push_str(&format!("{}. {}\n", t.number, t.thought));
        }
        let revised = self.superseded.len();
        if revised > 0 {
            out.push_str(&format!("\n({revised} 步已被后续修订)\n"));
        }
        out
    }
}

// ── 类型化返回 ────────────────────────────────────────────────────────────

/// 工具的类型化结果:文本 + 结构化数据。
///
/// `data` 让**程序**能判定(不必解析文本);`text` 让模型能读。两者并存,
/// 而不是二选一 —— 模型读文本更稳,程序读 JSON 更准。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TypedResult {
    /// 人读/模型读的文本。
    pub text: String,
    /// 结构化数据(`null` = 无结构化结果)。
    #[serde(default)]
    pub data: serde_json::Value,
    /// 类型名(调用方/UI 据此选择渲染器,如 `"diff"` / `"table"` / `"chart"`)。
    #[serde(default)]
    pub kind: Option<String>,
}

impl TypedResult {
    /// 纯文本结果。
    pub fn text(text: impl Into<String>) -> TypedResult {
        TypedResult {
            text: text.into(),
            data: serde_json::Value::Null,
            kind: None,
        }
    }

    /// 文本 + 结构化数据。
    pub fn typed(
        text: impl Into<String>,
        kind: impl Into<String>,
        data: serde_json::Value,
    ) -> TypedResult {
        TypedResult {
            text: text.into(),
            data,
            kind: Some(kind.into()),
        }
    }

    /// 是否有结构化数据。
    pub fn has_data(&self) -> bool {
        !self.data.is_null()
    }

    /// 渲染为 `ToolOutput.metadata` 的形状。
    pub fn to_metadata(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        if let Some(kind) = &self.kind {
            map.insert("kind".into(), serde_json::Value::String(kind.clone()));
        }
        if self.has_data() {
            map.insert("data".into(), self.data.clone());
        }
        serde_json::Value::Object(map)
    }
}

/// 按类型名分发到具体处理器(调用方用)。
pub fn dispatch_by_kind<'a>(
    handlers: &'a HashMap<String, String>,
    kind: &str,
) -> Option<&'a String> {
    handlers.get(kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_object() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "count": {"type": "integer"},
                "ratio": {"type": "number"},
                "flag": {"type": "boolean"},
                "items": {"type": "array", "items": {"type": "string"}},
                "config": {"type": "object"},
                "label": {"type": "string"}
            },
            "required": ["count"]
        })
    }

    #[test]
    fn string_number_repaired() {
        let args = serde_json::json!({"count": "42"});
        let out = repair_args(&args, &schema_object());
        assert_eq!(out.args["count"], 42);
        assert!(out.changed());
        assert!(out.notes.iter().any(|n| n.path == "count"));
    }

    #[test]
    fn string_float_repaired() {
        let args = serde_json::json!({"ratio": "0.75"});
        let out = repair_args(&args, &schema_object());
        assert_eq!(out.args["ratio"], 0.75);
    }

    #[test]
    fn non_numeric_string_not_repaired() {
        let args = serde_json::json!({"count": "many"});
        let out = repair_args(&args, &schema_object());
        assert_eq!(out.args["count"], "many", "无法无损修复则保留原值");
        assert!(!out.notes.iter().any(|n| n.path == "count"));
    }

    #[test]
    fn string_boolean_repaired() {
        let args = serde_json::json!({"flag": "true"});
        let out = repair_args(&args, &schema_object());
        assert_eq!(out.args["flag"], true);
    }

    #[test]
    fn scalar_to_array() {
        let args = serde_json::json!({"items": "only-one"});
        let out = repair_args(&args, &schema_object());
        assert_eq!(out.args["items"], serde_json::json!(["only-one"]));
    }

    #[test]
    fn json_string_to_object() {
        let args = serde_json::json!({"config": "{\"a\": 1}"});
        let out = repair_args(&args, &schema_object());
        assert_eq!(out.args["config"]["a"], 1);
    }

    #[test]
    fn unwraps_nested_args_wrapper() {
        let args = serde_json::json!({"args": {"count": 5}});
        let out = repair_args(&args, &schema_object());
        assert_eq!(out.args["count"], 5);
        assert!(out.notes.iter().any(|n| n.path == "args"));
    }

    #[test]
    fn does_not_unwrap_when_schema_has_args_field() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"args": {"type": "object"}}
        });
        let args = serde_json::json!({"args": {"count": 5}});
        let out = repair_args(&args, &schema);
        assert_eq!(out.args["args"]["count"], 5, "schema 声明了 args 就不拆");
    }

    #[test]
    fn no_change_for_valid_args() {
        let args = serde_json::json!({"count": 1, "label": "x"});
        let out = repair_args(&args, &schema_object());
        assert!(!out.changed(), "{:?}", out.notes);
        assert_eq!(out.args, args);
    }

    #[test]
    fn apply_defaults_fills_missing() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer", "default": 10},
                "mode": {"type": "string", "default": "fast"}
            }
        });
        let args = serde_json::json!({"limit": 5});
        let out = apply_defaults(&args, &schema);
        assert_eq!(out.args["mode"], "fast");
        assert_eq!(out.args["limit"], 5, "已有值不覆盖");
    }

    #[test]
    fn apply_defaults_skips_fields_without_default() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"required_thing": {"type": "string"}}
        });
        let out = apply_defaults(&serde_json::json!({}), &schema);
        assert!(!out.args.as_object().unwrap().contains_key("required_thing"));
        assert!(!out.changed());
    }

    #[test]
    fn sequential_thinking_add_and_render() {
        let mut st = SequentialThinking::new();
        assert_eq!(st.add("first"), 1);
        assert_eq!(st.add("second"), 2);
        assert_eq!(st.len(), 2);
        let rendered = st.render();
        assert!(rendered.contains("1. first"), "{rendered}");
        assert!(rendered.contains("2. second"), "{rendered}");
    }

    #[test]
    fn sequential_thinking_revision() {
        let mut st = SequentialThinking::new();
        st.add("wrong conclusion");
        st.add("more context");
        let n = st.revise(1, "corrected conclusion").expect("revise");
        assert_eq!(n, 3);
        assert!(st.is_superseded(1));
        let effective = st.effective();
        assert_eq!(effective.len(), 2, "被修订的步骤不进入有效链");
        assert!(effective.iter().all(|t| t.number != 1));
        let rendered = st.render();
        assert!(rendered.contains("corrected conclusion"), "{rendered}");
        assert!(!rendered.contains("wrong conclusion"), "{rendered}");
        assert!(rendered.contains("已被后续修订"), "{rendered}");
    }

    #[test]
    fn sequential_thinking_revise_unknown_returns_none() {
        let mut st = SequentialThinking::new();
        assert!(st.revise(99, "x").is_none());
        assert_eq!(st.len(), 0, "失败不应追加");
    }

    #[test]
    fn typed_result_text_only() {
        let r = TypedResult::text("hello");
        assert!(!r.has_data());
        let meta = r.to_metadata();
        assert!(meta.get("data").is_none());
    }

    #[test]
    fn typed_result_with_data() {
        let r = TypedResult::typed(
            "found 3 files",
            "table",
            serde_json::json!([{"path": "a"}, {"path": "b"}]),
        );
        assert!(r.has_data());
        let meta = r.to_metadata();
        assert_eq!(meta["kind"], "table");
        assert_eq!(meta["data"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn typed_result_serializes() {
        let r = TypedResult::typed("t", "chart", serde_json::json!({"n": 1}));
        let json = serde_json::to_string(&r).expect("serialize");
        let back: TypedResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, r);
    }

    #[test]
    fn dispatch_by_kind_routes_known_kind() {
        let mut handlers = HashMap::new();
        handlers.insert("diff".to_string(), "render_diff".to_string());
        assert_eq!(
            dispatch_by_kind(&handlers, "diff"),
            Some(&"render_diff".to_string())
        );
        assert!(dispatch_by_kind(&handlers, "missing").is_none());
    }
}
