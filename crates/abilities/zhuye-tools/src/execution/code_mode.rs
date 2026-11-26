//! # Code Mode:批量工具调用执行器(F21)
//!
//! 模型不必「一次喊一个工具」,可以提交一段**工具调用计划**,由引擎批量
//! 执行。省 token(一次往返代替 N 次)、省上下文(结果可聚合)。
//!
//! ## 设计取舍:为什么是「计划」而不是「任意代码」
//!
//! 真 Code Mode 让模型写 Python/JS 直接调工具。代价是**沙箱与权限面**
//! 急剧放大:任意代码意味着任意文件读写、任意网络、任意子进程 —— 每个
//! 都要重新证明权限继承正确。
//!
//! 本实现走 **受限计划**([`CallPlan`]):一串 `(tool, args)`,按顺序执行,
//! 支持 `on_error` 策略与结果引用(`$0.result`)。它拿到 Code Mode 的
//! 主要收益(批量 + 结果串联),而**每一步仍走既有权限/审批链路** ——
//! 不需要为"代码执行"重新论证安全边界。
//!
//! 需要真解释器时,`python_repl` / `node_repl` 作为**独立工具**接入
//! (各自有自己的沙箱判定),不混进本执行器。
//!
//! ## 权限
//!
//! 计划本身不执行任何工具 —— 它只是"把 N 次调用打包提交"。真正的
//! 审批发生在每个工具调用时(由 `ToolExecutionQueue` 走正常链路)。
//! 因此本模块**不引入新的权限绕过面**。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// 单条计划步骤。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    /// 工具名。
    pub tool: String,
    /// 参数。字符串值支持引用前序结果:`"$0.result"` / `"$1.output.text"`。
    #[serde(default)]
    pub args: serde_json::Value,
    /// 该步骤的别名,供后续 `$name.field` 引用。
    #[serde(default)]
    pub bind: Option<String>,
}

/// 出错时的策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnError {
    /// 中止后续步骤(默认;保守)。
    #[default]
    Abort,
    /// 跳过失败步骤,继续执行后续(失败结果仍记录)。
    Continue,
}

/// 批量调用计划。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallPlan {
    #[serde(default)]
    pub steps: Vec<PlanStep>,
    #[serde(default)]
    pub on_error: OnError,
    /// 最多执行多少步(防模型生成超长计划打爆预算)。
    #[serde(default = "default_max_steps")]
    pub max_steps: usize,
}

fn default_max_steps() -> usize {
    32
}

impl CallPlan {
    pub fn new(steps: Vec<PlanStep>) -> CallPlan {
        CallPlan {
            steps,
            on_error: OnError::Abort,
            max_steps: default_max_steps(),
        }
    }

    /// 构造期校验。
    pub fn validate(&self) -> Result<(), PlanError> {
        if self.steps.is_empty() {
            return Err(PlanError::Empty);
        }
        if self.steps.len() > self.max_steps {
            return Err(PlanError::TooManySteps {
                got: self.steps.len(),
                max: self.max_steps,
            });
        }
        for (i, step) in self.steps.iter().enumerate() {
            if step.tool.trim().is_empty() {
                return Err(PlanError::EmptyToolName { index: i });
            }
            // 引用必须指向**已出现**的步骤(防前向引用造成执行顺序歧义)。
            for r in collect_refs(&step.args) {
                // 仅**实际存在且指向未来**的步骤算前向引用;越界索引
                // (如 $9)由 resolve 阶段报 UnresolvedReference —— 这样
                // 错误信息更准确。
                if let Some(target) = r.step_index()
                    && target >= i
                    && target < self.steps.len()
                {
                    return Err(PlanError::ForwardReference { index: i, target });
                }
            }
        }
        Ok(())
    }
}

/// 计划错误。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("plan has no steps")]
    Empty,
    #[error("plan has {got} steps, exceeding max {max}")]
    TooManySteps { got: usize, max: usize },
    #[error("step {index}: tool name is empty")]
    EmptyToolName { index: usize },
    #[error("step {index} references future step {target}")]
    ForwardReference { index: usize, target: usize },
    #[error("unresolved reference in step {index}: {reference}")]
    UnresolvedReference { index: usize, reference: String },
}

