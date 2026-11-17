//! 从标准输入接收评测用例及实际输出,输出确定性评分报告。

use std::collections::BTreeMap;
use std::io::{self, Read};

use serde::Deserialize;
use zhuye_eval::{EvalCase, EvalRunner};

#[derive(Deserialize)]
struct Input {
    cases: Vec<EvalCase>,
    outputs: BTreeMap<String, (String, Vec<String>)>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let input: Input = serde_json::from_str(&input)?;
    let report = EvalRunner::deterministic_only().run_set(&input.cases, &input.outputs)?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}
