//! EventMsg 全变体线格式契约测试。
//!
//! 样本表是每个已完成事件变体的最低测试要求:新增变体时先补样本,
//! 本测试会同时校验 Rust enum ↔ 样本表奇偶、反序列化和 roundtrip。

use std::collections::BTreeSet;

use serde_json::{Value, json};
use zhuye_protocol::EventMsg;

const EVENT_ENUM: &str = include_str!("../src/event_msg/mod.rs");

/// 按 serde 的 `rename_all = "snake_case"` 规则转换 Rust 标识符。
fn serde_snake_case(name: &str) -> String {
    let mut out = String::new();
    for (index, ch) in name.chars().enumerate() {
        let prev = if index == 0 {
            None
        } else {
            name.chars().nth(index - 1)
        };
        let next = name.chars().nth(index + 1);
        let boundary = index > 0
            && ch.is_uppercase()
            && (prev.is_some_and(|p| p.is_lowercase() || p.is_numeric())
                || (prev.is_some_and(|p| p.is_uppercase())
                    && next.is_some_and(|n| n.is_lowercase())));
        if boundary {
            out.push('_');
        }
        out.extend(ch.to_lowercase());
    }
    out
}

/// 从协议源码提取 EventMsg 变体,保证测试以实现为唯一真源。
fn rust_event_types() -> BTreeSet<String> {
    let begin = EVENT_ENUM
        .find("pub enum EventMsg")
        .expect("找不到 EventMsg enum");
    let body = &EVENT_ENUM[begin..];
    let end = body.find("\n}").expect("找不到 EventMsg enum 结束边界");
    let body = &body[..end];
    let mut out = BTreeSet::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with(char::is_uppercase) {
            continue;
        }
        let variant = trimmed
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect::<String>();
        if !variant.is_empty() {
            out.insert(serde_snake_case(&variant));
        }
    }
    out
}

fn load_samples() -> serde_json::Map<String, Value> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../tests/contracts/events.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("读取事件契约失败:{}{error}", path.display()));
    serde_json::from_str::<Value>(&raw)
        .expect("events.json 必须是合法 JSON")
        .get("events")
        .and_then(Value::as_object)
        .expect("events.json 缺少 events 对象")
        .clone()
}

#[test]
fn event_contract_covers_every_rust_variant() {
    let rust = rust_event_types();
    let samples = load_samples();
    let sample_types: BTreeSet<_> = samples.keys().cloned().collect();
    let mut missing: Vec<_> = rust.difference(&sample_types).collect();
    let mut stale: Vec<_> = sample_types.difference(&rust).collect();
    missing.sort();
    stale.sort();
    assert!(
        missing.is_empty() && stale.is_empty(),
        "EventMsg 契约漂移:缺少 {:?},多出 {:?}",
        missing,
        stale
    );
    assert!(
        !rust.is_empty() && rust.len() >= 50,
        "EventMsg 变体解析结果异常:{} 个",
        rust.len()
    );
}

#[test]
fn every_event_sample_round_trips_through_serde() {
    let samples = load_samples();
    let mut failures = Vec::new();
    for (expected, sample) in &samples {
        assert_eq!(
            sample.get("type").and_then(Value::as_str),
            Some(expected.as_str()),
            "样本 {expected} 的 type 字段与键不一致"
        );
        let result = serde_json::from_value::<EventMsg>(sample.clone())
            .and_then(|event| serde_json::to_value(&event));
        match result {
            Ok(wire) => {
                let actual = wire.get("type").and_then(Value::as_str);
                if actual != Some(expected.as_str()) {
                    failures.push(format!("{expected}:roundtrip 后 type 变成 {actual:?}"));
                }
            }
            Err(error) => failures.push(format!("{expected}:{error};样本={sample}")),
        }
    }
    assert!(
        failures.is_empty(),
        "以下事件契约样本未通过真协议 serde roundtrip:\n{}",
        failures.join("\n")
    );
}

#[test]
fn event_serde_contract_detector_is_strict() {
    assert!(
        serde_json::from_value::<EventMsg>(json!({"type":"not_a_real_event_v1"})).is_err(),
        "未知事件 type 不能被接受,否则全变体契约测试会空转"
    );

    let mut samples = load_samples();
    let sample = samples
        .get_mut("turn_complete")
        .expect("turn_complete 样本必须存在");
    sample
        .get_mut("usage")
        .and_then(Value::as_object_mut)
        .expect("turn_complete.usage 必须是对象")
        .remove("input_tokens");
    assert!(
        serde_json::from_value::<EventMsg>(sample.clone()).is_err(),
        "缺失必填字段的事件样本必须被拒绝"
    );
}