/// 一次步骤执行的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepResult {
    pub index: usize,
    pub tool: String,
    /// 文本输出(聚合后的 content text)。
    pub text: String,
    pub is_error: bool,
    /// 是否因前序失败被跳过。
    pub skipped: bool,
}

/// 批量执行回执。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanReceipt {
    pub results: Vec<StepResult>,
    /// 是否有任一步失败。
    pub any_error: bool,
    /// 实际执行(非跳过)的步数。
    pub executed: usize,
    /// 因中止/跳过而未执行的步数。
    pub skipped: usize,
}

impl PlanReceipt {
    /// 人读摘要(进工具输出)。
    pub fn summary(&self) -> String {
        let ok = self
            .results
            .iter()
            .filter(|r| !r.is_error && !r.skipped)
            .count();
        let failed = self.results.iter().filter(|r| r.is_error).count();
        let skipped = self.skipped;
        format!(
            "code_mode: {ok} ok · {failed} failed · {skipped} skipped ({} steps)",
            self.results.len()
        )
    }
}

/// 一条引用(`$0.result` / `$alias.field`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    /// 步骤序号或别名。
    pub target: RefTarget,
    /// 字段路径(`result` / `output.text` …)。
    pub field: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefTarget {
    Index(usize),
    Alias(String),
}

impl Ref {
    fn step_index(&self) -> Option<usize> {
        match &self.target {
            RefTarget::Index(i) => Some(*i),
            RefTarget::Alias(_) => None,
        }
    }
}

/// 从 args 里收集所有 `$...` 引用。
fn collect_refs(value: &serde_json::Value) -> Vec<Ref> {
    let mut out = Vec::new();
    walk_refs(value, &mut out);
    out
}

fn walk_refs(value: &serde_json::Value, out: &mut Vec<Ref>) {
    match value {
        serde_json::Value::String(s) => {
            if let Some(r) = parse_ref(s) {
                out.push(r);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                walk_refs(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values() {
                walk_refs(v, out);
            }
        }
        _ => {}
    }
}

/// 解析 `$0.result` / `$name.output.text` 形态的引用。
fn parse_ref(s: &str) -> Option<Ref> {
    let body = s.strip_prefix('$')?;
    let (target_part, field) = body.split_once('.')?;
    if field.is_empty() {
        return None;
    }
    let target = if let Ok(idx) = target_part.parse::<usize>() {
        RefTarget::Index(idx)
    } else if target_part.is_empty() {
        return None;
    } else {
        RefTarget::Alias(target_part.to_string())
    };
    Some(Ref {
        target,
        field: field.to_string(),
    })
}

/// 解析 args 中的引用为实际值。
///
/// `results` 是已执行步骤的结果(`index -> StepResult`),`aliases` 是
/// `bind` 名到索引的映射。解析失败返回错误(不静默替换成空串)。
pub fn resolve_args(
    args: &serde_json::Value,
    step_index: usize,
    results: &HashMap<usize, StepResult>,
    aliases: &HashMap<String, usize>,
) -> Result<serde_json::Value, PlanError> {
    resolve_value(args, step_index, results, aliases)
}

fn resolve_value(
    value: &serde_json::Value,
    step_index: usize,
    results: &HashMap<usize, StepResult>,
    aliases: &HashMap<String, usize>,
) -> Result<serde_json::Value, PlanError> {
    use serde_json::Value;
    Ok(match value {
        Value::String(s) => {
            let Some(r) = parse_ref(s) else {
                return Ok(Value::String(s.clone()));
            };
            let target_idx = match &r.target {
                RefTarget::Index(i) => *i,
                RefTarget::Alias(name) => {
                    *aliases
                        .get(name)
                        .ok_or_else(|| PlanError::UnresolvedReference {
                            index: step_index,
                            reference: s.clone(),
                        })?
                }
            };
            let result =
                results
                    .get(&target_idx)
                    .ok_or_else(|| PlanError::UnresolvedReference {
                        index: step_index,
                        reference: s.clone(),
                    })?;
            // 目前支持两种字段:`result` / `text`(整段文本)与 `is_error`。
            match r.field.as_str() {
                "result" | "text" | "output.text" => Value::String(result.text.clone()),
                "is_error" | "output.is_error" => Value::Bool(result.is_error),
                "tool" | "output.tool" => Value::String(result.tool.clone()),
                _ => {
                    return Err(PlanError::UnresolvedReference {
                        index: step_index,
                        reference: s.clone(),
                    });
                }
            }
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| resolve_value(v, step_index, results, aliases))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                out.insert(k.clone(), resolve_value(v, step_index, results, aliases)?);
            }
            Value::Object(out)
        }
        other => other.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(tool: &str, args: serde_json::Value) -> PlanStep {
        PlanStep {
            tool: tool.to_string(),
            args,
            bind: None,
        }
    }

    fn ok_result(index: usize, tool: &str, text: &str) -> StepResult {
        StepResult {
            index,
            tool: tool.to_string(),
            text: text.to_string(),
            is_error: false,
            skipped: false,
        }
    }

    #[test]
    fn validate_rejects_empty_plan() {
        let plan = CallPlan::new(vec![]);
        assert_eq!(plan.validate(), Err(PlanError::Empty));
    }

    #[test]
    fn validate_rejects_too_many_steps() {
        let steps: Vec<PlanStep> = (0..40)
            .map(|i| step("echo", serde_json::json!({"text": i.to_string()})))
            .collect();
        let mut plan = CallPlan::new(steps);
        plan.max_steps = 32;
        assert!(matches!(
            plan.validate(),
            Err(PlanError::TooManySteps { .. })
        ));
    }

    #[test]
    fn validate_rejects_empty_tool_name() {
        let plan = CallPlan::new(vec![step("", serde_json::json!({}))]);
        assert_eq!(plan.validate(), Err(PlanError::EmptyToolName { index: 0 }));
    }

    #[test]
    fn validate_rejects_forward_reference() {
        let plan = CallPlan::new(vec![
            step("echo", serde_json::json!({"text": "$1.result"})),
            step("echo", serde_json::json!({"text": "x"})),
        ]);
        assert_eq!(
            plan.validate(),
            Err(PlanError::ForwardReference {
                index: 0,
                target: 1
            })
        );
    }

    #[test]
    fn validate_accepts_backward_reference() {
        let plan = CallPlan::new(vec![
            step("echo", serde_json::json!({"text": "hello"})),
            step("echo", serde_json::json!({"text": "$0.result"})),
        ]);
        assert!(plan.validate().is_ok());
    }

    #[test]
    fn parse_ref_index_and_alias() {
        let r = parse_ref("$0.result").expect("parse");
        assert_eq!(r.target, RefTarget::Index(0));
        assert_eq!(r.field, "result");

        let r = parse_ref("$first.output.text").expect("parse");
        assert_eq!(r.target, RefTarget::Alias("first".into()));
        assert_eq!(r.field, "output.text");

        assert!(parse_ref("not-a-ref").is_none());
        assert!(parse_ref("$0").is_none(), "缺字段不算引用");
        assert!(parse_ref("$.result").is_none(), "空目标不算引用");
    }

    #[test]
    fn resolve_args_substitutes_indexed_result() {
        let mut results = HashMap::new();
        results.insert(0, ok_result(0, "echo", "hello world"));
        let aliases = HashMap::new();
        let args = serde_json::json!({"text": "$0.result"});
        let resolved = resolve_args(&args, 1, &results, &aliases).expect("resolve");
        assert_eq!(resolved["text"], "hello world");
    }

    #[test]
    fn resolve_args_substitutes_alias() {
        let mut results = HashMap::new();
        results.insert(0, ok_result(0, "echo", "aliased"));
        let mut aliases = HashMap::new();
        aliases.insert("first".to_string(), 0usize);
        let args = serde_json::json!({"text": "$first.text"});
        let resolved = resolve_args(&args, 1, &results, &aliases).expect("resolve");
        assert_eq!(resolved["text"], "aliased");
    }

    #[test]
    fn resolve_args_nested_objects_and_arrays() {
        let mut results = HashMap::new();
        results.insert(0, ok_result(0, "echo", "deep"));
        let args = serde_json::json!({
            "list": ["$0.result", "literal"],
            "nested": {"inner": "$0.text"}
        });
        let resolved = resolve_args(&args, 1, &results, &HashMap::new()).expect("resolve");
        assert_eq!(resolved["list"][0], "deep");
        assert_eq!(resolved["list"][1], "literal");
        assert_eq!(resolved["nested"]["inner"], "deep");
    }

    #[test]
    fn resolve_args_unresolved_errors() {
        let args = serde_json::json!({"text": "$5.result"});
        let err = resolve_args(&args, 1, &HashMap::new(), &HashMap::new());
        assert!(matches!(err, Err(PlanError::UnresolvedReference { .. })));
    }

    #[test]
    fn resolve_args_unknown_alias_errors() {
        let args = serde_json::json!({"text": "$ghost.result"});
        let err = resolve_args(&args, 1, &HashMap::new(), &HashMap::new());
        assert!(matches!(err, Err(PlanError::UnresolvedReference { .. })));
    }

    #[test]
    fn resolve_args_unknown_field_errors() {
        let mut results = HashMap::new();
        results.insert(0, ok_result(0, "echo", "x"));
        let args = serde_json::json!({"text": "$0.nonsense"});
        assert!(resolve_args(&args, 1, &results, &HashMap::new()).is_err());
    }

    #[test]
    fn resolve_args_leaves_plain_strings_untouched() {
        let args = serde_json::json!({"text": "no refs here", "n": 42});
        let resolved = resolve_args(&args, 0, &HashMap::new(), &HashMap::new()).expect("resolve");
        assert_eq!(resolved["text"], "no refs here");
        assert_eq!(resolved["n"], 42);
    }

    #[test]
    fn receipt_summary_counts() {
        let receipt = PlanReceipt {
            results: vec![
                ok_result(0, "echo", "a"),
                StepResult {
                    index: 1,
                    tool: "echo".into(),
                    text: "boom".into(),
                    is_error: true,
                    skipped: false,
                },
                StepResult {
                    index: 2,
                    tool: "echo".into(),
                    text: String::new(),
                    is_error: false,
                    skipped: true,
                },
            ],
            any_error: true,
            executed: 2,
            skipped: 1,
        };
        let s = receipt.summary();
        assert!(s.contains("1 ok"), "{s}");
        assert!(s.contains("1 failed"), "{s}");
        assert!(s.contains("1 skipped"), "{s}");
    }

    #[test]
    fn on_error_defaults_to_abort() {
        let plan = CallPlan::new(vec![step("echo", serde_json::json!({}))]);
        assert_eq!(plan.on_error, OnError::Abort);
    }

    #[test]
    fn plan_serializes() {
        let plan = CallPlan::new(vec![step("echo", serde_json::json!({"text": "x"}))]);
        let json = serde_json::to_string(&plan).expect("serialize");
        let back: CallPlan = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, plan);
    }
}

// ── 执行器 ────────────────────────────────────────────────────────────────

/// 执行一个计划。
///
/// `execute` 是「执行单个工具调用」的回调 —— 由调用方(通常是
/// `ToolExecutionQueue`)提供,从而**每一步都走既有权限/审批链路**,
/// 本执行器不持有 registry、也不绕过任何门禁。
///
/// 返回回执;执行器自身不 panic(工具错误记录在 `StepResult.is_error`)。
pub async fn execute_plan<F, Fut>(plan: &CallPlan, execute: F) -> Result<PlanReceipt, PlanError>
where
    F: Fn(String, serde_json::Value) -> Fut,
    Fut: std::future::Future<Output = (String, bool)>,
{
    plan.validate()?;
    let mut results: HashMap<usize, StepResult> = HashMap::new();
    let mut aliases: HashMap<String, usize> = HashMap::new();
    let mut receipt = PlanReceipt::default();
    let mut aborted = false;

    for (index, step) in plan.steps.iter().enumerate() {
        if aborted {
            // 中止后剩余步骤全部记为 skipped(保留顺序,便于报告)。
            receipt.results.push(StepResult {
                index,
                tool: step.tool.clone(),
                text: String::new(),
                is_error: false,
                skipped: true,
            });
            receipt.skipped += 1;
            continue;
        }
        let resolved = match resolve_args(&step.args, index, &results, &aliases) {
            Ok(v) => v,
            Err(e) => {
                // 引用解析失败:记为失败步骤,按 on_error 决定是否中止。
                receipt.results.push(StepResult {
                    index,
                    tool: step.tool.clone(),
                    text: format!("{e}"),
                    is_error: true,
                    skipped: false,
                });
                receipt.any_error = true;
                receipt.executed += 1;
                if plan.on_error == OnError::Abort {
                    aborted = true;
                }
                continue;
            }
        };

        let (text, is_error) = execute(step.tool.clone(), resolved).await;
        let result = StepResult {
            index,
            tool: step.tool.clone(),
            text,
            is_error,
            skipped: false,
        };
        receipt.executed += 1;
        if is_error {
            receipt.any_error = true;
            if plan.on_error == OnError::Abort {
                aborted = true;
            }
        }
        if let Some(bind) = &step.bind {
            aliases.insert(bind.clone(), index);
        }
        results.insert(index, result.clone());
        receipt.results.push(result);
    }

    Ok(receipt)
}

#[cfg(test)]
mod executor_tests {
    use super::*;

    #[tokio::test]
    async fn executes_in_order_and_chains_results() {
        let plan = CallPlan::new(vec![
            PlanStep {
                tool: "echo".into(),
                args: serde_json::json!({"text": "hello"}),
                bind: Some("first".into()),
            },
            PlanStep {
                tool: "echo".into(),
                args: serde_json::json!({"text": "$first.result"}),
                bind: None,
            },
        ]);
        let receipt = execute_plan(&plan, |_tool, args| async move {
            (args["text"].as_str().unwrap_or("").to_string(), false)
        })
        .await
        .expect("execute");
        assert_eq!(receipt.executed, 2);
        assert!(!receipt.any_error);
        assert_eq!(
            receipt.results[1].text, "hello",
            "第二步入参应解析为第一步结果"
        );
    }

    #[tokio::test]
    async fn abort_stops_after_first_error() {
        let plan = CallPlan::new(vec![
            PlanStep {
                tool: "fail".into(),
                args: serde_json::json!({}),
                bind: None,
            },
            PlanStep {
                tool: "echo".into(),
                args: serde_json::json!({"text": "never"}),
                bind: None,
            },
        ]);
        let receipt = execute_plan(&plan, |tool, _args| async move {
            if tool == "fail" {
                ("boom".to_string(), true)
            } else {
                ("ran".to_string(), false)
            }
        })
        .await
        .expect("execute");
        assert_eq!(receipt.executed, 1);
        assert_eq!(receipt.skipped, 1);
        assert!(receipt.any_error);
        assert!(receipt.results[1].skipped, "后续步骤应被跳过");
    }

    #[tokio::test]
    async fn continue_mode_keeps_going() {
        let mut plan = CallPlan::new(vec![
            PlanStep {
                tool: "fail".into(),
                args: serde_json::json!({}),
                bind: None,
            },
            PlanStep {
                tool: "echo".into(),
                args: serde_json::json!({"text": "after"}),
                bind: None,
            },
        ]);
        plan.on_error = OnError::Continue;
        let receipt = execute_plan(&plan, |tool, _args| async move {
            if tool == "fail" {
                ("boom".to_string(), true)
            } else {
                ("ran".to_string(), false)
            }
        })
        .await
        .expect("execute");
        assert_eq!(receipt.executed, 2);
        assert_eq!(receipt.skipped, 0);
        assert_eq!(receipt.results[1].text, "ran");
    }

    #[tokio::test]
    async fn unresolved_reference_fails_step() {
        let plan = CallPlan::new(vec![PlanStep {
            tool: "echo".into(),
            args: serde_json::json!({"text": "$9.result"}),
            bind: None,
        }]);
        let receipt = execute_plan(&plan, |_t, _a| async { ("x".to_string(), false) })
            .await
            .expect("execute");
        assert!(receipt.results[0].is_error);
        assert!(receipt.any_error);
    }
}
